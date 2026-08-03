//! `warden report compare` — this period against the one immediately before it.
//!
//! The previous period is the same length, ending where this one starts: a
//! `--since 7d` run compares week against week, `--since 1d` day against day.
//! That requires a bounded period, so an unbounded run is an error with the fix
//! in it rather than a comparison against nothing.

use chrono::{SecondsFormat, TimeZone, Utc};

use crate::cli::TimeWindow;
use crate::output::{Cell, Report, Table};
use crate::store::Scanner;

use super::{count, round_money, scan, Cost, ReportCtx, ReportError, Totals};

pub fn build(scanner: &Scanner, ctx: &ReportCtx) -> Result<Report, ReportError> {
    let length = ctx
        .window
        .to_ms
        .checked_sub(ctx.window.from_ms)
        .filter(|len| *len > 0)
        .ok_or(ReportError::NeedsWindow("compare"))?;
    // `TimeWindow::all` uses saturating sentinels; there is no period before it.
    let previous_from = ctx
        .window
        .from_ms
        .checked_sub(length)
        .ok_or(ReportError::NeedsWindow("compare"))?;
    let previous_window = TimeWindow::new(previous_from, ctx.window.from_ms);

    let current = scan(scanner, ctx)?;
    let previous = scan(scanner, &ctx.with_window(previous_window))?;

    let now = fold(&current.events);
    let before = fold(&previous.events);

    let mut table = Table::new(["metric", "this period", "previous", "delta", "change"]);
    let mut rows = Vec::new();

    for metric in METRICS {
        let (this, prev) = ((metric.value)(&now), (metric.value)(&before));
        let cell = |value: Option<f64>| match (metric.kind, value) {
            (Kind::Count, Some(v)) => count(v as u64),
            (Kind::Money, Some(v)) => Cell::money_est(v),
            (_, None) => Cell::Unsupported,
        };

        let delta = match (this, prev) {
            (Some(a), Some(b)) => Some(a - b),
            _ => None,
        };
        let change = match (this, prev) {
            (Some(a), Some(b)) if b != 0.0 => Some((a - b) / b * 100.0),
            _ => None,
        };

        table.push(vec![
            Cell::text(metric.label),
            cell(this),
            cell(prev),
            match (delta, metric.kind) {
                (Some(d), Kind::Money) => Cell::money_est(d),
                (Some(d), Kind::Count) => Cell::Int(d as i64),
                (None, _) => Cell::Unsupported,
            },
            match change {
                Some(pct) => Cell::text(format!("{pct:+.1}%")),
                None => Cell::Unsupported,
            },
        ]);

        let number = |value: Option<f64>| match (metric.kind, value) {
            (Kind::Count, Some(v)) => serde_json::json!(v as i64),
            (Kind::Money, Some(v)) => serde_json::json!(round_money(v)),
            (_, None) => serde_json::Value::Null,
        };
        rows.push(serde_json::json!({
            "metric": metric.label,
            "this_period": number(this),
            "previous_period": number(prev),
            "delta": number(delta),
            "change_pct": change.map(|pct| (pct * 10.0).round() / 10.0),
        }));
    }

    let mut notes = current.notes;
    notes.merge(&previous.notes);
    notes.push(format!(
        "previous period is the {} immediately before this one, ending where it begins ({} to {})",
        super::format_span(length),
        iso(previous_window.from_ms),
        iso(previous_window.to_ms),
    ));
    notes.push(
        "change is blank where the previous period had nothing to divide by, and where a figure \
         could not be derived in both periods",
    );

    Ok(Report::new("compare", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

fn iso(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unbounded".to_string())
}

fn fold(events: &[crate::store::Event]) -> Totals {
    let mut totals = Totals::default();
    for event in events {
        totals.add(event);
    }
    totals
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Count,
    Money,
}

struct Metric {
    label: &'static str,
    kind: Kind,
    /// `None` means "not derivable", which renders `–` rather than `0`.
    value: fn(&Totals) -> Option<f64>,
}

/// Cost is `None` when nothing in the period could be priced, so an unpriced
/// model never reads as "spend went to zero".
const METRICS: &[Metric] = &[
    Metric {
        label: "sessions",
        kind: Kind::Count,
        value: |t| Some(t.sessions.len() as f64),
    },
    Metric {
        label: "requests",
        kind: Kind::Count,
        value: |t| Some(t.requests as f64),
    },
    Metric {
        label: "input tok",
        kind: Kind::Count,
        value: |t| Some(t.input as f64),
    },
    Metric {
        label: "output tok",
        kind: Kind::Count,
        value: |t| Some(t.output as f64),
    },
    Metric {
        label: "cache read tok",
        kind: Kind::Count,
        value: |t| Some(t.cache_read as f64),
    },
    Metric {
        label: "cache write tok",
        kind: Kind::Count,
        value: |t| Some(t.cache_write as f64),
    },
    Metric {
        label: "total tok",
        kind: Kind::Count,
        value: |t| Some(t.total_tokens() as f64),
    },
    Metric {
        label: "est. cost",
        kind: Kind::Money,
        value: |t| priced_total(t.cost),
    },
];

fn priced_total(cost: Cost) -> Option<f64> {
    (cost.priced > 0).then_some(cost.total)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::output::{Style, UNSUPPORTED};

    fn window() -> TimeWindow {
        TimeWindow::new(ms(2026, 8, 4, 0), ms(2026, 8, 5, 0))
    }

    fn report(events: &[crate::store::Event]) -> Report {
        let (_dir, paths) = store(events);
        build(&Scanner::new(paths), &ReportCtx::new(window(), None, true)).unwrap()
    }

    fn two_days() -> Vec<crate::store::Event> {
        vec![
            priced(used("y", ms(2026, 8, 3, 10), "acme", "m", 100, 10), 1.0),
            priced(used("t1", ms(2026, 8, 4, 10), "acme", "m", 150, 10), 2.0),
        ]
    }

    #[test]
    fn compares_day_against_the_day_before() {
        let report = report(&two_days());
        let input = row(&report, "input tok");
        assert_eq!(input["this_period"], 150);
        assert_eq!(input["previous_period"], 100);
        assert_eq!(input["delta"], 50);
        assert_eq!(input["change_pct"], 50.0);

        let cost = row(&report, "est. cost");
        assert_eq!(cost["this_period"], 2.0);
        assert_eq!(cost["delta"], 1.0);
        assert!(report.table.render(Style::plain()).contains("+50.0%"));
    }

    fn row<'a>(report: &'a Report, metric: &str) -> &'a serde_json::Value {
        report
            .json_rows
            .iter()
            .find(|row| row["metric"] == metric)
            .expect("metric present")
    }

    #[test]
    fn an_unbounded_period_is_an_error_that_names_the_fix() {
        let (_dir, paths) = store(&two_days());
        let err = build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--since"), "{msg}");
    }

    #[test]
    fn an_empty_previous_period_blanks_the_change_instead_of_dividing_by_zero() {
        let report = report(&[priced(
            used("t", ms(2026, 8, 4, 10), "acme", "m", 150, 10),
            2.0,
        )]);
        let input = row(&report, "input tok");
        assert_eq!(input["previous_period"], 0);
        assert!(input["change_pct"].is_null());
        assert!(report.table.render(Style::plain()).contains(UNSUPPORTED));
    }

    #[test]
    fn unpriced_cost_never_reads_as_spend_dropping_to_zero() {
        let report = report(&[used("t", ms(2026, 8, 4, 10), "acme", "m", 150, 10)]);
        let cost = row(&report, "est. cost");
        assert!(cost["this_period"].is_null());
        assert!(cost["delta"].is_null());
    }
}
