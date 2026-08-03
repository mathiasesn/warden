//! The named reports (MVP §3) and the rollup behind `warden query`.
//!
//! Every report in here is a pure consumer of [`crate::store::Scanner`] — none
//! of them opens an event file itself (MVP §8 step 4). A report's whole job is
//! to turn a window of events into a [`Report`]; whether that reaches a terminal
//! or a harness is [`crate::output`]'s problem.
//!
//! Three honesty rules are enforced here rather than in each report:
//!
//! - A figure warden cannot derive is [`Cell::Unsupported`], never `0`.
//! - An absent token count means "not applicable on this record", because usage
//!   is logged once per request and repeated on no sibling. Summing treats it as
//!   contributing nothing, and [`Notes`] says how many records carried usage.
//! - Anything that would make a number differ from what a user sees elsewhere —
//!   sidechain events, unpriced models, skipped lines — becomes a note.

pub mod compare;
pub mod files;
pub mod models;
pub mod projects;
pub mod query;
pub mod sessions;
pub mod summary;
pub mod tools;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;

use chrono::{TimeZone, Utc};
use serde_json::{Map, Value};

use crate::cli::TimeWindow;
use crate::output::{Cell, Report};
use crate::store::{Event, ScanQuery, ScanStats, Scanner};

/// The model name Claude Code writes for records it synthesized itself. It is
/// not a model anyone is billed for, so it is counted in volume and excluded
/// from cost (see [`Cost::add`]).
pub const SYNTHETIC_MODEL: &str = "<synthetic>";

/// The named reports, in the order MVP §3 lists them.
pub const NAMES: [&str; 7] = [
    "summary", "projects", "models", "sessions", "tools", "compare", "files",
];

/// Everything a report is given besides the scanner.
#[derive(Debug, Clone)]
pub struct ReportCtx {
    pub window: TimeWindow,
    pub project: Option<String>,
    /// Sidechain (subagent) events are real spend and are included by default.
    pub include_sidechain: bool,
}

impl ReportCtx {
    pub fn new(window: TimeWindow, project: Option<String>, include_sidechain: bool) -> Self {
        Self {
            window,
            project,
            include_sidechain,
        }
    }

    /// The same context over a different window, for `compare`.
    pub fn with_window(&self, window: TimeWindow) -> Self {
        Self {
            window,
            ..self.clone()
        }
    }

    fn scan_query(&self) -> ScanQuery {
        ScanQuery::new(self.window).with_project(self.project.clone())
    }
}

/// A report by name.
pub type Builder = fn(&Scanner, &ReportCtx) -> Result<Report, ReportError>;

/// Resolve a report name, listing the valid ones when it is not one.
pub fn resolve(name: &str) -> Result<Builder, ReportError> {
    match name {
        "summary" => Ok(summary::build),
        "projects" => Ok(projects::build),
        "models" => Ok(models::build),
        "sessions" => Ok(sessions::build),
        "tools" => Ok(tools::build),
        "compare" => Ok(compare::build),
        "files" => Ok(files::build),
        other => Err(ReportError::Unknown(other.to_string())),
    }
}

/// Build a named report end to end.
pub fn run(scanner: &Scanner, name: &str, ctx: &ReportCtx) -> Result<Report, ReportError> {
    resolve(name)?(scanner, ctx)
}

#[derive(Debug)]
pub enum ReportError {
    /// No such report. Carries the name so the message can list the real ones.
    Unknown(String),
    /// The report is meaningless without a bounded period (`compare`).
    NeedsWindow(&'static str),
    /// `--group-by` named a dimension that does not exist.
    UnknownDimension(String),
    Io(io::Error),
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::Unknown(name) => write!(
                f,
                "unknown report {:?}: expected one of {}",
                name,
                NAMES.join(", ")
            ),
            ReportError::NeedsWindow(name) => write!(
                f,
                "report {name} compares a period against the one before it, so it needs a bounded \
                 period: pass --since (e.g. --since 7d)"
            ),
            ReportError::UnknownDimension(dim) => write!(
                f,
                "unknown --group-by dimension {:?}: expected one of {}",
                dim,
                query::DIMENSIONS.join(", ")
            ),
            ReportError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ReportError {}

impl From<io::Error> for ReportError {
    fn from(err: io::Error) -> Self {
        ReportError::Io(err)
    }
}

/// A completed scan, with the observations every report turns into notes.
pub struct Scanned {
    pub events: Vec<Event>,
    pub stats: ScanStats,
    pub notes: Notes,
}

/// Read the window through the one shared scanner.
pub fn scan(scanner: &Scanner, ctx: &ReportCtx) -> Result<Scanned, ReportError> {
    let mut events = Vec::new();
    let mut notes = Notes::new(ctx.include_sidechain);
    let stats = scanner.scan_with(&ctx.scan_query(), |event| {
        if event.is_sidechain == Some(true) {
            notes.sidechain_events += 1;
            if !ctx.include_sidechain {
                return;
            }
        }
        notes.observe(&event);
        events.push(event);
    })?;
    notes.lines_skipped = stats.lines_skipped;
    Ok(Scanned {
        events,
        stats,
        notes,
    })
}

/// Running cost for one bucket.
///
/// `priced` and `unpriced` are counted separately so a bucket whose model has no
/// configured rate renders as `–` instead of `$0.00` (MVP §2.5).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    pub total: f64,
    /// Events that carried usage and a configured price.
    pub priced: u64,
    /// Events that carried usage but whose model has no configured price.
    pub unpriced: u64,
}

impl Cost {
    fn add(&mut self, event: &Event) {
        if !has_usage(event) || event.model.as_deref() == Some(SYNTHETIC_MODEL) {
            return;
        }
        match event.cost_est {
            Some(cost) => {
                self.total += cost;
                self.priced += 1;
            }
            None => self.unpriced += 1,
        }
    }

    fn merge(&mut self, other: Cost) {
        self.total += other.total;
        self.priced += other.priced;
        self.unpriced += other.unpriced;
    }

    /// `–` when nothing in this bucket could be priced; otherwise an estimate.
    pub fn cell(&self) -> Cell {
        if self.priced == 0 {
            Cell::Unsupported
        } else {
            Cell::money_est(self.total)
        }
    }

    /// `null` rather than `0` when nothing could be priced.
    pub fn json(&self) -> Value {
        if self.priced == 0 {
            Value::Null
        } else {
            serde_json::json!(round_money(self.total))
        }
    }
}

/// Whether this record carries usage at all. Usage is logged once per request,
/// so most records legitimately carry none.
pub fn has_usage(event: &Event) -> bool {
    event.input_tok.is_some()
        || event.output_tok.is_some()
        || event.cache_read_tok.is_some()
        || event.cache_write_tok.is_some()
}

/// Everything summed for one bucket of events.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    /// Events in the bucket, whether or not they carried usage.
    pub events: u64,
    /// Events that carried usage — i.e. billable requests.
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: Cost,
    pub sessions: BTreeSet<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
}

impl Totals {
    pub fn add(&mut self, event: &Event) {
        self.events += 1;
        if has_usage(event) {
            self.requests += 1;
        }
        self.input += event.input_tok.unwrap_or(0);
        self.output += event.output_tok.unwrap_or(0);
        self.cache_read += event.cache_read_tok.unwrap_or(0);
        self.cache_write += event.cache_write_tok.unwrap_or(0);
        self.cost.add(event);
        if let Some(session) = &event.session_id {
            self.sessions.insert(session.clone());
        }
        self.first_ts = Some(self.first_ts.map_or(event.ts, |ts| ts.min(event.ts)));
        self.last_ts = Some(self.last_ts.map_or(event.ts, |ts| ts.max(event.ts)));
    }

    pub fn merge(&mut self, other: &Totals) {
        self.events += other.events;
        self.requests += other.requests;
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cost.merge(other.cost);
        self.sessions.extend(other.sessions.iter().cloned());
        self.first_ts = min_opt(self.first_ts, other.first_ts);
        self.last_ts = max_opt(self.last_ts, other.last_ts);
    }

    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    /// The four columns every report ends with: in, out, cache read, cost.
    pub fn tail_cells(&self) -> Vec<Cell> {
        vec![
            count(self.input),
            count(self.output),
            count(self.cache_read),
            self.cost.cell(),
        ]
    }

    /// The token fields, written into a JSON row.
    pub fn write_json(&self, row: &mut Map<String, Value>) {
        row.insert("requests".into(), serde_json::json!(self.requests));
        row.insert("input_tok".into(), serde_json::json!(self.input));
        row.insert("output_tok".into(), serde_json::json!(self.output));
        row.insert("cache_read_tok".into(), serde_json::json!(self.cache_read));
        row.insert(
            "cache_write_tok".into(),
            serde_json::json!(self.cache_write),
        );
        row.insert("cost_est".into(), self.cost.json());
    }
}

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Bucket events by a key. `None` from `key` drops the event from the rollup.
pub fn rollup<K, F>(events: &[Event], key: F) -> BTreeMap<K, Totals>
where
    K: Ord,
    F: Fn(&Event) -> Option<K>,
{
    let mut buckets: BTreeMap<K, Totals> = BTreeMap::new();
    for event in events {
        if let Some(k) = key(event) {
            buckets.entry(k).or_default().add(event);
        }
    }
    buckets
}

/// Heaviest bucket first, ties broken by key so output is deterministic.
pub fn by_weight_desc<K: Ord + Clone>(buckets: BTreeMap<K, Totals>) -> Vec<(K, Totals)> {
    let mut rows: Vec<(K, Totals)> = buckets.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.total_tokens()
            .cmp(&a.1.total_tokens())
            .then_with(|| a.0.cmp(&b.0))
    });
    rows
}

/// A count cell, saturating rather than wrapping on an implausible total.
pub fn count(n: u64) -> Cell {
    Cell::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `2026-08-04` in UTC. The store is UTC throughout, so days are too.
pub fn day_of(ts_ms: i64) -> Option<String> {
    Utc.timestamp_millis_opt(ts_ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
}

/// `1h12m`, `4m`, `12s` — a wall-clock span, for `sessions`.
pub fn format_span(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Money is compared and diffed as a float but published at cent precision.
pub fn round_money(amount: f64) -> f64 {
    (amount * 100.0).round() / 100.0
}

/// Long ids are unreadable in a table; JSON keeps them whole.
pub fn short_id(id: &str) -> String {
    match id.char_indices().nth(8) {
        Some((idx, _)) => format!("{}…", &id[..idx]),
        None => id.to_string(),
    }
}

/// The observations that become a report's `notes`.
///
/// Notes exist so a number a user cannot reconcile against what their agent
/// showed them is explained rather than merely printed.
#[derive(Debug, Clone)]
pub struct Notes {
    include_sidechain: bool,
    pub sidechain_events: u64,
    pub lines_skipped: u64,
    unpriced_models: BTreeSet<String>,
    synthetic_events: u64,
    events: u64,
    requests: u64,
    extra: Vec<String>,
}

impl Notes {
    fn new(include_sidechain: bool) -> Self {
        Self {
            include_sidechain,
            sidechain_events: 0,
            lines_skipped: 0,
            unpriced_models: BTreeSet::new(),
            synthetic_events: 0,
            events: 0,
            requests: 0,
            extra: Vec::new(),
        }
    }

    fn observe(&mut self, event: &Event) {
        self.events += 1;
        if !has_usage(event) {
            return;
        }
        self.requests += 1;
        if event.model.as_deref() == Some(SYNTHETIC_MODEL) {
            self.synthetic_events += 1;
        } else if event.cost_est.is_none() {
            self.unpriced_models.insert(
                event
                    .model
                    .clone()
                    .unwrap_or_else(|| "(unknown model)".into()),
            );
        }
    }

    /// Add a report-specific note, kept ahead of the shared ones.
    pub fn push(&mut self, note: impl Into<String>) {
        self.extra.push(note.into());
    }

    /// Merge another window's observations in (`compare` scans twice).
    pub fn merge(&mut self, other: &Notes) {
        self.sidechain_events += other.sidechain_events;
        self.lines_skipped += other.lines_skipped;
        self.unpriced_models
            .extend(other.unpriced_models.iter().cloned());
        self.synthetic_events += other.synthetic_events;
        self.events += other.events;
        self.requests += other.requests;
    }

    /// The full note list, report-specific notes first.
    pub fn finish(&self) -> Vec<String> {
        let mut notes = self.extra.clone();

        if self.events > 0 {
            notes.push(format!(
                "usage is recorded once per request: {} of {} events carry token counts, and the \
                 rest contribute nothing rather than zero",
                self.requests, self.events
            ));
        }

        if self.sidechain_events > 0 {
            notes.push(if self.include_sidechain {
                format!(
                    "includes {} sidechain (subagent) events — real spend, but counted in no \
                     per-session figure your agent shows you, so these totals will read higher; \
                     pass --no-sidechain to exclude them",
                    self.sidechain_events
                )
            } else {
                format!(
                    "excludes {} sidechain (subagent) events (--no-sidechain); they are real \
                     spend, so these totals understate it",
                    self.sidechain_events
                )
            });
        }

        if !self.unpriced_models.is_empty() {
            notes.push(format!(
                "no configured price for {} — est. cost is blank for those rows rather than 0; add \
                 rates under [pricing.<provider>] in config.toml",
                self.unpriced_models
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        if self.synthetic_events > 0 {
            notes.push(format!(
                "{} events report the model as {SYNTHETIC_MODEL}, a placeholder the agent writes \
                 for records it generated itself; their tokens are counted and their cost is not, \
                 because nobody is billed for them",
                self.synthetic_events
            ));
        }

        if self.lines_skipped > 0 {
            notes.push(format!(
                "skipped {} unreadable line(s) in the store (a torn final line, or schema drift)",
                self.lines_skipped
            ));
        }

        notes.push("cost figures are estimates".into());
        notes
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    use crate::store::{Event, StorePaths, StoreWriter, ToolCall};
    use chrono::{TimeZone, Utc};

    pub fn ms(y: i32, mo: u32, d: u32, h: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    /// An assistant event carrying usage.
    pub fn used(id: &str, ts: i64, project: &str, model: &str, input: u64, output: u64) -> Event {
        let mut event = Event::new(id, ts, "claude-code", "anthropic", "assistant");
        event.project = Some(project.into());
        event.model = Some(model.into());
        event.session_id = Some(format!("session-{project}"));
        event.input_tok = Some(input);
        event.output_tok = Some(output);
        event.cache_read_tok = Some(input * 10);
        event.cache_write_tok = Some(0);
        event
    }

    pub fn priced(mut event: Event, cost: f64) -> Event {
        event.cost_est = Some(cost);
        event
    }

    pub fn with_tools(mut event: Event, targets: &[(&str, &str)]) -> Event {
        event.tool_calls = targets
            .iter()
            .map(|(name, target)| ToolCall::new(*name, Some((*target).to_string())))
            .collect();
        event
    }

    pub fn store(events: &[Event]) -> (tempfile::TempDir, StorePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let mut writer = StoreWriter::open(paths.clone()).unwrap();
        for event in events {
            writer.append_event(event).unwrap();
        }
        (dir, paths)
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    fn ctx() -> ReportCtx {
        ReportCtx::new(TimeWindow::all(), None, true)
    }

    #[test]
    fn unknown_report_lists_the_valid_names() {
        let err = resolve("costs").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown report \"costs\""), "{msg}");
        for name in NAMES {
            assert!(msg.contains(name), "{msg} is missing {name}");
        }
    }

    #[test]
    fn every_documented_name_resolves() {
        for name in NAMES {
            assert!(resolve(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn an_unpriced_bucket_is_unsupported_not_zero() {
        let mut totals = Totals::default();
        totals.add(&used("a", 0, "p", "claude-opus-5", 10, 10));
        assert_eq!(totals.cost.cell(), Cell::Unsupported);
        assert_eq!(totals.cost.json(), Value::Null);

        totals.add(&priced(used("b", 0, "p", "claude-opus-5", 10, 10), 0.5));
        assert_eq!(totals.cost.cell(), Cell::money_est(0.5));
        assert_eq!(totals.cost.json(), serde_json::json!(0.5));
    }

    #[test]
    fn records_without_usage_contribute_nothing_but_are_still_counted() {
        let mut totals = Totals::default();
        totals.add(&used("a", 0, "p", "m", 100, 20));
        let mut sibling = Event::new("b", 0, "claude-code", "anthropic", "assistant");
        sibling.session_id = Some("session-p".into());
        totals.add(&sibling);

        assert_eq!(totals.events, 2);
        assert_eq!(totals.requests, 1, "usage is counted once per request");
        assert_eq!(totals.input, 100);
        assert_eq!(totals.sessions.len(), 1);
    }

    #[test]
    fn synthetic_is_counted_in_tokens_and_excluded_from_cost() {
        let mut totals = Totals::default();
        totals.add(&priced(used("a", 0, "p", SYNTHETIC_MODEL, 10, 5), 9.99));
        assert_eq!(totals.input, 10);
        assert_eq!(
            totals.cost,
            Cost::default(),
            "no cost accrues to {SYNTHETIC_MODEL}"
        );
        assert_eq!(totals.cost.cell(), Cell::Unsupported);
    }

    #[test]
    fn sidechain_events_are_included_by_default_and_always_noted() {
        let mut sidechain = used("s", ms(2026, 8, 4, 9), "p", "m", 5, 5);
        sidechain.is_sidechain = Some(true);
        let (_dir, paths) = store(&[used("a", ms(2026, 8, 4, 8), "p", "m", 10, 10), sidechain]);
        let scanner = Scanner::new(paths);

        let scanned = scan(&scanner, &ctx()).unwrap();
        assert_eq!(scanned.events.len(), 2);
        assert!(scanned
            .notes
            .finish()
            .iter()
            .any(|n| n.contains("includes 1 sidechain")));

        let excluded = scan(&scanner, &ReportCtx::new(TimeWindow::all(), None, false)).unwrap();
        assert_eq!(excluded.events.len(), 1);
        assert!(excluded
            .notes
            .finish()
            .iter()
            .any(|n| n.contains("excludes 1 sidechain")));
    }

    #[test]
    fn unpriced_models_are_named_in_the_notes() {
        let (_dir, paths) = store(&[used("a", ms(2026, 8, 4, 8), "p", "claude-opus-5", 10, 10)]);
        let notes = scan(&Scanner::new(paths), &ctx()).unwrap().notes.finish();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("no configured price for claude-opus-5")),
            "{notes:?}"
        );
        assert!(notes.iter().any(|n| n == "cost figures are estimates"));
    }

    #[test]
    fn spans_and_ids_are_readable() {
        assert_eq!(format_span(0), "0s");
        assert_eq!(format_span(45_000), "45s");
        assert_eq!(format_span(4 * 60_000 + 5_000), "4m05s");
        assert_eq!(format_span(72 * 60_000), "1h12m");
        assert_eq!(short_id("0123456789abcdef"), "01234567…");
        assert_eq!(short_id("short"), "short");
    }

    #[test]
    fn days_are_utc() {
        assert_eq!(day_of(ms(2026, 8, 4, 23)).unwrap(), "2026-08-04");
    }
}
