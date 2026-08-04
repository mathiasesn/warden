//! The shared presentation layer: one table renderer, one JSON envelope.
//!
//! Every report goes out through [`emit`], so no command branches on `--json`
//! itself and the two surfaces cannot drift apart. A report's job is to build a
//! [`Report`]; how it reaches a terminal or a harness is this module's job.

pub mod envelope;
pub mod table;

pub use envelope::{iso8601_ms, Envelope, Period};
pub use table::{format_count, format_money, Cell, Style, Table, UNSUPPORTED};

use std::io::{self, Write};

use crate::cli::TimeWindow;

/// One report, rendered either way.
///
/// `rows` (the table) and `json_rows` (the envelope) are kept separate on
/// purpose: the table is formatted for a human, the JSON stays raw so a harness
/// can do its own arithmetic on it.
#[derive(Debug, Clone)]
pub struct Report {
    /// The stable report name, e.g. `"projects"` (MVP §3).
    pub name: String,
    pub window: TimeWindow,
    pub table: Table,
    pub json_rows: Vec<serde_json::Value>,
    pub notes: Vec<String>,
    /// Pre-rendered human form, for commands whose output is prose rather than
    /// a table (`ingest`, `doctor`). When set it replaces the table in non-JSON
    /// mode; the envelope is unaffected either way.
    pub text: Option<String>,
}

impl Report {
    pub fn new(name: impl Into<String>, window: TimeWindow, table: Table) -> Self {
        Self {
            name: name.into(),
            window,
            table,
            json_rows: Vec::new(),
            notes: Vec::new(),
            text: None,
        }
    }

    /// A report whose human form is prose. It still goes out through [`emit`],
    /// so `--json` remains a single parseable document with no table alongside.
    pub fn prose(name: impl Into<String>, window: TimeWindow, text: String) -> Self {
        Self {
            text: Some(text),
            ..Self::new(name, window, Table::new(Vec::<String>::new()))
        }
    }

    pub fn with_json_rows(mut self, rows: Vec<serde_json::Value>) -> Self {
        self.json_rows = rows;
        self
    }

    pub fn with_notes<S: Into<String>>(mut self, notes: impl IntoIterator<Item = S>) -> Self {
        self.notes = notes.into_iter().map(Into::into).collect();
        self
    }

    /// The `--json` form, without printing it.
    pub fn envelope(&self) -> Envelope {
        Envelope::new(self.name.clone(), self.window, self.json_rows.clone())
            .with_notes(self.notes.clone())
    }
}

/// Print a report to stdout: the envelope when `json`, otherwise the table.
///
/// ANSI escapes are only ever emitted for the table on a TTY.
pub fn emit(report: &Report, json: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let style = if json { Style::plain() } else { Style::auto() };
    let mut lock = stdout.lock();
    write_report(&mut lock, report, json, style)?;
    lock.flush()
}

/// [`emit`] against an arbitrary writer, for tests and for callers that capture
/// output. Never emits escape codes unless `style` allows them.
pub fn write_report<W: Write>(
    out: &mut W,
    report: &Report,
    json: bool,
    style: Style,
) -> io::Result<()> {
    if json {
        let body = serde_json::to_string_pretty(&report.envelope())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(out, "{body}")
    } else if let Some(text) = &report.text {
        write!(out, "{text}")
    } else {
        write!(out, "{}", report.table.render(style))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        let table = Table::new(["project", "sessions", "est. cost"]).with_row(vec![
            Cell::text("acme-api"),
            Cell::Int(41),
            Cell::money_est(12.40),
        ]);
        Report::new("projects", TimeWindow::new(0, 86_400_000), table)
            .with_json_rows(vec![
                serde_json::json!({"project": "acme-api", "cost_est": 12.4}),
            ])
            .with_notes(["cost figures are estimates"])
    }

    fn render(json: bool) -> String {
        let mut buf = Vec::new();
        write_report(&mut buf, &report(), json, Style::plain()).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn json_mode_emits_the_envelope_and_no_table() {
        let out = render(true);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["report"], "projects");
        assert_eq!(v["rows"][0]["project"], "acme-api");
        assert!(!out.contains("PROJECT"));
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn a_prose_report_prints_its_text_but_still_emits_the_envelope() {
        let report = Report::prose(
            "ingest",
            TimeWindow::all(),
            "claude-code   1 files\n".into(),
        )
        .with_json_rows(vec![serde_json::json!({"adapter": "claude-code"})]);

        let mut human = Vec::new();
        write_report(&mut human, &report, false, Style::plain()).unwrap();
        assert_eq!(String::from_utf8(human).unwrap(), "claude-code   1 files\n");

        let mut json = Vec::new();
        write_report(&mut json, &report, true, Style::plain()).unwrap();
        let out = String::from_utf8(json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["report"], "ingest");
        assert_eq!(v["rows"][0]["adapter"], "claude-code");
        assert!(!out.contains("claude-code   1 files"), "no prose in --json");
    }

    #[test]
    fn table_mode_emits_the_table_and_no_json() {
        let out = render(false);
        assert!(out.starts_with("PROJECT"));
        assert!(out.contains("$12.40 ~"));
        assert!(out.trim_end().ends_with("~ estimated"));
        assert!(!out.contains("warden_version"));
    }
}
