//! `warden report sessions` — the longest and most expensive sessions.
//!
//! Claude Code logs no per-turn duration, so "longest" is answered with a
//! *span*: the wall-clock gap between a session's first and last event. That is
//! derived from timestamps warden does have, and it is named `span` rather than
//! `duration` so it cannot be mistaken for measured model time. The `duration`
//! column stays next to it and greys out, which is the honest answer to "how
//! long did the model actually work?" for a source that never wrote it down.

use std::collections::BTreeMap;

use crate::adapters::Kpi;
use crate::output::{Cell, Report, Table};
use crate::store::Scanner;

use super::{count, format_span, rollup, scan, short_id, ReportCtx, ReportError, Totals};

/// Sessions are long-tailed; the table shows the head and says so.
const LIMIT: usize = 20;
const NO_PROJECT: &str = "(no project)";

pub fn build(scanner: &Scanner, ctx: &ReportCtx) -> Result<Report, ReportError> {
    let scanned = scan(scanner, ctx)?;
    let by_session = rollup(&scanned.events, &ctx.pricing, |event| {
        event.session_id.clone()
    });
    let total_sessions = by_session.len();

    // Per-session facts that do not live on `Totals`.
    let mut projects: BTreeMap<String, String> = BTreeMap::new();
    let mut durations: BTreeMap<String, Option<u64>> = BTreeMap::new();
    for event in &scanned.events {
        let Some(session) = event.session_id.clone() else {
            continue;
        };
        if let Some(project) = &event.project {
            projects.entry(session.clone()).or_insert(project.clone());
        }
        if let Some(duration) = event.duration_ms {
            *durations.entry(session).or_default().get_or_insert(0) += duration;
        }
    }

    // Most expensive first when anything could be priced, heaviest first
    // otherwise — ranking by a column that is entirely `–` would be a lie.
    let priced_anywhere = by_session.values().any(|totals| totals.cost.priced > 0);
    let mut ranked: Vec<(String, Totals)> = by_session.into_iter().collect();
    ranked.sort_by(|a, b| {
        let key = |t: &Totals| {
            if priced_anywhere {
                t.cost.total
            } else {
                t.total_tokens() as f64
            }
        };
        key(&b.1)
            .partial_cmp(&key(&a.1))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.total_tokens().cmp(&a.1.total_tokens()))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut table = Table::new([
        "session",
        "project",
        "span",
        "duration",
        "requests",
        "in",
        "out",
        "cache r",
        "est. cost",
    ]);
    let mut rows = Vec::new();

    for (session, totals) in ranked.iter().take(LIMIT) {
        let span = span_ms(totals);
        let duration = durations.get(session).copied().flatten();
        let project = projects.get(session).cloned();

        let mut row = vec![
            Cell::text(short_id(session)),
            Cell::text(project.clone().unwrap_or_else(|| NO_PROJECT.to_string())),
            Cell::text(format_span(span)),
            match duration {
                Some(ms) => Cell::text(format_span(ms as i64)),
                None => Cell::Unsupported,
            },
            count(totals.requests),
        ];
        row.extend(totals.tail_cells());
        table.push(row);

        let mut json = serde_json::Map::new();
        json.insert("session_id".into(), serde_json::json!(session));
        json.insert("project".into(), serde_json::json!(project));
        json.insert("first_ts".into(), serde_json::json!(totals.first_ts));
        json.insert("last_ts".into(), serde_json::json!(totals.last_ts));
        json.insert("span_ms".into(), serde_json::json!(span));
        json.insert("duration_ms".into(), serde_json::json!(duration));
        totals.write_json(&mut json);
        rows.push(serde_json::Value::Object(json));
    }

    let mut notes = scanned.notes;
    notes.push(
        "span is the wall clock between a session's first and last event, including time you spent \
         reading; it is not measured model time",
    );
    if rows.iter().all(|row| row["duration_ms"].is_null()) {
        notes.push(format!(
            "duration is blank because no source in this period logs per-turn duration ({}); blank \
             means unrecorded, not zero",
            adapters_without_duration()
        ));
    }
    notes.push(if priced_anywhere {
        "ordered by est. cost, highest first"
    } else {
        "ordered by total tokens, highest first, because no session in this period could be priced"
    });
    if total_sessions > LIMIT {
        notes.push(format!(
            "showing the top {LIMIT} of {total_sessions} sessions, in the table and in --json alike"
        ));
    }

    Ok(Report::new("sessions", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

fn span_ms(totals: &Totals) -> i64 {
    match (totals.first_ts, totals.last_ts) {
        (Some(first), Some(last)) => last - first,
        _ => 0,
    }
}

/// Named so the note points at the adapter, not at warden.
fn adapters_without_duration() -> String {
    crate::adapters::registry()
        .iter()
        .filter(|adapter| {
            adapter.is_implemented() && !adapter.capabilities().supports(Kpi::DurationMs)
        })
        .map(|adapter| adapter.name())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::cli::TimeWindow;
    use crate::output::{Style, UNSUPPORTED};
    use crate::store::Event;

    fn report(events: &[Event]) -> Report {
        let (_dir, paths) = store(events);
        build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap()
    }

    fn cheap_and_dear() -> Vec<Event> {
        vec![
            priced(used("a", ms(2026, 8, 4, 8), "acme", "m", 10, 1), 0.1),
            priced(used("b", ms(2026, 8, 4, 10), "acme", "m", 10, 1), 0.1),
            priced(used("c", ms(2026, 8, 4, 9), "dotfiles", "m", 5_000, 1), 9.0),
        ]
    }

    #[test]
    fn most_expensive_session_first() {
        let report = report(&cheap_and_dear());
        assert_eq!(report.json_rows[0]["session_id"], "session-dotfiles");
        assert_eq!(report.json_rows[0]["cost_est"], 9.0);
        assert_eq!(report.json_rows[0]["project"], "dotfiles");
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("ordered by est. cost")));
    }

    #[test]
    fn falls_back_to_tokens_when_nothing_can_be_priced() {
        let report = report(&[
            used("a", ms(2026, 8, 4, 8), "acme", "m", 10, 1),
            used("c", ms(2026, 8, 4, 9), "dotfiles", "m", 5_000, 1),
        ]);
        assert_eq!(report.json_rows[0]["session_id"], "session-dotfiles");
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("ordered by total tokens")));
    }

    #[test]
    fn span_is_derived_and_unlogged_duration_stays_unsupported() {
        let report = report(&cheap_and_dear());
        let acme = report
            .json_rows
            .iter()
            .find(|row| row["session_id"] == "session-acme")
            .unwrap();
        assert_eq!(acme["span_ms"], 2 * 3_600_000);
        assert!(acme["duration_ms"].is_null());

        let rendered = report.table.render(Style::plain());
        assert!(rendered.contains("2h00m"), "{rendered}");
        assert!(rendered.contains(UNSUPPORTED), "{rendered}");
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("blank means unrecorded, not zero")));
        assert!(report.notes.iter().any(|n| n.contains("claude-code")));
    }

    #[test]
    fn duration_is_shown_when_a_source_does_log_it() {
        let mut timed = used("a", ms(2026, 8, 4, 8), "acme", "m", 10, 1);
        timed.duration_ms = Some(8_140);
        let report = report(&[timed]);
        assert_eq!(report.json_rows[0]["duration_ms"], 8_140);
        assert!(report.table.render(Style::plain()).contains("8s"));
    }
}
