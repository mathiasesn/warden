//! `warden report models` — usage split by model.

use crate::output::{Cell, Report, Table};
use crate::store::Scanner;

use super::{by_weight_desc, count, rollup, scan, ReportCtx, ReportError};

const UNKNOWN: &str = "(no model)";

pub fn build(scanner: &Scanner, ctx: &ReportCtx) -> Result<Report, ReportError> {
    let scanned = scan(scanner, ctx)?;
    // Only records that carry usage identify a model in a meaningful way; a
    // user prompt has no model and would otherwise invent a `(no model)` row
    // the size of the transcript.
    let by_model = rollup(&scanned.events, &ctx.pricing, |event| {
        super::has_usage(event).then(|| event.model.clone().unwrap_or_else(|| UNKNOWN.to_string()))
    });

    let mut table = Table::new([
        "model",
        "requests",
        "sessions",
        "in",
        "out",
        "cache r",
        "est. cost",
    ]);
    let mut rows = Vec::new();

    for (model, totals) in by_weight_desc(by_model) {
        let mut row = vec![
            Cell::text(&model),
            count(totals.requests),
            count(totals.sessions.len() as u64),
        ];
        row.extend(totals.tail_cells());
        table.push(row);

        let mut json = serde_json::Map::new();
        json.insert(
            "model".into(),
            if model == UNKNOWN {
                serde_json::Value::Null
            } else {
                serde_json::json!(model)
            },
        );
        json.insert("sessions".into(), serde_json::json!(totals.sessions.len()));
        totals.write_json(&mut json);
        rows.push(serde_json::Value::Object(json));
    }

    let mut notes = scanned.notes;
    notes.push(
        "rows cover records that carry usage; prompts and tool results name no model and are \
         excluded here rather than pooled into a fictitious row",
    );
    Ok(Report::new("models", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::cli::TimeWindow;
    use crate::output::Style;
    use crate::reports::SYNTHETIC_MODEL;
    use crate::store::Event;

    fn report() -> Report {
        let prompt = Event::new("p", ms(2026, 8, 4, 8), "claude-code", "anthropic", "user");
        let (_dir, paths) = store(&[
            priced(
                used("a", ms(2026, 8, 4, 8), "acme", "opus", 1_000, 100),
                5.0,
            ),
            priced(used("b", ms(2026, 8, 4, 9), "acme", "sonnet", 10, 1), 0.1),
            priced(
                used("c", ms(2026, 8, 4, 9), "acme", SYNTHETIC_MODEL, 5, 1),
                9.0,
            ),
            prompt,
        ]);
        build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap()
    }

    #[test]
    fn splits_usage_by_model_heaviest_first() {
        let report = report();
        let models: Vec<&str> = report
            .json_rows
            .iter()
            .map(|row| row["model"].as_str().unwrap())
            .collect();
        assert_eq!(models, ["opus", "sonnet", SYNTHETIC_MODEL]);
        assert_eq!(report.json_rows[0]["requests"], 1);
    }

    #[test]
    fn prompts_do_not_become_a_no_model_row() {
        assert_eq!(report().json_rows.len(), 3);
    }

    #[test]
    fn synthetic_shows_its_tokens_but_no_cost() {
        let report = report();
        let synthetic = &report.json_rows[2];
        assert_eq!(synthetic["input_tok"], 5);
        assert!(
            synthetic["cost_est"].is_null(),
            "nobody is billed for {SYNTHETIC_MODEL}"
        );
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains(SYNTHETIC_MODEL) && n.contains("cost is not")));
        assert!(report
            .table
            .render(Style::plain())
            .contains(SYNTHETIC_MODEL));
    }
}
