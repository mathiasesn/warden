//! `warden query` — a general rollup over named dimensions.
//!
//! This is the escape valve from the fixed report set: `--group-by
//! project,model` answers a question no named report does. It is deliberately
//! narrow — a closed list of dimensions, no filters beyond the global ones —
//! because a real query language would freeze the record shape — warden offers a
//! fixed rollup, not arbitrary queries.

use std::collections::BTreeMap;

use crate::output::{Cell, Report, Table};
use crate::store::Event;
use crate::store::Scanner;

use super::{by_weight_desc, count, day_of, scan, ReportCtx, ReportError, Totals};

/// The dimensions `--group-by` accepts, in the order the error lists them.
pub const DIMENSIONS: [&str; 7] = [
    "project", "model", "agent", "provider", "day", "session", "role",
];

/// Used when `--group-by` is omitted: the rollup people mean by default.
pub const DEFAULT_GROUP_BY: &str = "project";

/// Shown in a table cell when an event does not carry this dimension. The JSON
/// keeps `null`, so a consumer never has to parse this string.
const ABSENT: &str = "(none)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimension(&'static str);

impl Dimension {
    pub fn name(self) -> &'static str {
        self.0
    }

    fn value(self, event: &Event) -> Option<String> {
        match self.0 {
            "project" => event.project.clone(),
            "model" => event.model.clone(),
            "agent" => Some(event.agent.clone()),
            "provider" => Some(event.provider.clone()),
            "day" => day_of(event.ts),
            "session" => event.session_id.clone(),
            "role" => Some(event.role.clone()),
            _ => None,
        }
    }
}

/// Parse a comma-separated `--group-by`, rejecting anything not in
/// [`DIMENSIONS`] and de-duplicating repeats.
pub fn parse_dimensions(spec: &str) -> Result<Vec<Dimension>, ReportError> {
    let mut dims: Vec<Dimension> = Vec::new();
    for raw in spec.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let known = DIMENSIONS
            .iter()
            .find(|dim| **dim == name)
            .ok_or_else(|| ReportError::UnknownDimension(name.to_string()))?;
        let dim = Dimension(known);
        if !dims.contains(&dim) {
            dims.push(dim);
        }
    }
    if dims.is_empty() {
        return Err(ReportError::UnknownDimension(spec.trim().to_string()));
    }
    Ok(dims)
}

pub fn build(
    scanner: &Scanner,
    ctx: &ReportCtx,
    dims: &[Dimension],
) -> Result<Report, ReportError> {
    let scanned = scan(scanner, ctx)?;

    let mut buckets: BTreeMap<Vec<Option<String>>, Totals> = BTreeMap::new();
    for event in &scanned.events {
        let key: Vec<Option<String>> = dims.iter().map(|dim| dim.value(event)).collect();
        buckets.entry(key).or_default().add(event, &ctx.pricing);
    }

    let mut headers: Vec<String> = dims.iter().map(|dim| dim.name().to_string()).collect();
    headers.extend(
        ["requests", "sessions", "in", "out", "cache r", "est. cost"]
            .into_iter()
            .map(String::from),
    );
    let mut table = Table::new(headers);
    let mut rows = Vec::new();

    for (key, totals) in by_weight_desc(buckets) {
        let mut row: Vec<Cell> = key
            .iter()
            .map(|value| Cell::text(value.clone().unwrap_or_else(|| ABSENT.to_string())))
            .collect();
        row.push(count(totals.requests));
        row.push(count(totals.sessions.len() as u64));
        row.extend(totals.tail_cells());
        table.push(row);

        let mut json = serde_json::Map::new();
        for (dim, value) in dims.iter().zip(&key) {
            json.insert(dim.name().to_string(), serde_json::json!(value));
        }
        json.insert("sessions".into(), serde_json::json!(totals.sessions.len()));
        totals.write_json(&mut json);
        rows.push(serde_json::Value::Object(json));
    }

    let mut notes = scanned.notes;
    notes.push(format!(
        "grouped by {}; a dimension an event does not carry is {ABSENT} in the table and null in \
         --json",
        dims.iter()
            .map(|dim| dim.name())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    Ok(Report::new("query", ctx.window, table)
        .with_json_rows(rows)
        .with_notes(notes.finish()))
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::cli::TimeWindow;
    use crate::output::Style;

    fn report(spec: &str) -> Report {
        let mut sonnet = used("c", ms(2026, 8, 4, 9), "acme", "sonnet", 10, 1);
        sonnet.session_id = Some("s2".into());
        let (_dir, paths) = store(&[
            priced(
                used("a", ms(2026, 8, 4, 8), "acme", "opus", 1_000, 100),
                5.0,
            ),
            priced(sonnet, 0.1),
            priced(used("d", ms(2026, 8, 5, 9), "dotfiles", "opus", 20, 2), 0.2),
        ]);
        build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
            &parse_dimensions(spec).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn groups_by_several_dimensions_at_once() {
        let report = report("project,model");
        assert_eq!(report.json_rows.len(), 3);
        let first = &report.json_rows[0];
        assert_eq!(first["project"], "acme");
        assert_eq!(first["model"], "opus");
        assert_eq!(first["input_tok"], 1_000);
        assert_eq!(first["cost_est"], 5.0);

        let rendered = report.table.render(Style::plain());
        assert!(rendered.starts_with("PROJECT   MODEL"), "{rendered}");
    }

    #[test]
    fn supports_every_documented_dimension() {
        for dim in DIMENSIONS {
            let report = report(dim);
            assert!(!report.json_rows.is_empty(), "{dim}");
            assert!(report.json_rows[0].get(dim).is_some(), "{dim}");
        }
    }

    #[test]
    fn rejects_an_unknown_dimension_and_lists_the_real_ones() {
        let err = parse_dimensions("project,colour").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown --group-by dimension \"colour\""),
            "{msg}"
        );
        for dim in DIMENSIONS {
            assert!(msg.contains(dim), "{msg} is missing {dim}");
        }
        assert!(parse_dimensions("").is_err());
        assert!(parse_dimensions(" , ").is_err());
    }

    #[test]
    fn repeated_dimensions_collapse() {
        let dims = parse_dimensions("project, model ,project").unwrap();
        assert_eq!(
            dims.iter().map(|d| d.name()).collect::<Vec<_>>(),
            ["project", "model"]
        );
    }

    #[test]
    fn a_dimension_an_event_lacks_is_null_in_json() {
        let mut orphan = used("x", ms(2026, 8, 4, 8), "acme", "m", 5, 1);
        orphan.project = None;
        let (_dir, paths) = store(&[orphan]);
        let report = build(
            &Scanner::new(paths),
            &ReportCtx::new(TimeWindow::all(), None, true),
            &parse_dimensions("project").unwrap(),
        )
        .unwrap();
        assert!(report.json_rows[0]["project"].is_null());
        assert!(report.table.render(Style::plain()).contains(ABSENT));
    }
}
