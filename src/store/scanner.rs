//! The shared read path (MVP §8 step 4).
//!
//! Every report consumes this scanner and nothing else opens event files
//! directly — that is what keeps a derived cache addable later without touching
//! each report. The scanner opens only partitions overlapping the requested
//! window and skips a torn or unparseable line rather than failing the run.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

use crate::cli::TimeWindow;

use super::paths::{Partition, StorePaths};
use super::record::Event;

/// A filtered scan over the event store.
#[derive(Debug, Clone)]
pub struct ScanQuery {
    /// Only events whose `ts` falls in this window are yielded.
    pub window: TimeWindow,
    /// Optional exact-match project filter.
    pub project: Option<String>,
}

impl ScanQuery {
    pub fn new(window: TimeWindow) -> Self {
        Self {
            window,
            project: None,
        }
    }

    pub fn with_project(mut self, project: Option<String>) -> Self {
        self.project = project;
        self
    }

    fn matches(&self, event: &Event) -> bool {
        if !self.window.contains(event.ts) {
            return false;
        }
        match &self.project {
            Some(project) => event.project.as_deref() == Some(project.as_str()),
            None => true,
        }
    }
}

/// What a scan skipped, so callers can report it honestly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Partitions actually opened.
    pub partitions_read: usize,
    /// Lines read from those partitions.
    pub lines_read: u64,
    /// Lines that failed to parse (torn final line, or schema drift).
    pub lines_skipped: u64,
    /// Parsed events excluded by the window or project filter.
    pub events_filtered: u64,
}

/// Result of a scan: the matching events plus what was skipped.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub events: Vec<Event>,
    pub stats: ScanStats,
}

/// Reads events from the store.
#[derive(Debug, Clone)]
pub struct Scanner {
    paths: StorePaths,
}

impl Scanner {
    pub fn new(paths: StorePaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    /// Partitions present on disk that overlap the query window, in
    /// chronological order.
    pub fn partitions_for(&self, window: TimeWindow) -> io::Result<Vec<(Partition, PathBuf)>> {
        StorePaths::partitions_in(&self.paths.events_dir(), window)
    }

    /// Scan the store, calling `visit` for each matching event in partition
    /// order. Unparseable lines are counted and skipped.
    pub fn scan_with<F>(&self, query: &ScanQuery, mut visit: F) -> io::Result<ScanStats>
    where
        F: FnMut(Event),
    {
        let mut stats = ScanStats::default();
        for (_, path) in self.partitions_for(query.window)? {
            let file = match File::open(&path) {
                Ok(file) => file,
                // A partition can vanish between listing and opening.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            stats.partitions_read += 1;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                stats.lines_read += 1;
                match serde_json::from_str::<Event>(&line) {
                    Ok(event) if query.matches(&event) => visit(event),
                    Ok(_) => stats.events_filtered += 1,
                    Err(_) => stats.lines_skipped += 1,
                }
            }
        }
        Ok(stats)
    }

    /// Collect matching events into memory.
    pub fn scan(&self, query: &ScanQuery) -> io::Result<Scan> {
        let mut events = Vec::new();
        let stats = self.scan_with(query, |event| events.push(event))?;
        Ok(Scan { events, stats })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::record::Event;
    use crate::store::writer::StoreWriter;
    use chrono::{TimeZone, Utc};

    fn ms(y: i32, mo: u32, d: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, 12, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    fn event(id: &str, ts: i64, project: &str) -> Event {
        let mut event = Event::new(id, ts, "claude-code", "anthropic", "assistant");
        event.project = Some(project.to_string());
        event
    }

    fn store_with_three_months() -> (tempfile::TempDir, StorePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let mut writer = StoreWriter::open(paths.clone()).unwrap();
        writer
            .append_event(&event("jun", ms(2026, 6, 15), "acme"))
            .unwrap();
        writer
            .append_event(&event("jul", ms(2026, 7, 15), "acme"))
            .unwrap();
        writer
            .append_event(&event("aug", ms(2026, 8, 15), "warden"))
            .unwrap();
        (dir, paths)
    }

    #[test]
    fn opens_only_overlapping_partitions() {
        let (_dir, paths) = store_with_three_months();
        let scanner = Scanner::new(paths);
        let window = TimeWindow::new(ms(2026, 7, 10), ms(2026, 8, 20));

        let partitions: Vec<_> = scanner
            .partitions_for(window)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(
            partitions,
            vec![Partition::new(2026, 7), Partition::new(2026, 8)]
        );

        let scan = scanner.scan(&ScanQuery::new(window)).unwrap();
        assert_eq!(scan.stats.partitions_read, 2);
        let ids: Vec<_> = scan.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["jul", "aug"]);
    }

    #[test]
    fn filters_by_window_within_a_partition() {
        let (_dir, paths) = store_with_three_months();
        let scanner = Scanner::new(paths);
        // Covers all of July but starts after the 15th, so nothing matches.
        let window = TimeWindow::new(ms(2026, 7, 20), ms(2026, 7, 25));
        let scan = scanner.scan(&ScanQuery::new(window)).unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.stats.partitions_read, 1);
        assert_eq!(scan.stats.events_filtered, 1);
    }

    #[test]
    fn filters_by_project() {
        let (_dir, paths) = store_with_three_months();
        let scanner = Scanner::new(paths);
        let query = ScanQuery::new(TimeWindow::all()).with_project(Some("acme".into()));
        let scan = scanner.scan(&query).unwrap();
        let ids: Vec<_> = scan.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["jun", "jul"]);
        assert_eq!(scan.stats.events_filtered, 1);
    }

    #[test]
    fn skips_torn_final_line_and_counts_it() {
        let (_dir, paths) = store_with_three_months();
        let partition = paths.event_partition(Partition::new(2026, 8));
        let mut text = std::fs::read_to_string(&partition).unwrap();
        // A reader that hits a half-written record must skip it (MVP §5).
        text.push_str("{\"v\":1,\"id\":\"torn\",\"ts\":17543000");
        std::fs::write(&partition, text).unwrap();

        let scan = Scanner::new(paths)
            .scan(&ScanQuery::new(TimeWindow::all()))
            .unwrap();
        let ids: Vec<_> = scan.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["jun", "jul", "aug"]);
        assert_eq!(scan.stats.lines_skipped, 1);
        assert_eq!(scan.stats.lines_read, 4);
    }

    #[test]
    fn missing_store_scans_empty() {
        let dir = tempfile::tempdir().unwrap();
        let scanner = Scanner::new(StorePaths::new(dir.path().join("absent")));
        let scan = scanner.scan(&ScanQuery::new(TimeWindow::all())).unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.stats, ScanStats::default());
    }

    #[test]
    fn ignores_non_partition_files() {
        let (_dir, paths) = store_with_three_months();
        std::fs::write(paths.events_dir().join("notes.txt"), "junk").unwrap();
        std::fs::write(paths.events_dir().join("scratch.jsonl"), "junk\n").unwrap();
        let scan = Scanner::new(paths)
            .scan(&ScanQuery::new(TimeWindow::all()))
            .unwrap();
        assert_eq!(scan.events.len(), 3);
        assert_eq!(scan.stats.partitions_read, 3);
    }
}
