//! `warden report projects` — tokens and est. cost per project (MVP §3).

use crate::output::{Cell, Report, Table};
use crate::store::Scanner;

use super::{by_weight_desc, count, rollup, scan, ReportCtx, ReportError};

/// Events with no project are still real spend, so they get a row rather than
/// being dropped.
const UNATTRIBUTED: &str = "(no project)";

pub fn build(scanner: &Scanner, ctx: &ReportCtx) -> Result<Report, ReportError> {
    let scanned = scan(scanner, ctx)?;
    let by_project = rollup(&scanned.events, |event| {
        Some(
            event
                .project
                .clone()
                .unwrap_or_else(|| UNATTRIBUTED.to_string()),
        )
    });

    let mut table = Table::new(["project", "sessions", "in", "out", "cache r", "est. cost"]);
    let mut rows = Vec::new();
    let mut unattributed = false;

    for (project, totals) in by_weight_desc(by_project) {
        unattributed |= project == UNATTRIBUTED;
        let mut row = vec![Cell::text(&project), count(totals.sessions.len() as u64)];
        row.extend(totals.tail_cells());
        table.push(row);

        let mut json = serde_json::Map::new();
        json.insert(
            "project".into(),
            if project == UNATTRIBUTED {
                serde_json::Value::Null
            } else {
                serde_json::json!(project)
            },
        );
        json.insert("sessions".into(), serde_json::json!(totals.sessions.len()));
        totals.write_json(&mut json);
        rows.push(serde_json::Value::Object(json));
    }

    let mut notes = scanned.notes;
    if unattributed {
        notes.push(format!(
            "{UNATTRIBUTED}: events whose source log recorded no working directory; they are real \
             spend and are kept rather than dropped, and their JSON `project` is null"
        ));
    }
    Ok(Report::new("projects", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::cli::TimeWindow;
    use crate::output::Style;

    fn report(project_filter: Option<&str>) -> Report {
        let mut orphan = used("d", ms(2026, 8, 4, 9), "x", "m", 7, 1);
        orphan.project = None;
        let (_dir, paths) = store(&[
            priced(used("a", ms(2026, 8, 4, 8), "acme", "m", 1_000, 100), 5.0),
            priced(used("b", ms(2026, 8, 4, 9), "acme", "m", 500, 50), 2.5),
            priced(used("c", ms(2026, 8, 4, 9), "dotfiles", "m", 10, 1), 0.1),
            orphan,
        ]);
        build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), project_filter.map(str::to_string), true),
        )
        .unwrap()
    }

    #[test]
    fn heaviest_project_first_with_the_documented_columns() {
        let rendered = report(None).table.render(Style::plain());
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0], "PROJECT       SESSIONS    IN  OUT  CACHE R  EST. COST",
            "{rendered}"
        );
        assert!(lines[1].starts_with("acme"), "{rendered}");
        assert!(lines[2].starts_with("dotfiles"), "{rendered}");
        assert!(rendered.contains("~ estimated"));
    }

    #[test]
    fn honours_the_project_filter() {
        let report = report(Some("dotfiles"));
        assert_eq!(report.json_rows.len(), 1);
        assert_eq!(report.json_rows[0]["project"], "dotfiles");
    }

    #[test]
    fn unattributed_events_are_kept_with_a_null_project_and_a_note() {
        let report = report(None);
        let orphan = report
            .json_rows
            .iter()
            .find(|row| row["project"].is_null())
            .expect("the projectless event still has a row");
        assert_eq!(orphan["input_tok"], 7);
        assert!(report.notes.iter().any(|n| n.contains("(no project)")));
    }

    #[test]
    fn unpriced_projects_show_a_dash_rather_than_zero() {
        let (_dir, paths) = store(&[used("a", ms(2026, 8, 4, 8), "acme", "claude-opus-5", 10, 1)]);
        let report = build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
        )
        .unwrap();
        let rendered = report.table.render(Style::plain());
        assert!(rendered.contains(crate::output::UNSUPPORTED), "{rendered}");
        assert!(!rendered.contains("$0.00"), "{rendered}");
        assert!(report.json_rows[0]["cost_est"].is_null());
    }
}
