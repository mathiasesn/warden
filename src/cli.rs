//! Command-line surface: argument parsing and the shared `--since` time window.

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use clap::{Parser, Subcommand};

/// Top-level CLI. Global flags apply to every subcommand (MVP §3).
#[derive(Debug, Parser)]
#[command(
    name = "warden",
    version,
    about = "Local, read-only reporting over coding-agent session logs"
)]
pub struct Cli {
    /// Emit the versioned JSON envelope instead of a table.
    #[arg(long, global = true)]
    pub json: bool,

    /// Time window: relative (`7d`, `24h`, `90m`) or absolute (`2026-01-01`).
    #[arg(long, global = true, value_name = "7d|2026-01-01")]
    pub since: Option<String>,

    /// Restrict output to a single project.
    #[arg(long, global = true, value_name = "NAME")]
    pub project: Option<String>,

    /// Override the store location (default: config, else `~/.warden`).
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Skip the implicit ingest that otherwise runs before a report.
    #[arg(long, global = true)]
    pub no_ingest: bool,

    /// Exclude subagent (sidechain) events. They are real spend and are
    /// included by default; excluding them understates totals.
    #[arg(long, global = true)]
    pub no_sidechain: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scan sources and append new events to the store.
    Ingest,
    /// Render a named report.
    Report {
        /// Report name (`summary`, `projects`, `models`, ...).
        name: String,
    },
    /// Filtered event rollup over named dimensions.
    Query {
        /// Dimensions to group by, comma-separated.
        #[arg(long, value_name = "DIMS")]
        group_by: Option<String>,
    },
    /// Live burn rate.
    Watch {
        /// Print a single status-bar-friendly line and exit.
        #[arg(long)]
        oneline: bool,
    },
    /// Detected improvements (exact-duplicate prompts).
    Suggest {
        /// Print the SKILL.md draft for a suggestion to stdout.
        #[arg(long, value_name = "ID")]
        draft: Option<String>,
    },
    /// Report what warden can see and why a number might be empty.
    Doctor,
    /// Remove stored data. Rewrites files, so it must be explicit.
    Purge {
        /// Delete stored prompt text.
        #[arg(long)]
        prompts: bool,
        /// Skip the confirmation prompt. Required when stdin is not a terminal.
        #[arg(long, visible_alias = "force")]
        yes: bool,
    },
}

impl Command {
    /// Stable name used in the JSON envelope and error messages.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Ingest => "ingest",
            Command::Report { .. } => "report",
            Command::Query { .. } => "query",
            Command::Watch { .. } => "watch",
            Command::Suggest { .. } => "suggest",
            Command::Doctor => "doctor",
            Command::Purge { .. } => "purge",
        }
    }
}

/// A half-open `[from, to)` window in epoch milliseconds.
///
/// Every read path takes one of these; it is what lets the scanner open only
/// the partitions that overlap the requested period (MVP §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    /// Inclusive lower bound, epoch ms.
    pub from_ms: i64,
    /// Exclusive upper bound, epoch ms.
    pub to_ms: i64,
}

impl TimeWindow {
    pub fn new(from_ms: i64, to_ms: i64) -> Self {
        Self { from_ms, to_ms }
    }

    /// The widest window representable; used when `--since` is absent.
    pub fn all() -> Self {
        Self::new(i64::MIN, i64::MAX)
    }

    pub fn contains(&self, ts_ms: i64) -> bool {
        ts_ms >= self.from_ms && ts_ms < self.to_ms
    }

    pub fn from(&self) -> Option<DateTime<Utc>> {
        Utc.timestamp_millis_opt(self.from_ms).single()
    }

    pub fn to(&self) -> Option<DateTime<Utc>> {
        Utc.timestamp_millis_opt(self.to_ms).single()
    }

    /// Parse a `--since` value relative to `now`, ending at `now`.
    pub fn parse_since(spec: &str, now: DateTime<Utc>) -> Result<Self, SinceParseError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(SinceParseError::new(spec));
        }

        if let Some(date) = parse_absolute(spec) {
            return Ok(Self::new(date.timestamp_millis(), now.timestamp_millis()));
        }

        let duration = parse_relative(spec).ok_or_else(|| SinceParseError::new(spec))?;
        let from = now
            .checked_sub_signed(duration)
            .ok_or_else(|| SinceParseError::new(spec))?;
        Ok(Self::new(from.timestamp_millis(), now.timestamp_millis()))
    }
}

fn parse_absolute(spec: &str) -> Option<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(spec, "%Y-%m-%d").ok()?;
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?).into()
}

fn parse_relative(spec: &str) -> Option<Duration> {
    let (digits, unit) = spec.split_at(spec.len().checked_sub(1)?);
    let n: i64 = digits.parse().ok()?;
    if n < 0 {
        return None;
    }
    match unit {
        "m" => Duration::try_minutes(n),
        "h" => Duration::try_hours(n),
        "d" => Duration::try_days(n),
        "w" => Duration::try_weeks(n),
        _ => None,
    }
}

/// `--since` could not be interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinceParseError {
    spec: String,
}

impl SinceParseError {
    fn new(spec: &str) -> Self {
        Self {
            spec: spec.to_string(),
        }
    }
}

impl fmt::Display for SinceParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid --since value {:?}: expected a relative window like 7d, 24h, 90m, 2w, \
             or an absolute date like 2026-01-01",
            self.spec
        )
    }
}

impl std::error::Error for SinceParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 4, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_relative_days() {
        let w = TimeWindow::parse_since("7d", now()).unwrap();
        assert_eq!(w.to_ms, now().timestamp_millis());
        assert_eq!(
            w.from_ms,
            (now() - Duration::try_days(7).unwrap()).timestamp_millis()
        );
    }

    #[test]
    fn parses_relative_hours_minutes_weeks() {
        for (spec, dur) in [
            ("24h", Duration::try_hours(24).unwrap()),
            ("90m", Duration::try_minutes(90).unwrap()),
            ("2w", Duration::try_weeks(2).unwrap()),
            ("30d", Duration::try_days(30).unwrap()),
        ] {
            let w = TimeWindow::parse_since(spec, now()).unwrap();
            assert_eq!(w.from_ms, (now() - dur).timestamp_millis(), "spec {spec}");
        }
    }

    #[test]
    fn parses_absolute_date_as_utc_midnight() {
        let w = TimeWindow::parse_since("2026-01-01", now()).unwrap();
        let expected = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(w.from_ms, expected.timestamp_millis());
        assert_eq!(w.to_ms, now().timestamp_millis());
    }

    #[test]
    fn rejects_nonsense() {
        for spec in ["", "d", "7y", "-3d", "7 d", "2026-13-01", "seven days"] {
            assert!(
                TimeWindow::parse_since(spec, now()).is_err(),
                "expected {spec:?} to be rejected"
            );
        }
    }

    #[test]
    fn window_containment_is_half_open() {
        let w = TimeWindow::new(100, 200);
        assert!(w.contains(100));
        assert!(w.contains(199));
        assert!(!w.contains(200));
        assert!(!w.contains(99));
        assert!(TimeWindow::all().contains(0));
    }
}
