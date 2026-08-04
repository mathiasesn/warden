//! `warden doctor`.

use std::fmt::Write as _;
use std::io;

use super::{thousands, Env};
use crate::adapters::Kpi;
use crate::cli::TimeWindow;
use crate::doctor::{self, AdapterHealth, DoctorReport, SourceStatus};
use crate::output::{emit, Report};

/// Inspect every adapter and the store, then say why each number is empty.
///
/// Goes out through [`emit`], so `--json` is the versioned envelope and nothing
/// else. The notes carry the explanations doctor is required to keep
/// surfacing; the store row carries the store stats.
pub fn run(env: &Env<'_>) -> io::Result<DoctorReport> {
    let report = doctor::run(env.config, env.paths, env.window, env.project)?;
    emit(&to_report(&report, env.window), env.json)?;
    Ok(report)
}

/// The human form, unchanged: one line per adapter, then the store.
fn text(report: &DoctorReport) -> String {
    let mut out = String::new();
    for adapter in &report.adapters {
        let _ = writeln!(out, "{}", adapter_line(adapter));
    }

    let store = &report.store;
    let _ = writeln!(
        out,
        "{:<13} {} partitions  {}  {}",
        "store",
        store.partitions,
        human_bytes(store.bytes),
        match store.oldest {
            Some(partition) => format!("oldest {:04}-{:02}", partition.year, partition.month),
            None => "no events yet".to_string(),
        }
    );
    let _ = writeln!(out, "{:<13} {} events", "", thousands(store.events));

    for note in notes(report) {
        let _ = writeln!(out, "note: {note}");
    }
    out
}

/// Every blank column, explained — the same list in both surfaces.
fn notes(report: &DoctorReport) -> Vec<String> {
    let mut notes = Vec::new();
    for adapter in &report.adapters {
        let unsupported = adapter.capabilities.unsupported();
        if adapter.status == SourceStatus::Found && !unsupported.is_empty() {
            notes.push(format!(
                "{} does not log {} — those columns stay blank rather than showing 0",
                adapter.name,
                labels(&unsupported)
            ));
        }
    }
    if !report.store.unpriced_models.is_empty() {
        notes.push(format!(
            "no configured price for {} — est. cost omits their spend; add rates under \
             [pricing.<provider>] in config.toml and rerun (pricing is applied when a report is \
             built, so no re-ingest is needed)",
            report.store.unpriced_models.join(", ")
        ));
    }
    notes
}

fn to_report(report: &DoctorReport, window: TimeWindow) -> Report {
    // Rows are discriminated by `kind` rather than split across envelope
    // fields: the envelope's shape is fixed.
    let mut rows: Vec<serde_json::Value> = report.adapters.iter().map(adapter_row).collect();
    let store = &report.store;
    rows.push(serde_json::json!({
        "kind": "store",
        "partitions": store.partitions,
        "bytes": store.bytes,
        "oldest": store.oldest.map(|p| format!("{:04}-{:02}", p.year, p.month)),
        "events": store.events,
        "unpriced_models": store.unpriced_models,
    }));

    Report::prose("doctor", window, text(report))
        .with_json_rows(rows)
        .with_notes(notes(report))
}

fn adapter_row(adapter: &AdapterHealth) -> serde_json::Value {
    let kpis = |supported: bool| -> Vec<&'static str> {
        Kpi::ALL
            .into_iter()
            .filter(|kpi| adapter.capabilities.supports(*kpi) == supported)
            .map(|kpi| kpi.label())
            .collect()
    };
    serde_json::json!({
        "kind": "adapter",
        "adapter": adapter.name,
        "status": match adapter.status {
            SourceStatus::Found => "found",
            SourceStatus::NotFound => "not-found",
            SourceStatus::NotImplemented => "not-implemented",
        },
        "path": adapter.root.as_ref().map(|root| root.display().to_string()),
        "sessions": adapter.sessions,
        "supported_kpis": kpis(true),
        "unsupported_kpis": kpis(false),
    })
}

fn adapter_line(adapter: &AdapterHealth) -> String {
    let root = adapter
        .root
        .as_deref()
        .map(|root| root.display().to_string())
        .unwrap_or_default();
    match adapter.status {
        SourceStatus::NotImplemented => {
            format!("{:<13} – adapter not implemented", adapter.name)
        }
        SourceStatus::NotFound => format!("{:<13} – not found  {root}", adapter.name),
        SourceStatus::Found => format!(
            "{:<13} ✓ found  {root}   {} sessions   {}",
            adapter.name,
            thousands(adapter.sessions as u64),
            kpi_flags(adapter),
        ),
    }
}

/// Supported KPIs are ticked; unsupported ones are struck through, so the
/// difference is visible at a glance and never reads as a zero.
fn kpi_flags(adapter: &AdapterHealth) -> String {
    Kpi::ALL
        .into_iter()
        .map(|kpi| {
            if adapter.capabilities.supports(kpi) {
                format!("{} ✓", kpi.label())
            } else {
                format!("{} –", kpi.label())
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn labels(kpis: &[Kpi]) -> String {
    kpis.iter()
        .map(|kpi| kpi.label())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Shared with `purge`, which reports the size of what it removed.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::Capabilities;
    use crate::doctor::StoreHealth;
    use crate::output::{write_report, Style};
    use crate::store::Partition;

    #[test]
    fn scales_byte_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(43_200_512), "41.2 MB");
    }

    fn sample() -> DoctorReport {
        DoctorReport {
            adapters: vec![
                AdapterHealth {
                    name: "claude-code".into(),
                    status: SourceStatus::Found,
                    root: Some("/home/me/.claude/projects".into()),
                    sessions: 14,
                    capabilities: crate::adapters::registry()[0].capabilities(),
                },
                AdapterHealth {
                    name: "cursor".into(),
                    status: SourceStatus::NotImplemented,
                    root: None,
                    sessions: 0,
                    capabilities: Capabilities::none(),
                },
            ],
            store: StoreHealth {
                partitions: 3,
                bytes: 43_200_512,
                oldest: Some(Partition::new(2026, 6)),
                events: 41_200,
                unpriced_models: vec!["claude-opus-5".into()],
            },
        }
    }

    fn render(json: bool) -> String {
        let mut buf = Vec::new();
        write_report(
            &mut buf,
            &to_report(&sample(), TimeWindow::all()),
            json,
            Style::plain(),
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn the_table_output_still_explains_every_empty_column() {
        let out = render(false);
        assert!(out.contains("claude-code   ✓ found"), "{out}");
        assert!(
            out.contains("cursor        – adapter not implemented"),
            "{out}"
        );
        assert!(
            out.contains("store         3 partitions  41.2 MB  oldest 2026-06"),
            "{out}"
        );
        assert!(out.contains("41,200 events"), "{out}");
        // doctor keeps naming both causes of a blank column.
        assert!(
            out.contains("note: claude-code does not log duration"),
            "{out}"
        );
        assert!(
            out.contains("note: no configured price for claude-opus-5"),
            "{out}"
        );
    }

    #[test]
    fn json_emits_the_envelope_and_nothing_else() {
        let out = render(true);
        let v: serde_json::Value = serde_json::from_str(&out).expect("a single parseable document");
        assert_eq!(v["report"], "doctor");

        let rows = v["rows"].as_array().unwrap();
        let claude = &rows[0];
        assert_eq!(claude["kind"], "adapter");
        assert_eq!(claude["adapter"], "claude-code");
        assert_eq!(claude["status"], "found");
        assert_eq!(claude["sessions"], 14);
        assert_eq!(claude["path"], "/home/me/.claude/projects");
        assert!(claude["supported_kpis"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("tokens")));
        assert!(!claude["unsupported_kpis"].as_array().unwrap().is_empty());

        assert_eq!(rows[1]["status"], "not-implemented");
        assert!(rows[1]["path"].is_null());

        let store = rows.last().unwrap();
        assert_eq!(store["kind"], "store");
        assert_eq!(store["partitions"], 3);
        assert_eq!(store["events"], 41_200);
        assert_eq!(store["oldest"], "2026-06");
        assert_eq!(store["unpriced_models"][0], "claude-opus-5");

        // The explanations live in `notes`, not in stray prose on stdout.
        let notes = v["notes"].as_array().unwrap();
        assert!(notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("no configured price")));
        assert!(notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("does not log")));
        assert!(!out.contains("✓ found"), "no table alongside the JSON");
    }
}
