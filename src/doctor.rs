//! `warden doctor` — "why is this number empty?".
//!
//! Every blank column has exactly one of three causes, and doctor names all
//! three: the adapter is not implemented, the adapter cannot populate that KPI
//! from its logs, or the model has no configured price.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use crate::adapters::{self, Capabilities};
use crate::cli::TimeWindow;
use crate::config::{Config, Pricing};
use crate::store::{expand_tilde, Partition, ScanQuery, Scanner, StorePaths};

/// Whether a source is there to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Found,
    NotFound,
    NotImplemented,
}

/// One adapter's health.
#[derive(Debug, Clone)]
pub struct AdapterHealth {
    pub name: String,
    pub status: SourceStatus,
    /// Where warden looked, when it knows.
    pub root: Option<PathBuf>,
    pub sessions: usize,
    pub capabilities: Capabilities,
}

/// Store-side stats, plus the reasons a cost column might be blank.
#[derive(Debug, Clone, Default)]
pub struct StoreHealth {
    pub partitions: usize,
    pub bytes: u64,
    pub oldest: Option<Partition>,
    pub events: u64,
    /// Models seen with token counts but no configured price. Their cost is
    /// blank because pricing is user-editable config, not a built-in table.
    pub unpriced_models: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub adapters: Vec<AdapterHealth>,
    pub store: StoreHealth,
}

/// Inspect every adapter and the store.
pub fn run(
    config: &Config,
    paths: &StorePaths,
    window: TimeWindow,
    project: Option<&str>,
) -> io::Result<DoctorReport> {
    let mut health = Vec::new();
    for adapter in adapters::registry() {
        let root = adapter.root(config).map(|r| expand_tilde(&r)).transpose()?;
        let present = root.as_deref().is_some_and(|root| root.is_dir());
        let status = match (adapter.is_implemented(), present) {
            (false, _) => SourceStatus::NotImplemented,
            (true, true) => SourceStatus::Found,
            (true, false) => SourceStatus::NotFound,
        };
        let sessions = match (status, root.as_deref()) {
            (SourceStatus::Found, Some(root)) => adapter.session_count(root)?,
            _ => 0,
        };
        health.push(AdapterHealth {
            name: adapter.name().to_string(),
            status,
            root,
            sessions,
            capabilities: adapter.capabilities(),
        });
    }

    Ok(DoctorReport {
        adapters: health,
        store: store_health(paths, window, project, &config.pricing())?,
    })
}

fn store_health(
    paths: &StorePaths,
    window: TimeWindow,
    project: Option<&str>,
    pricing: &Pricing,
) -> io::Result<StoreHealth> {
    let scanner = Scanner::new(paths.clone());
    let partitions = scanner.partitions_for(window)?;

    let mut store = StoreHealth {
        partitions: partitions.len(),
        oldest: partitions.first().map(|(partition, _)| *partition),
        ..StoreHealth::default()
    };
    for (_, path) in &partitions {
        store.bytes += std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    }

    let mut unpriced = BTreeSet::new();
    let query = ScanQuery::new(window).with_project(project.map(str::to_string));
    scanner.scan_with(&query, |event| {
        store.events += 1;
        // Priced from the config as it is now, exactly as a report would: a
        // rate added since ingest must stop doctor from calling it unpriced.
        // `has_usage` and not a narrower check, so a cache-only record counts
        // here exactly as it counts in a report.
        if event.has_usage() && crate::reports::event_cost(&event, pricing).is_none() {
            unpriced.insert(event.model.unwrap_or_else(|| "(unknown model)".into()));
        }
    })?;
    store.unpriced_models = unpriced.into_iter().collect();
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::Kpi;

    #[test]
    fn reports_found_missing_and_unimplemented_sources() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("projects");
        std::fs::create_dir_all(logs.join("proj")).unwrap();
        std::fs::write(logs.join("proj/s1.jsonl"), "").unwrap();
        std::fs::write(logs.join("proj/s2.jsonl"), "").unwrap();

        let mut config = Config::default();
        config.sources.insert(
            "claude-code".into(),
            crate::config::Source {
                enabled: true,
                path: Some(logs),
            },
        );
        let paths = StorePaths::new(dir.path().join("store"));
        let report = run(&config, &paths, TimeWindow::all(), None).unwrap();

        let claude = &report.adapters[0];
        assert_eq!(claude.status, SourceStatus::Found);
        assert_eq!(claude.sessions, 2);
        assert!(claude.capabilities.supports(Kpi::Tokens));
        // The answer to "why is the duration column empty?".
        assert!(claude.capabilities.unsupported().contains(&Kpi::DurationMs));

        assert_eq!(report.adapters[1].status, SourceStatus::NotImplemented);
        assert_eq!(report.store.partitions, 0);
    }

    #[test]
    fn missing_source_directory_is_not_found_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.sources.insert(
            "claude-code".into(),
            crate::config::Source {
                enabled: true,
                path: Some(dir.path().join("nope")),
            },
        );
        let report = run(
            &config,
            &StorePaths::new(dir.path()),
            TimeWindow::all(),
            None,
        )
        .unwrap();
        assert_eq!(report.adapters[0].status, SourceStatus::NotFound);
        assert_eq!(report.adapters[0].sessions, 0);
    }

    #[test]
    fn store_stats_name_the_unpriced_models() {
        use crate::store::{Event, StoreWriter};
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path().join("store"));
        let mut writer = StoreWriter::open(paths.clone()).unwrap();
        let mut event = Event::new(
            "a",
            1_785_924_000_000,
            "claude-code",
            "anthropic",
            "assistant",
        );
        event.model = Some("claude-sonnet-4-6".into());
        event.input_tok = Some(10);
        writer.append_event(&event).unwrap();

        let report = run(&Config::default(), &paths, TimeWindow::all(), None).unwrap();
        assert_eq!(report.store.partitions, 1);
        assert_eq!(report.store.events, 1);
        assert!(report.store.bytes > 0);
        assert_eq!(report.store.oldest, Some(Partition::new(2026, 8)));
        assert_eq!(report.store.unpriced_models, vec!["claude-sonnet-4-6"]);

        // Adding the rate to config re-prices the *existing* store: doctor
        // stops naming the model without anything being re-ingested.
        let config: Config = toml::from_str(
            r#"
[pricing.anthropic]
"claude-sonnet-4-6" = { input = 3.0, output = 15.0, cache_read = 0.3 }
"#,
        )
        .unwrap();
        let report = run(&config, &paths, TimeWindow::all(), None).unwrap();
        assert!(report.store.unpriced_models.is_empty());
        assert_eq!(report.store.events, 1, "nothing was re-ingested");
    }
}
