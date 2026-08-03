//! `warden report tools` — tool call frequency, and the failure rate warden
//! deliberately refuses to invent.
//!
//! The ingested event carries the `tool_use` blocks a model emitted; it does not
//! carry the tool *result*, so whether a call succeeded is not in the store.
//! A `0%` failure rate would be a fabrication, so the column renders `–` and a
//! note says why (MVP §3: "Reports grey out unsupported columns rather than
//! printing a misleading `0`").

use std::collections::BTreeMap;

use crate::output::{Cell, Report, Table};
use crate::store::Scanner;

use super::{count, scan, ReportCtx, ReportError};

#[derive(Default)]
struct ToolUse {
    calls: u64,
    events: u64,
    targets: std::collections::BTreeSet<String>,
}

pub fn build(scanner: &Scanner, ctx: &ReportCtx) -> Result<Report, ReportError> {
    let scanned = scan(scanner, ctx)?;

    let mut by_tool: BTreeMap<String, ToolUse> = BTreeMap::new();
    let mut total_calls = 0u64;
    for event in &scanned.events {
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for call in &event.tool_calls {
            total_calls += 1;
            let entry = by_tool.entry(call.tool_name.clone()).or_default();
            entry.calls += 1;
            if let Some(target) = &call.tool_target {
                entry.targets.insert(target.clone());
            }
            if seen.insert(call.tool_name.as_str()) {
                entry.events += 1;
            }
        }
    }

    let mut ranked: Vec<(String, ToolUse)> = by_tool.into_iter().collect();
    ranked.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then_with(|| a.0.cmp(&b.0)));

    let mut table = Table::new(["tool", "calls", "share", "turns", "targets", "fail rate"]);
    let mut rows = Vec::new();

    for (tool, use_) in &ranked {
        let share = if total_calls == 0 {
            0.0
        } else {
            use_.calls as f64 * 100.0 / total_calls as f64
        };
        table.push(vec![
            Cell::text(tool),
            count(use_.calls),
            Cell::Float(share, 1),
            count(use_.events),
            count(use_.targets.len() as u64),
            // Not derivable from what is ingested. Never `0%`.
            Cell::Unsupported,
        ]);
        rows.push(serde_json::json!({
            "tool": tool,
            "calls": use_.calls,
            "share_pct": (share * 10.0).round() / 10.0,
            "turns": use_.events,
            "distinct_targets": use_.targets.len(),
            "failures": serde_json::Value::Null,
            "fail_rate": serde_json::Value::Null,
        }));
    }

    let mut notes = scanned.notes;
    notes.push(
        "fail rate is blank, not 0%: warden ingests the tool calls a model made, not the results \
         they returned, so success and failure are not in the store and would have to be invented",
    );
    notes.push(
        "share is the percentage of all tool calls in the period; turns counts the events that \
         used the tool at least once",
    );

    Ok(Report::new("tools", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::cli::TimeWindow;
    use crate::output::{Style, UNSUPPORTED};

    fn report() -> Report {
        let (_dir, paths) = store(&[
            with_tools(
                used("a", ms(2026, 8, 4, 8), "acme", "m", 10, 1),
                &[("Read", "src/lib.rs"), ("Read", "src/main.rs")],
            ),
            with_tools(
                used("b", ms(2026, 8, 4, 9), "acme", "m", 10, 1),
                &[("Edit", "src/lib.rs")],
            ),
        ]);
        build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap()
    }

    #[test]
    fn counts_calls_and_ranks_the_busiest_tool_first() {
        let report = report();
        assert_eq!(report.json_rows[0]["tool"], "Read");
        assert_eq!(report.json_rows[0]["calls"], 2);
        assert_eq!(report.json_rows[0]["turns"], 1, "two calls in one event");
        assert_eq!(report.json_rows[0]["distinct_targets"], 2);
        assert_eq!(report.json_rows[0]["share_pct"], 66.7);
        assert_eq!(report.json_rows[1]["tool"], "Edit");
    }

    #[test]
    fn failure_is_null_and_never_a_fabricated_zero() {
        let report = report();
        for row in &report.json_rows {
            assert!(row["fail_rate"].is_null());
            assert!(row["failures"].is_null());
        }
        let rendered = report.table.render(Style::plain());
        assert!(rendered.contains(UNSUPPORTED), "{rendered}");
        assert!(!rendered.contains("0.0%"), "{rendered}");
        assert!(!rendered.contains(" 0%"), "{rendered}");
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("fail rate is blank, not 0%")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn a_period_with_no_tool_calls_renders_an_empty_table_not_a_panic() {
        let (_dir, paths) = store(&[used("a", ms(2026, 8, 4, 8), "acme", "m", 10, 1)]);
        let report = build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap();
        assert!(report.table.is_empty());
        assert_eq!(report.table.render(Style::plain()).lines().count(), 1);
    }
}
