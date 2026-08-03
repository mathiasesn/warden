//! Data-dir resolution and the monthly partition layout (MVP §2.1).

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, TimeZone, Utc};

use crate::cli::TimeWindow;

/// Layout of the store on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePaths {
    root: PathBuf,
}

impl StorePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve the store root: `--data-dir` beats config, config beats
    /// `~/.warden`.
    pub fn resolve(flag: Option<&Path>, configured: Option<&Path>) -> Result<Self, io::Error> {
        if let Some(path) = flag.or(configured) {
            return Ok(Self::new(expand_tilde(path)?));
        }
        Ok(Self::new(default_root()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn events_dir(&self) -> PathBuf {
        self.root.join("events")
    }

    pub fn prompts_dir(&self) -> PathBuf {
        self.root.join("prompts")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// `state/ingest.jsonl` — append-only cursors, last record per path wins.
    pub fn ingest_state_file(&self) -> PathBuf {
        self.state_dir().join("ingest.jsonl")
    }

    /// `events/YYYY-MM.jsonl` for the partition an event timestamp belongs to.
    pub fn event_partition(&self, partition: Partition) -> PathBuf {
        self.events_dir().join(partition.file_name())
    }

    /// `prompts/YYYY-MM.jsonl`, partitioned identically to events.
    pub fn prompt_partition(&self, partition: Partition) -> PathBuf {
        self.prompts_dir().join(partition.file_name())
    }

    /// Partition files in `dir` overlapping `window`, chronologically.
    ///
    /// `events/` and `prompts/` are partitioned identically, so the overlap rule
    /// lives here once: both the event scanner and the prompt reader call this
    /// rather than each deciding for itself what "overlaps" means.
    ///
    /// A missing directory is an empty store, not an error.
    pub fn partitions_in(
        dir: &Path,
        window: TimeWindow,
    ) -> Result<Vec<(Partition, PathBuf)>, io::Error> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut found = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(partition) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(Partition::parse_stem)
            else {
                continue;
            };
            // Half-open overlap: [start, end) against [from, to).
            if partition.end_ms() > window.from_ms && partition.start_ms() < window.to_ms {
                found.push((partition, path));
            }
        }
        found.sort_by_key(|(partition, _)| *partition);
        Ok(found)
    }
}

/// One month of data, in UTC. Partition membership is decided by the event's
/// UTC month, so a boundary is unambiguous regardless of local timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Partition {
    pub year: i32,
    pub month: u32,
}

impl Partition {
    pub fn new(year: i32, month: u32) -> Self {
        debug_assert!((1..=12).contains(&month));
        Self { year, month }
    }

    /// The partition an epoch-millisecond timestamp routes to.
    pub fn for_timestamp(ts_ms: i64) -> Option<Self> {
        let dt: DateTime<Utc> = Utc.timestamp_millis_opt(ts_ms).single()?;
        Some(Self::new(dt.year(), dt.month()))
    }

    /// Parse `YYYY-MM` from a partition file stem.
    pub fn parse_stem(stem: &str) -> Option<Self> {
        let (year, month) = stem.split_once('-')?;
        if year.len() != 4 || month.len() != 2 {
            return None;
        }
        let year: i32 = year.parse().ok()?;
        let month: u32 = month.parse().ok()?;
        (1..=12).contains(&month).then(|| Self::new(year, month))
    }

    pub fn file_name(&self) -> String {
        format!("{:04}-{:02}.jsonl", self.year, self.month)
    }

    /// Inclusive epoch-ms start of the month.
    pub fn start_ms(&self) -> i64 {
        Utc.with_ymd_and_hms(self.year, self.month, 1, 0, 0, 0)
            .single()
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(i64::MIN)
    }

    /// Exclusive epoch-ms end of the month (start of the next one).
    pub fn end_ms(&self) -> i64 {
        self.next().start_ms()
    }

    pub fn next(&self) -> Self {
        if self.month == 12 {
            Self::new(self.year + 1, 1)
        } else {
            Self::new(self.year, self.month + 1)
        }
    }
}

/// `~/.warden`.
fn default_root() -> Result<PathBuf, io::Error> {
    Ok(home_dir()?.join(".warden"))
}

fn home_dir() -> Result<PathBuf, io::Error> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()));
    home.map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot determine home directory; pass --data-dir",
        )
    })
}

/// Expand a leading `~` so config files can use the same notation as MVP §7.
pub fn expand_tilde(path: &Path) -> Result<PathBuf, io::Error> {
    let Some(text) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    match text.strip_prefix('~') {
        Some("") => home_dir(),
        Some(rest) if rest.starts_with('/') => Ok(home_dir()?.join(rest.trim_start_matches('/'))),
        _ => Ok(path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_by_utc_month() {
        let ts = Utc
            .with_ymd_and_hms(2026, 8, 4, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            Partition::for_timestamp(ts).unwrap(),
            Partition::new(2026, 8)
        );
    }

    #[test]
    fn month_boundary_is_utc_exact() {
        let last = Utc
            .with_ymd_and_hms(2026, 7, 31, 23, 59, 59)
            .unwrap()
            .timestamp_millis()
            + 999;
        let first = last + 1;
        assert_eq!(
            Partition::for_timestamp(last).unwrap(),
            Partition::new(2026, 7)
        );
        assert_eq!(
            Partition::for_timestamp(first).unwrap(),
            Partition::new(2026, 8)
        );
        assert_eq!(Partition::new(2026, 7).end_ms(), first);
    }

    #[test]
    fn december_rolls_into_january() {
        assert_eq!(Partition::new(2026, 12).next(), Partition::new(2027, 1));
        assert_eq!(
            Partition::for_timestamp(Partition::new(2026, 12).end_ms()).unwrap(),
            Partition::new(2027, 1)
        );
    }

    #[test]
    fn file_names_round_trip() {
        let p = Partition::new(2026, 1);
        assert_eq!(p.file_name(), "2026-01.jsonl");
        assert_eq!(Partition::parse_stem("2026-01").unwrap(), p);
        assert_eq!(Partition::parse_stem("2026-13"), None);
        assert_eq!(Partition::parse_stem("nonsense"), None);
        assert_eq!(Partition::parse_stem("2026-1"), None);
    }

    #[test]
    fn data_dir_flag_beats_config() {
        let flag = PathBuf::from("/flag");
        let cfg = PathBuf::from("/cfg");
        assert_eq!(
            StorePaths::resolve(Some(&flag), Some(&cfg)).unwrap().root(),
            Path::new("/flag")
        );
        assert_eq!(
            StorePaths::resolve(None, Some(&cfg)).unwrap().root(),
            Path::new("/cfg")
        );
    }

    #[test]
    fn layout_is_the_documented_one() {
        let paths = StorePaths::new("/store");
        assert_eq!(
            paths.event_partition(Partition::new(2026, 8)),
            Path::new("/store/events/2026-08.jsonl")
        );
        assert_eq!(
            paths.prompt_partition(Partition::new(2026, 8)),
            Path::new("/store/prompts/2026-08.jsonl")
        );
        assert_eq!(
            paths.ingest_state_file(),
            Path::new("/store/state/ingest.jsonl")
        );
        assert_eq!(paths.config_file(), Path::new("/store/config.toml"));
    }
}
