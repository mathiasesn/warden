//! `warden doctor` (MVP §3).

use std::io;

use super::thousands;
use crate::adapters::Kpi;
use crate::cli::TimeWindow;
use crate::config::Config;
use crate::doctor::{self, AdapterHealth, DoctorReport, SourceStatus};
use crate::store::StorePaths;

pub fn run(
    config: &Config,
    paths: &StorePaths,
    window: TimeWindow,
    project: Option<&str>,
) -> io::Result<DoctorReport> {
    let report = doctor::run(config, paths, window, project)?;
    print(&report);
    Ok(report)
}

fn print(report: &DoctorReport) {
    for adapter in &report.adapters {
        println!("{}", adapter_line(adapter));
    }

    let store = &report.store;
    println!(
        "{:<13} {} partitions  {}  {}",
        "store",
        store.partitions,
        human_bytes(store.bytes),
        match store.oldest {
            Some(partition) => format!("oldest {:04}-{:02}", partition.year, partition.month),
            None => "no events yet".to_string(),
        }
    );
    println!("{:<13} {} events", "", thousands(store.events));

    // Every blank column, explained.
    for adapter in &report.adapters {
        let unsupported = adapter.capabilities.unsupported();
        if adapter.status == SourceStatus::Found && !unsupported.is_empty() {
            println!(
                "note: {} does not log {} — those columns stay blank rather than showing 0",
                adapter.name,
                labels(&unsupported)
            );
        }
    }
    if !store.unpriced_models.is_empty() {
        println!(
            "note: no configured price for {} — est. cost is blank for them; add rates under \
             [pricing.anthropic] in config.toml",
            store.unpriced_models.join(", ")
        );
    }
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

    #[test]
    fn scales_byte_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(43_200_512), "41.2 MB");
    }
}
