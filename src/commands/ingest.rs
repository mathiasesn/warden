//! `warden ingest` (MVP §3).

use std::io;

use super::thousands;
use crate::cli::TimeWindow;
use crate::config::Config;
use crate::ingest::{self, IngestOptions, IngestReport};
use crate::store::StorePaths;

/// Ingest every enabled source, then print one line per adapter.
pub fn run(
    config: &Config,
    paths: &StorePaths,
    window: TimeWindow,
    project: Option<&str>,
) -> io::Result<IngestReport> {
    let options = IngestOptions {
        window,
        project: project.map(str::to_string),
    };
    let report = ingest::run(config, paths, &options)?;
    print(&report);
    Ok(report)
}

fn print(report: &IngestReport) {
    for adapter in &report.adapters {
        let mut line = format!(
            "{:<13} {} files   {} new events",
            adapter.adapter,
            thousands(adapter.files_read as u64),
            thousands(adapter.new_events),
        );
        if adapter.skipped_unparseable > 0 {
            line.push_str(&format!(
                "   {} skipped (unparseable)",
                thousands(adapter.skipped_unparseable)
            ));
        }
        println!("{line}");
    }
    if report.cursors_withheld {
        println!("note: --project filters what is stored, so no cursors were advanced");
    }
}
