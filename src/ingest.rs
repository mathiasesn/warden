//! Resumable, idempotent ingest.
//!
//! Three properties matter, and each is bought by one mechanism:
//!
//! * **Cheap re-runs** — a cursor per source file records the byte offset
//!   consumed. A file whose size and mtime are unchanged is not opened at all.
//! * **Idempotence** — event ids are content-derived, so a replay produces
//!   lines that are already in the store and are dropped before they are
//!   written. Re-ingesting yields a byte-identical store.
//! * **Crash safety** — a cursor is written only after a file has been read to
//!   its last complete line. An interrupted run therefore replays from the last
//!   committed offset, and the replay dedupes, so an interrupted-then-resumed
//!   run and an uninterrupted one produce the same store.
//!
//! A partial trailing line (a writer caught mid-append) is never consumed: it
//! is reported as skipped for this run and picked up once it is complete.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::adapters::{self, usage_key, Adapter, Parsed};
use crate::cli::TimeWindow;
use crate::config::{Config, TokenCounts};
use crate::store::{
    expand_tilde, text_hash, Event, IngestCursor, PromptRecord, ScanQuery, Scanner, StorePaths,
    StoreWriter,
};

/// Scope for one ingest run. The window narrows *which files are opened*; it
/// never drops events from a file that is read, because the store is supposed
/// to be complete for every period it covers.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Only consider source files modified within this window.
    pub window: TimeWindow,
    /// Only store events for this project. Because this does drop events, a
    /// run with a project filter deliberately writes no cursors — a cursor must
    /// only ever mean "this file is fully ingested".
    pub project: Option<String>,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            window: TimeWindow::all(),
            project: None,
        }
    }
}

/// What one adapter did, in the shape `warden ingest` prints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterIngest {
    pub adapter: String,
    /// Source root, when one could be resolved.
    pub root: Option<PathBuf>,
    /// Files that had unread bytes and were opened this run.
    pub files_read: usize,
    /// Files present under the root, whether or not they were opened.
    pub files_seen: usize,
    pub new_events: u64,
    pub new_prompts: u64,
    /// Lines the adapter could not parse. Never fatal.
    pub skipped_unparseable: u64,
    /// Source files skipped because their metadata could not be read (the file
    /// vanished mid-run, or its mtime is unusable). Counted and reported rather
    /// than passed over in silence: the next run will retry them.
    pub unreadable_files: u64,
    /// Lines replayed after an interruption that were already stored.
    pub duplicates: u64,
}

/// Result of an ingest run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub adapters: Vec<AdapterIngest>,
    /// True when cursors were withheld because the run was filtered.
    pub cursors_withheld: bool,
}

impl IngestReport {
    pub fn new_events(&self) -> u64 {
        self.adapters.iter().map(|a| a.new_events).sum()
    }
}

/// Run every enabled, implemented adapter against the store.
pub fn run(
    config: &Config,
    paths: &StorePaths,
    options: &IngestOptions,
) -> io::Result<IngestReport> {
    let mut writer = StoreWriter::open(paths.clone())?;
    let mut state = StoreState::load(paths)?;
    let cursors = load_cursors(paths)?;

    let mut report = IngestReport {
        cursors_withheld: options.project.is_some(),
        ..IngestReport::default()
    };
    for adapter in adapters::enabled(config) {
        report.adapters.push(ingest_adapter(
            adapter.as_ref(),
            config,
            options,
            &cursors,
            &mut state,
            &mut writer,
        )?);
    }
    Ok(report)
}

fn ingest_adapter(
    adapter: &dyn Adapter,
    config: &Config,
    options: &IngestOptions,
    cursors: &[IngestCursor],
    state: &mut StoreState,
    writer: &mut StoreWriter,
) -> io::Result<AdapterIngest> {
    let mut summary = AdapterIngest {
        adapter: adapter.name().to_string(),
        ..AdapterIngest::default()
    };

    let Some(root) = adapter
        .root(config)
        .map(|root| expand_tilde(&root))
        .transpose()?
    else {
        return Ok(summary);
    };
    summary.root = Some(root.clone());
    if !root.is_dir() {
        return Ok(summary);
    }

    // Last record for a path wins (the log is append-only), so a single
    // forward pass leaves the newest cursor per path. Indexing once beats
    // re-scanning the whole log for every discovered file — the log grows by a
    // line per file per run, so the linear form degrades every time it runs.
    let mut latest: HashMap<&Path, &IngestCursor> = HashMap::new();
    for cursor in cursors {
        if cursor.adapter == adapter.name() {
            latest.insert(Path::new(&cursor.path), cursor);
        }
    }

    for path in adapter.discover(&root)? {
        summary.files_seen += 1;
        let Ok(meta) = std::fs::metadata(&path) else {
            summary.unreadable_files += 1;
            continue;
        };
        let Ok(mtime) = mtime_ms(&meta) else {
            summary.unreadable_files += 1;
            continue;
        };
        if !options.window.contains(mtime) {
            continue;
        }

        let cursor = latest.get(path.as_path()).copied();
        // A file that shrank was rotated or rewritten; re-read it from the
        // start and let id dedup absorb what is already stored.
        let start = match cursor {
            Some(cursor) if cursor.offset <= meta.len() => cursor.offset,
            _ => 0,
        };
        if start == meta.len() {
            continue;
        }

        summary.files_read += 1;
        let consumed = ingest_file(
            adapter,
            config,
            options,
            &path,
            start,
            state,
            writer,
            &mut summary,
        )?;
        if options.project.is_none() {
            writer.append_cursor(&IngestCursor {
                path: path.to_string_lossy().into_owned(),
                mtime,
                offset: consumed,
                adapter: adapter.name().to_string(),
                ts: Utc::now().timestamp_millis(),
            })?;
        }
    }

    Ok(summary)
}

/// Read one source file from `start`, returning the offset of the end of the
/// last *complete* line consumed.
#[allow(clippy::too_many_arguments)]
fn ingest_file(
    adapter: &dyn Adapter,
    config: &Config,
    options: &IngestOptions,
    path: &Path,
    start: u64,
    state: &mut StoreState,
    writer: &mut StoreWriter,
    summary: &mut AdapterIngest,
) -> io::Result<u64> {
    // Read-only: the source tree is never opened for writing.
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);

    let mut offset = start;
    let mut line = String::new();
    loop {
        line.clear();
        let read = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(read) => read,
            // Invalid UTF-8 is corruption we cannot step over safely; stop
            // here and let the next run retry from the committed offset.
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                summary.skipped_unparseable += 1;
                break;
            }
            Err(err) => return Err(err),
        };
        if !line.ends_with('\n') {
            // A torn trailing line: the writer is mid-append. Skip it without
            // consuming it, so it is ingested once it is complete.
            summary.skipped_unparseable += 1;
            break;
        }

        match adapter.parse_line(path, &line) {
            Parsed::Skipped => {}
            Parsed::Unparseable => summary.skipped_unparseable += 1,
            Parsed::Record(record) => {
                let mut event = record.event;
                if !state.claim_event(&event.id) {
                    summary.duplicates += 1;
                } else if matches(options, &event) {
                    apply_usage(&mut event, record.usage_key.as_deref(), state, config);
                    writer.append_event(&event)?;
                    summary.new_events += 1;
                    if let Some(prompt) =
                        prompt_record(&event, record.prompt_text.as_deref(), config)
                    {
                        writer.append_prompt(event.ts, &prompt)?;
                        summary.new_prompts += 1;
                    }
                }
            }
        }
        offset += read as u64;
    }
    Ok(offset)
}

fn matches(options: &IngestOptions, event: &Event) -> bool {
    match &options.project {
        Some(project) => event.project.as_deref() == Some(project.as_str()),
        None => true,
    }
}

/// Drop repeated per-request usage, then price whatever survives.
fn apply_usage(event: &mut Event, key: Option<&str>, state: &mut StoreState, config: &Config) {
    if let Some(key) = key {
        if !state.claim_usage(key) {
            event.input_tok = None;
            event.output_tok = None;
            event.cache_read_tok = None;
            event.cache_write_tok = None;
            return;
        }
    }
    if let (true, Some(model)) = (event.has_usage(), event.model.as_deref()) {
        // `None` when the model is unpriced — never a misleading 0.0.
        event.cost_est = config.estimate_cost(
            &event.provider,
            model,
            TokenCounts {
                input: event.input_tok,
                output: event.output_tok,
                cache_read: event.cache_read_tok,
                cache_write: event.cache_write_tok,
            },
        );
    }
}

/// `text_hash` is always stored so duplicate detection survives with text off.
fn prompt_record(event: &Event, text: Option<&str>, config: &Config) -> Option<PromptRecord> {
    let text = text?;
    Some(PromptRecord {
        event_id: event.id.clone(),
        text: config.general.index_prompt_text.then(|| text.to_string()),
        text_hash: text_hash(text),
    })
}

/// What the store already contains, so a replay writes nothing twice.
struct StoreState {
    event_ids: HashSet<String>,
    usage_keys: HashSet<String>,
}

impl StoreState {
    /// Rebuild from the store itself rather than from a side file: the JSONL is
    /// the only authority, and this is what makes a resumed run agree with an
    /// uninterrupted one.
    fn load(paths: &StorePaths) -> io::Result<Self> {
        let mut state = Self {
            event_ids: HashSet::new(),
            usage_keys: HashSet::new(),
        };
        let scanner = Scanner::new(paths.clone());
        scanner.scan_with(&ScanQuery::new(TimeWindow::all()), |event| {
            if let (Some(turn), true) = (event.turn_id.as_deref(), event.has_usage()) {
                state
                    .usage_keys
                    .insert(usage_key(&event.agent, event.session_id.as_deref(), turn));
            }
            state.event_ids.insert(event.id);
        })?;
        Ok(state)
    }

    /// True when this id is new.
    fn claim_event(&mut self, id: &str) -> bool {
        self.event_ids.insert(id.to_string())
    }

    /// True when this request's usage has not been counted yet.
    fn claim_usage(&mut self, key: &str) -> bool {
        self.usage_keys.insert(key.to_string())
    }
}

/// Read `state/ingest.jsonl`. Append-only, so later records for a path win;
/// callers scan from the back.
///
/// A line that is *unparseable* is skipped — that is a torn append, and the
/// record before it still stands. A line that cannot be **read** is an error:
/// truncating the cursor list silently would replay every file from an older
/// offset, and the run would look like a clean ingest that just found more
/// events. A wrong answer with no diagnostic is worse than a failed run.
pub fn load_cursors(paths: &StorePaths) -> io::Result<Vec<IngestCursor>> {
    let path = paths.ingest_state_file();
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut cursors = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "reading ingest cursors from {}: {err}; refusing to continue from a truncated \
                     cursor list, which would silently re-read sources from an older offset",
                    path.display()
                ),
            )
        })?;
        if let Ok(cursor) = serde_json::from_str::<IngestCursor>(&line) {
            cursors.push(cursor);
        }
    }
    Ok(cursors)
}

/// A source file's mtime, in epoch milliseconds.
///
/// An unreadable or pre-epoch mtime is an error rather than `0`: `0` is a
/// perfectly valid timestamp, so it would be written into a cursor and compared
/// against `--since` windows as if it were the truth.
fn mtime_ms(meta: &std::fs::Metadata) -> io::Result<i64> {
    let modified = meta.modified()?;
    let since = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("source file mtime precedes the unix epoch: {err}"),
            )
        })?;
    i64::try_from(since.as_millis()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "source file mtime does not fit in epoch milliseconds",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// One assistant record, one sibling sharing its request, a user prompt, a
    /// tool_use, a sidechain record, an unknown type and a malformed line.
    fn fixture() -> String {
        [
            r#"{"type":"mode","mode":"normal"}"#.to_string(),
            record("user", "u1", "10:00:00", None),
            record("assistant", "u2", "10:00:01", Some("req_1")),
            record("assistant", "u3", "10:00:02", Some("req_1")),
            "{not json at all".to_string(),
            record("assistant", "u4", "10:00:03", Some("req_2"))
                .replace("\"isSidechain\":false", "\"isSidechain\":true"),
        ]
        .join("\n")
            + "\n"
    }

    fn record(kind: &str, uuid: &str, time: &str, request: Option<&str>) -> String {
        let request = request
            .map(|id| format!(r#""requestId":"{id}","#))
            .unwrap_or_default();
        let message = if kind == "assistant" {
            r#""message":{"model":"claude-sonnet-4-6","stop_reason":"tool_use",
                "usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,
                         "cache_creation_input_tokens":40},
                "content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}"#
        } else {
            r#""message":{"role":"user","content":"run the tests"}"#
        };
        format!(
            r#"{{"type":"{kind}","uuid":"{uuid}","timestamp":"2026-08-01T{time}.000Z",
              "sessionId":"s1","cwd":"/home/me/code/acme-api","isSidechain":false,{request}{message}}}"#
        )
        .replace('\n', " ")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: StorePaths,
        source: PathBuf,
        config: Config,
    }

    fn setup() -> Fixture {
        setup_with_config(Config::default())
    }

    fn setup_with_config(mut config: Config) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs/projects/-home-me-code-acme-api");
        std::fs::create_dir_all(&logs).unwrap();
        let source = logs.join("s1.jsonl");
        std::fs::write(&source, fixture()).unwrap();

        config.sources.insert(
            "claude-code".into(),
            crate::config::Source {
                enabled: true,
                path: Some(dir.path().join("logs/projects")),
            },
        );
        Fixture {
            store: StorePaths::new(dir.path().join("store")),
            source,
            config,
            _dir: dir,
        }
    }

    fn ingest(f: &Fixture) -> IngestReport {
        run(&f.config, &f.store, &IngestOptions::default()).unwrap()
    }

    fn events(f: &Fixture) -> String {
        std::fs::read_to_string(
            f.store.event_partition(
                crate::store::Partition::for_timestamp(1_785_924_000_000).unwrap(),
            ),
        )
        .unwrap()
    }

    fn prompts(f: &Fixture) -> String {
        std::fs::read_to_string(
            f.store.prompt_partition(
                crate::store::Partition::for_timestamp(1_785_924_000_000).unwrap(),
            ),
        )
        .unwrap()
    }

    #[test]
    fn ingests_every_supported_record_and_counts_the_bad_line() {
        let f = setup();
        let report = ingest(&f);
        let summary = &report.adapters[0];
        assert_eq!(summary.adapter, "claude-code");
        assert_eq!(summary.files_read, 1);
        // 1 user + 3 assistant; `mode` is skipped silently.
        assert_eq!(summary.new_events, 4);
        assert_eq!(summary.new_prompts, 1);
        assert_eq!(summary.skipped_unparseable, 1);

        let stored = events(&f);
        assert_eq!(stored.lines().count(), 4);
        assert!(stored.contains("\"tool_name\":\"Read\""));
        assert!(stored.contains("\"is_sidechain\":true"));
        assert!(!stored.contains("duration_ms"), "never invented");
        assert!(prompts(&f).contains("\"text\":\"run the tests\""));
    }

    #[test]
    fn repeated_request_usage_is_counted_once() {
        let f = setup();
        ingest(&f);
        let counted = events(&f)
            .lines()
            .filter(|line| line.contains("\"input_tok\":10"))
            .count();
        // req_1 appears on two assistant records; req_2 on one.
        assert_eq!(counted, 2);
    }

    #[test]
    fn second_ingest_is_a_byte_identical_no_op() {
        let f = setup();
        ingest(&f);
        let before = events(&f);
        let report = ingest(&f);
        assert_eq!(report.new_events(), 0);
        assert_eq!(
            report.adapters[0].files_read, 0,
            "unchanged file not reopened"
        );
        assert_eq!(events(&f), before);
    }

    #[test]
    fn interrupted_run_replays_to_an_identical_store() {
        // Uninterrupted.
        let full = setup();
        ingest(&full);
        let expected = events(&full);

        // Interrupted: only the first half of the file existed, and no cursor
        // was committed for the rest.
        let partial = setup();
        let all = std::fs::read_to_string(&partial.source).unwrap();
        let cut = all.match_indices('\n').nth(2).unwrap().0 + 1;
        std::fs::write(&partial.source, &all[..cut]).unwrap();
        ingest(&partial);
        std::fs::write(&partial.source, &all).unwrap();
        ingest(&partial);

        assert_eq!(events(&partial), expected);
        assert_eq!(prompts(&partial), prompts(&full));
    }

    #[test]
    fn replaying_from_a_stale_offset_writes_nothing_twice() {
        let f = setup();
        ingest(&f);
        let before = events(&f);
        // Simulate a crash after events were written but before the cursor was:
        // rewind the committed offset to zero.
        let mut state = std::fs::OpenOptions::new()
            .append(true)
            .open(f.store.ingest_state_file())
            .unwrap();
        let mut cursor = load_cursors(&f.store).unwrap().pop().unwrap();
        cursor.offset = 0;
        writeln!(state, "{}", serde_json::to_string(&cursor).unwrap()).unwrap();
        drop(state);

        let report = ingest(&f);
        assert_eq!(report.new_events(), 0);
        assert!(report.adapters[0].duplicates > 0);
        assert_eq!(events(&f), before);
    }

    #[test]
    fn a_torn_trailing_line_is_skipped_counted_and_retried() {
        let f = setup();
        let all = std::fs::read_to_string(&f.source).unwrap();
        let cut = all.rfind('\n').unwrap() + 1;
        let torn = format!("{}{}", &all[..cut], r#"{"type":"assistant","uuid":"u9""#);
        std::fs::write(&f.source, &torn).unwrap();

        let report = ingest(&f);
        // The malformed line plus the torn trailing one.
        assert_eq!(report.adapters[0].skipped_unparseable, 2);
        let stored = events(&f);
        assert!(!stored.contains("\"u9\""));

        // Completing the line ingests it without duplicating anything before it.
        std::fs::write(&f.source, all).unwrap();
        ingest(&f);
        assert_eq!(events(&f).lines().count(), 4);
    }

    #[test]
    fn index_prompt_text_false_stores_only_the_hash() {
        let mut config = Config::default();
        config.general.index_prompt_text = false;
        let f = setup_with_config(config);
        ingest(&f);
        let stored = prompts(&f);
        assert!(stored.contains("\"text_hash\":"));
        assert!(!stored.contains("run the tests"), "{stored}");
    }

    /// An unparseable cursor line is a torn append: skip it, keep the rest.
    #[test]
    fn a_torn_cursor_line_is_skipped_but_the_others_still_load() {
        let f = setup();
        ingest(&f);
        let mut state = std::fs::OpenOptions::new()
            .append(true)
            .open(f.store.ingest_state_file())
            .unwrap();
        write!(state, "{{\"path\":\"/x\",\"mtime\"").unwrap();
        drop(state);

        assert_eq!(load_cursors(&f.store).unwrap().len(), 1);
    }

    /// An *unreadable* cursor line is not: silently truncating the list would
    /// replay every source from an older offset with no diagnostic.
    #[test]
    fn an_unreadable_cursor_line_is_an_error_not_a_silent_truncation() {
        let f = setup();
        ingest(&f);
        let committed = load_cursors(&f.store).unwrap();
        assert_eq!(committed.len(), 1, "a cursor was written to truncate");

        // Invalid UTF-8: `BufRead::lines` yields Err, not a short line.
        let mut state = std::fs::OpenOptions::new()
            .append(true)
            .open(f.store.ingest_state_file())
            .unwrap();
        state.write_all(&[0xff, 0xfe, b'\n']).unwrap();
        drop(state);

        let err = load_cursors(&f.store).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ingest cursors"), "{msg}");
        assert!(msg.contains("older offset"), "{msg}");

        // And the run that would have replayed fails loudly instead.
        assert!(run(&f.config, &f.store, &IngestOptions::default()).is_err());
    }

    #[test]
    fn a_usable_mtime_is_required_rather_than_defaulted_to_zero() {
        let meta = std::fs::metadata(&setup().source).unwrap();
        let mtime = mtime_ms(&meta).expect("a real file has a readable mtime");
        assert!(mtime > 1_700_000_000_000, "got {mtime}");
    }

    #[test]
    fn a_project_filter_scopes_the_run_and_withholds_cursors() {
        let f = setup();
        let options = IngestOptions {
            project: Some("other".into()),
            ..IngestOptions::default()
        };
        let report = run(&f.config, &f.store, &options).unwrap();
        assert_eq!(report.new_events(), 0);
        assert!(report.cursors_withheld);
        assert!(load_cursors(&f.store).unwrap().is_empty());
    }
}
