//! `warden report summary` — totals for the period, broken down by day.

use crate::output::{Cell, Report, Table};
use crate::store::Scanner;

use super::{count, day_of, rollup, scan, ReportCtx, ReportError, Totals};

pub fn build(scanner: &Scanner, ctx: &ReportCtx) -> Result<Report, ReportError> {
    let scanned = scan(scanner, ctx)?;
    let by_day = rollup(&scanned.events, &ctx.pricing, |event| day_of(event.ts));

    let mut table = Table::new([
        "day",
        "sessions",
        "requests",
        "in",
        "out",
        "cache r",
        "est. cost",
    ]);
    let mut rows = Vec::new();
    let mut grand = Totals::default();

    // Chronological, not heaviest-first: a summary is read as a timeline.
    for (day, totals) in &by_day {
        let mut row = vec![
            Cell::text(day),
            count(totals.sessions.len() as u64),
            count(totals.requests),
        ];
        row.extend(totals.tail_cells());
        table.push(row);

        let mut json = serde_json::Map::new();
        json.insert("day".into(), serde_json::json!(day));
        json.insert("sessions".into(), serde_json::json!(totals.sessions.len()));
        totals.write_json(&mut json);
        rows.push(serde_json::Value::Object(json));

        grand.merge(totals);
    }

    if !by_day.is_empty() {
        let mut total = vec![
            Cell::text("total"),
            count(grand.sessions.len() as u64),
            count(grand.requests),
        ];
        total.extend(grand.tail_cells());
        table.push(total);
    }

    let mut notes = scanned.notes;
    notes.push("the final row totals the period; daily session counts overlap when a session spans midnight, so they sum to more than the period total");
    Ok(Report::new("summary", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::cli::TimeWindow;

    fn report() -> Report {
        let (_dir, paths) = store(&[
            priced(used("a", ms(2026, 8, 3, 10), "acme", "m", 100, 20), 1.0),
            priced(used("b", ms(2026, 8, 3, 11), "acme", "m", 50, 10), 0.5),
            priced(used("c", ms(2026, 8, 4, 10), "acme", "m", 10, 2), 0.25),
        ]);
        build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap()
    }

    #[test]
    fn breaks_the_period_down_by_day_in_order() {
        let rendered = report().table.render(crate::output::Style::plain());
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[0].starts_with("DAY"));
        assert!(lines[1].starts_with("2026-08-03"), "{rendered}");
        assert!(lines[2].starts_with("2026-08-04"), "{rendered}");
        assert!(lines[3].starts_with("total"), "{rendered}");
    }

    #[test]
    fn the_total_row_sums_the_days() {
        let report = report();
        assert_eq!(report.json_rows.len(), 2);
        assert_eq!(report.json_rows[0]["input_tok"], 150);
        assert_eq!(report.json_rows[1]["input_tok"], 10);
        assert_eq!(report.json_rows[0]["cost_est"], 1.5);
        assert!(report
            .table
            .render(crate::output::Style::plain())
            .contains("$1.75 ~"));
    }
}
