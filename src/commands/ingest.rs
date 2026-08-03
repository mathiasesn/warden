//! `warden ingest` (MVP §3).

use std::io::{self, Write};

use super::{thousands, Env};
use crate::cli::TimeWindow;
use crate::config::Config;
use crate::ingest::{self, AdapterIngest, IngestOptions, IngestReport};
use crate::output::{emit, Report};
use crate::store::StorePaths;

/// Ingest every enabled source, then report what it did.
///
/// Goes out through [`emit`] like every other command, so `--json` is the
/// versioned envelope and nothing else (MVP §5).
pub fn run(env: &Env<'_>) -> io::Result<IngestReport> {
    let report = run_quiet(env.config, env.paths, env.window, env.project)?;
    emit(&to_report(&report, env.window), env.json)?;
    Ok(report)
}

/// Ingest without printing anything.
///
/// The implicit pre-report ingest uses this so it can put its progress on
/// stderr instead: a report's stdout is the report, and under `--json` it must
/// stay one parseable document.
pub fn run_quiet(
    config: &Config,
    paths: &StorePaths,
    window: TimeWindow,
    project: Option<&str>,
) -> io::Result<IngestReport> {
    let options = IngestOptions {
        window,
        project: project.map(str::to_string),
    };
    ingest::run(config, paths, &options)
}

/// The progress lines, against any writer.
pub fn write_lines<W: Write>(out: &mut W, report: &IngestReport) -> io::Result<()> {
    write!(out, "{}", text(report))
}

/// One row per adapter, plus the cursor note — the shape MVP §3 prints.
fn text(report: &IngestReport) -> String {
    let mut out = String::new();
    for adapter in &report.adapters {
        out.push_str(&format!(
            "{:<13} {} files   {} new events",
            adapter.adapter,
            thousands(adapter.files_read as u64),
            thousands(adapter.new_events),
        ));
        if adapter.skipped_unparseable > 0 {
            out.push_str(&format!(
                "   {} skipped (unparseable)",
                thousands(adapter.skipped_unparseable)
            ));
        }
        if adapter.unreadable_files > 0 {
            out.push_str(&format!(
                "   {} skipped (unreadable file)",
                thousands(adapter.unreadable_files)
            ));
        }
        out.push('\n');
    }
    if report.cursors_withheld {
        out.push_str("note: --project filters what is stored, so no cursors were advanced\n");
    }
    out
}

fn to_report(report: &IngestReport, window: TimeWindow) -> Report {
    let rows: Vec<serde_json::Value> = report
        .adapters
        .iter()
        .map(|adapter| row(adapter, !report.cursors_withheld))
        .collect();

    let mut notes = Vec::new();
    if report.cursors_withheld {
        notes.push(
            "--project filters what is stored, so no cursors were advanced: a cursor must only \
             ever mean \"this file is fully ingested\""
                .to_string(),
        );
    }
    notes.push(
        "ingest is idempotent — event ids are content-derived, so re-running it appends nothing \
         twice"
            .to_string(),
    );

    Report::prose("ingest", window, text(report))
        .with_json_rows(rows)
        .with_notes(notes)
}

fn row(adapter: &AdapterIngest, cursors_written: bool) -> serde_json::Value {
    serde_json::json!({
        "adapter": adapter.adapter,
        "root": adapter.root.as_ref().map(|root| root.display().to_string()),
        "files_seen": adapter.files_seen,
        "files_read": adapter.files_read,
        "new_events": adapter.new_events,
        "new_prompts": adapter.new_prompts,
        "skipped_unparseable": adapter.skipped_unparseable,
        "unreadable_files": adapter.unreadable_files,
        "duplicates": adapter.duplicates,
        // False when the run was filtered: the next run re-reads from the old
        // offset, and a consumer should not treat this run as a full ingest.
        "cursors_written": cursors_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{write_report, Style};

    fn sample() -> IngestReport {
        IngestReport {
            adapters: vec![AdapterIngest {
                adapter: "claude-code".into(),
                root: Some("/logs".into()),
                files_read: 14,
                files_seen: 20,
                new_events: 1_203,
                new_prompts: 40,
                skipped_unparseable: 2,
                unreadable_files: 0,
                duplicates: 0,
            }],
            cursors_withheld: false,
        }
    }

    fn render(report: &IngestReport, json: bool) -> String {
        let mut buf = Vec::new();
        write_report(
            &mut buf,
            &to_report(report, TimeWindow::all()),
            json,
            Style::plain(),
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn the_table_output_is_the_documented_line() {
        assert_eq!(
            render(&sample(), false),
            "claude-code   14 files   1,203 new events   2 skipped (unparseable)\n"
        );
    }

    #[test]
    fn json_emits_the_envelope_and_nothing_else() {
        let out = render(&sample(), true);
        let v: serde_json::Value = serde_json::from_str(&out).expect("a single parseable document");
        assert_eq!(v["report"], "ingest");
        assert_eq!(v["record_version"], crate::store::RECORD_VERSION);

        let row = &v["rows"][0];
        assert_eq!(row["adapter"], "claude-code");
        assert_eq!(row["files_read"], 14);
        assert_eq!(row["files_seen"], 20);
        assert_eq!(row["new_events"], 1_203);
        assert_eq!(row["skipped_unparseable"], 2);
        assert_eq!(row["cursors_written"], true);

        assert!(!out.contains("new events"), "no table alongside the JSON");
    }

    #[test]
    fn a_filtered_run_says_no_cursors_were_written() {
        let mut report = sample();
        report.cursors_withheld = true;

        assert!(render(&report, false).contains("no cursors were advanced"));

        let v: serde_json::Value = serde_json::from_str(&render(&report, true)).unwrap();
        assert_eq!(v["rows"][0]["cursors_written"], false);
        assert!(v["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("no cursors were advanced")));
    }
}
