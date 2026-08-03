//! `warden report files` — *attributed* tokens per file (MVP §4).
//!
//! No log records "this file consumed N tokens". This report derives it:
//!
//! 1. read `tool_calls` on each event for file paths,
//! 2. assign that event's token cost across the distinct files it touched,
//! 3. split evenly.
//!
//! That is a heuristic and it is labelled as one everywhere it surfaces: the
//! columns say `attrib.`, the notes say `attributed, not measured`, and every
//! JSON row carries `"method": "even-split"` so a consumer can tell without
//! reading this file (MVP §4).

use std::collections::BTreeMap;

use crate::config::Pricing;
use crate::output::{Cell, Report, Table};
use crate::store::Event;
use crate::store::Scanner;

use super::{count, round_money, scan, ReportCtx, ReportError, SYNTHETIC_MODEL};

/// The documented attribution rule, published in the rows and the notes.
pub const METHOD: &str = "even-split";

/// Files are long-tailed; the table shows the heaviest and says so.
const LIMIT: usize = 25;

/// A share of a turn's tokens, hence fractional.
#[derive(Debug, Clone, Copy, Default)]
struct Attributed {
    events: u64,
    tool_calls: u64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    cost: f64,
    priced_events: u64,
    /// Events with usage this file was attributed a share of, whose model has
    /// no configured price. Their spend is missing from `cost`.
    unpriced_events: u64,
}

impl Attributed {
    fn add(&mut self, event: &Event, share: f64, calls: u64, pricing: &Pricing) {
        self.events += 1;
        self.tool_calls += calls;
        self.input += event.input_tok.unwrap_or(0) as f64 * share;
        self.output += event.output_tok.unwrap_or(0) as f64 * share;
        self.cache_read += event.cache_read_tok.unwrap_or(0) as f64 * share;
        self.cache_write += event.cache_write_tok.unwrap_or(0) as f64 * share;
        if event.model.as_deref() != Some(SYNTHETIC_MODEL) && super::has_usage(event) {
            match super::event_cost(event, pricing) {
                Some(cost) => {
                    self.cost += cost * share;
                    self.priced_events += 1;
                }
                None => self.unpriced_events += 1,
            }
        }
    }

    fn total(&self) -> f64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn is_partial(&self) -> bool {
        self.priced_events > 0 && self.unpriced_events > 0
    }

    fn cost_cell(&self) -> Cell {
        if self.priced_events == 0 {
            Cell::Unsupported
        } else if self.is_partial() {
            Cell::money_partial(self.cost)
        } else {
            Cell::money_est(self.cost)
        }
    }

    fn cost_json(&self) -> serde_json::Value {
        if self.priced_events == 0 {
            serde_json::Value::Null
        } else {
            serde_json::json!(round_money(self.cost))
        }
    }
}

pub fn build(scanner: &Scanner, ctx: &ReportCtx) -> Result<Report, ReportError> {
    let scanned = scan(scanner, ctx)?;

    let mut by_file: BTreeMap<String, Attributed> = BTreeMap::new();
    let mut attributed_events = 0u64;
    let mut unattributed = super::Totals::default();

    for event in &scanned.events {
        // Distinct paths: two Edits of the same file are one file, not two
        // shares, or a turn that edits one file twice would halve its own cost.
        let mut calls: BTreeMap<&str, u64> = BTreeMap::new();
        for call in &event.tool_calls {
            if let Some(path) = file_target(call.tool_target.as_deref()) {
                *calls.entry(path).or_default() += 1;
            }
        }
        if calls.is_empty() {
            if super::has_usage(event) {
                unattributed.add(event, &ctx.pricing);
            }
            continue;
        }
        attributed_events += 1;
        let share = 1.0 / calls.len() as f64;
        for (path, calls_here) in calls {
            by_file.entry(path.to_string()).or_default().add(
                event,
                share,
                calls_here,
                &ctx.pricing,
            );
        }
    }

    let total_files = by_file.len();
    let mut ranked: Vec<(String, Attributed)> = by_file.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.total()
            .partial_cmp(&a.1.total())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut table = Table::new([
        "file",
        "turns",
        "calls",
        "attrib. in",
        "attrib. out",
        "attrib. cache r",
        "attrib. cost",
    ]);
    let mut rows = Vec::new();

    for (path, attributed) in ranked.iter().take(LIMIT) {
        table.push(vec![
            Cell::text(path),
            count(attributed.events),
            count(attributed.tool_calls),
            count(attributed.input.round() as u64),
            count(attributed.output.round() as u64),
            count(attributed.cache_read.round() as u64),
            attributed.cost_cell(),
        ]);
        rows.push(serde_json::json!({
            "file": path,
            "method": METHOD,
            "attributed": true,
            "turns": attributed.events,
            "tool_calls": attributed.tool_calls,
            "attributed_input_tok": round2(attributed.input),
            "attributed_output_tok": round2(attributed.output),
            "attributed_cache_read_tok": round2(attributed.cache_read),
            "attributed_cache_write_tok": round2(attributed.cache_write),
            "attributed_cost_est": attributed.cost_json(),
            "cost_partial": attributed.is_partial(),
            "cost_priced_events": attributed.priced_events,
            "cost_unpriced_events": attributed.unpriced_events,
        }));
    }

    let mut notes = scanned.notes;
    notes.push(format!(
        "attributed, not measured: no log records what a file cost. Each event's tokens are split \
         evenly across the distinct files its tool calls touched (method: {METHOD}); --json carries \
         \"method\": \"{METHOD}\" on every row"
    ));
    notes.push(format!(
        "{attributed_events} events touched a file and were attributed; {} events carried usage \
         but touched no file (a plain answer, a shell command) and are in no row here — attributed \
         totals therefore sum to less than the period's",
        unattributed.requests
    ));
    if total_files > LIMIT {
        notes.push(format!(
            "showing the {LIMIT} heaviest of {total_files} files, in the table and in --json alike"
        ));
    }

    Ok(Report::new("files", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

/// A tool target that names a file. URLs are targets too (`WebFetch`), and a
/// fetched page is not a file in the repo, so they are excluded.
fn file_target(target: Option<&str>) -> Option<&str> {
    let target = target?.trim();
    if target.is_empty() || target.starts_with("http://") || target.starts_with("https://") {
        return None;
    }
    Some(target)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::cli::TimeWindow;
    use crate::output::Style;

    fn report(events: &[Event]) -> Report {
        let (_dir, paths) = store(events);
        build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap()
    }

    fn row<'a>(report: &'a Report, file: &str) -> &'a serde_json::Value {
        report
            .json_rows
            .iter()
            .find(|row| row["file"] == file)
            .unwrap_or_else(|| panic!("no row for {file}"))
    }

    #[test]
    fn a_turn_splits_evenly_across_the_files_it_touched() {
        let report = report(&[with_tools(
            priced(used("a", ms(2026, 8, 4, 8), "acme", "m", 100, 20), 1.0),
            &[("Read", "src/a.rs"), ("Edit", "src/b.rs")],
        )]);
        for file in ["src/a.rs", "src/b.rs"] {
            let row = row(&report, file);
            assert_eq!(row["attributed_input_tok"], 50.0, "{file}");
            assert_eq!(row["attributed_output_tok"], 10.0, "{file}");
            assert_eq!(row["attributed_cost_est"], 0.5, "{file}");
            assert_eq!(row["method"], METHOD);
        }
    }

    #[test]
    fn two_calls_on_one_file_are_one_share_not_two() {
        let report = report(&[with_tools(
            used("a", ms(2026, 8, 4, 8), "acme", "m", 100, 20),
            &[("Read", "src/a.rs"), ("Edit", "src/a.rs")],
        )]);
        let row = row(&report, "src/a.rs");
        assert_eq!(row["attributed_input_tok"], 100.0, "the whole turn, once");
        assert_eq!(row["tool_calls"], 2);
        assert_eq!(row["turns"], 1);
        assert_eq!(report.json_rows.len(), 1);
    }

    #[test]
    fn the_method_is_labelled_in_the_table_and_the_notes() {
        let report = report(&[with_tools(
            used("a", ms(2026, 8, 4, 8), "acme", "m", 100, 20),
            &[("Read", "src/a.rs")],
        )]);
        let rendered = report.table.render(Style::plain());
        assert!(rendered.contains("ATTRIB. IN"), "{rendered}");
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("attributed, not measured") && n.contains(METHOD)),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn events_that_touched_no_file_are_excluded_and_the_shortfall_is_stated() {
        let report = report(&[
            with_tools(
                used("a", ms(2026, 8, 4, 8), "acme", "m", 100, 20),
                &[("Read", "src/a.rs")],
            ),
            used("b", ms(2026, 8, 4, 9), "acme", "m", 900, 20),
        ]);
        assert_eq!(report.json_rows.len(), 1);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("1 events carried usage but touched no file")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn urls_are_not_files() {
        let report = report(&[with_tools(
            used("a", ms(2026, 8, 4, 8), "acme", "m", 100, 20),
            &[("WebFetch", "https://example.com/x"), ("Read", "src/a.rs")],
        )]);
        assert_eq!(report.json_rows.len(), 1);
        assert_eq!(
            row(&report, "src/a.rs")["attributed_input_tok"],
            100.0,
            "the fetched page takes no share"
        );
    }

    #[test]
    fn an_unpriced_file_shows_a_dash_not_zero() {
        let report = report(&[with_tools(
            used("a", ms(2026, 8, 4, 8), "acme", "claude-opus-5", 100, 20),
            &[("Read", "src/a.rs")],
        )]);
        assert!(row(&report, "src/a.rs")["attributed_cost_est"].is_null());
        let rendered = report.table.render(Style::plain());
        assert!(rendered.contains(crate::output::UNSUPPORTED), "{rendered}");
        assert!(!rendered.contains("$0.00"), "{rendered}");
    }
}
