//! Exact-duplicate prompt detection.
//!
//! One detector, deliberately: prompts are grouped by `text_hash` and nothing
//! else. No embeddings, no fuzzy matching, and therefore no false positives —
//! every group warden reports is a byte-identical prompt you really did send
//! more than once.
//!
//! `prompts/` is the only place prompt text lives, so this is the one read path
//! that opens it. The join back to `events/` (via the shared scanner) is what
//! supplies the timestamp and project a `PromptRecord` deliberately does not
//! carry, and it is also what makes `--since` and `--project` apply here.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

use crate::cli::TimeWindow;
use crate::store::{PromptRecord, ScanQuery, Scanner, StorePaths};

/// How many times a prompt must appear before it is worth reporting.
pub const MIN_OCCURRENCES: usize = 2;

/// Length of the short `<id>` that `--draft` addresses.
const ID_LEN: usize = 8;

/// A prompt short enough to retype is worth an alias; a longer one is carrying
/// a procedure and wants a skill. Documented because it is a judgement call.
const SLASH_COMMAND_MAX_CHARS: usize = 60;

/// Width the prompt is truncated to for one-line display.
const PREVIEW_CHARS: usize = 64;

/// Projects named in a table cell before the rest become a count.
const PROJECTS_SHOWN: usize = 3;

/// Markers of transcript furniture the *client* wrote into the log — slash
/// command expansions, compaction notices, bash echoes, interrupt markers. They
/// repeat constantly and are not prompts anyone typed, so suggesting you alias
/// them is noise. They are counted and reported, never silently dropped.
const FURNITURE_PREFIXES: [&str; 7] = [
    "<local-command-",
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<bash-input>",
    "<bash-stdout>",
    "[Request interrupted",
];

/// What warden suggests doing about a repeated prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Short and literal: alias it.
    SlashCommand,
    /// Longer, or unknown text: draft a skill with this name.
    Skill(String),
}

impl Action {
    /// The `→` line in table output.
    pub fn label(&self) -> String {
        match self {
            Action::SlashCommand => "save as a slash command".to_string(),
            Action::Skill(name) => format!("draft skill: {name}"),
        }
    }

    /// Stable machine-readable kind for `--json`.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::SlashCommand => "slash-command",
            Action::Skill(_) => "skill",
        }
    }

    pub fn skill_name(&self) -> Option<&str> {
        match self {
            Action::SlashCommand => None,
            Action::Skill(name) => Some(name),
        }
    }
}

/// One set of byte-identical prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// Short, stable handle for `--draft`. Derived from `text_hash`, so it is
    /// the same on every run and on every machine with the same prompt.
    pub id: String,
    pub text_hash: String,
    /// Absent when `general.index_prompt_text = false`: the hash still groups,
    /// but there is no text to show.
    pub text: Option<String>,
    pub count: usize,
    /// Most recent occurrence, epoch ms.
    pub last_ts: i64,
    /// Projects the prompt was sent from, sorted; empty if none were recorded.
    pub projects: Vec<String>,
    pub action: Action,
}

impl DuplicateGroup {
    /// One line of the prompt, whitespace-collapsed and truncated, or an
    /// explicit statement that the text was never stored.
    pub fn preview(&self) -> String {
        match &self.text {
            Some(text) => format!("{:?}", truncate(&collapse(text), PREVIEW_CHARS)),
            None => "(text not indexed)".to_string(),
        }
    }

    /// Projects for a table cell: a few names, then a count. A prompt repeated
    /// across two dozen repos would otherwise be one unreadable row. `--json`
    /// keeps the full list.
    pub fn projects_label(&self) -> String {
        if self.projects.is_empty() {
            return "(no project)".to_string();
        }
        let shown = self.projects.len().min(PROJECTS_SHOWN);
        let label = self.projects[..shown].join(", ");
        match self.projects.len() - shown {
            0 => label,
            rest => format!("{label}, +{rest} more"),
        }
    }
}

/// The id a user types at `warden suggest --draft <id>`. Deterministic — same
/// hash, same id, always — so an id printed by one run still resolves in the
/// next. Distinct from `reports::short_id`, which only truncates for display.
pub fn group_id(text_hash: &str) -> String {
    text_hash.chars().take(ID_LEN).collect()
}

/// The result of a scan: the repeated prompts worth acting on, and a count of
/// what was set aside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detection {
    /// Most-repeated first.
    pub groups: Vec<DuplicateGroup>,
    /// Repeated groups that were client transcript furniture, not prompts.
    pub furniture: usize,
}

/// Find every prompt sent more than once in the window.
///
/// Ordering is most-repeated first, then most-recent, then by id — total, so
/// two runs over the same store print the same list in the same order.
pub fn detect(
    scanner: &Scanner,
    window: TimeWindow,
    project: Option<&str>,
) -> io::Result<Detection> {
    let events = event_index(scanner, window, project)?;
    if events.is_empty() {
        return Ok(Detection::default());
    }

    let mut groups: HashMap<String, Accumulator> = HashMap::new();
    for path in prompt_partitions(scanner, window)? {
        let file = match File::open(&path) {
            Ok(file) => file,
            // A partition can vanish between listing and opening.
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // A torn or future-shaped line is skipped, as everywhere else.
            let Ok(prompt) = serde_json::from_str::<PromptRecord>(&line) else {
                continue;
            };
            // The event index is the filter: a prompt whose event is outside
            // the window or the project simply is not there.
            let Some(placement) = events.get(&prompt.event_id) else {
                continue;
            };
            groups
                .entry(prompt.text_hash.clone())
                .or_insert_with(|| Accumulator::new(&prompt.text_hash))
                .observe(&prompt, placement);
        }
    }

    let repeated = groups
        .into_values()
        .filter(|acc| acc.count >= MIN_OCCURRENCES)
        .map(Accumulator::finish);

    let mut detection = Detection::default();
    for group in repeated {
        if group.text.as_deref().is_some_and(is_furniture) {
            detection.furniture += 1;
        } else {
            detection.groups.push(group);
        }
    }
    detection.groups.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| b.last_ts.cmp(&a.last_ts))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(detection)
}

/// Whether a prompt is something the client wrote into the transcript rather
/// than something a person typed.
///
/// Only exact, structural markers count — a prompt that merely *mentions* one
/// is a real prompt. With `index_prompt_text = false` there is no text to
/// judge, so nothing is filtered and the list is simply noisier.
fn is_furniture(text: &str) -> bool {
    let trimmed = text.trim();
    if FURNITURE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return true;
    }
    // A prompt that is already just a slash command needs no suggestion.
    trimmed.starts_with('/') && !trimmed.contains(char::is_whitespace)
}

/// Where a prompt's event sits: its time, and the project it came from.
#[derive(Debug, Clone)]
struct Placement {
    ts: i64,
    project: Option<String>,
}

/// `event_id → placement`, for every event the window and project filter admit.
fn event_index(
    scanner: &Scanner,
    window: TimeWindow,
    project: Option<&str>,
) -> io::Result<HashMap<String, Placement>> {
    let query = ScanQuery::new(window).with_project(project.map(str::to_string));
    let mut index = HashMap::new();
    scanner.scan_with(&query, |event| {
        index.insert(
            event.id,
            Placement {
                ts: event.ts,
                project: event.project,
            },
        );
    })?;
    Ok(index)
}

/// Prompt partitions overlapping the window, chronologically.
///
/// Prompts are partitioned exactly like events, so this is the event scanner's
/// overlap rule rather than a second copy of it: `--since 7d` opens one or two
/// files rather than the whole of `prompts/`.
fn prompt_partitions(scanner: &Scanner, window: TimeWindow) -> io::Result<Vec<PathBuf>> {
    let found = StorePaths::partitions_in(&scanner.paths().prompts_dir(), window)?;
    Ok(found.into_iter().map(|(_, path)| path).collect())
}

/// Running state for one `text_hash` while scanning.
#[derive(Debug)]
struct Accumulator {
    text_hash: String,
    text: Option<String>,
    count: usize,
    last_ts: i64,
    projects: BTreeSet<String>,
}

impl Accumulator {
    fn new(text_hash: &str) -> Self {
        Self {
            text_hash: text_hash.to_string(),
            text: None,
            count: 0,
            last_ts: i64::MIN,
            projects: BTreeSet::new(),
        }
    }

    fn observe(&mut self, prompt: &PromptRecord, placement: &Placement) {
        self.count += 1;
        self.last_ts = self.last_ts.max(placement.ts);
        if self.text.is_none() {
            // Identical hash, identical text: the first one seen will do.
            self.text.clone_from(&prompt.text);
        }
        if let Some(project) = &placement.project {
            self.projects.insert(project.clone());
        }
    }

    fn finish(self) -> DuplicateGroup {
        let action = action_for(self.text.as_deref());
        DuplicateGroup {
            id: group_id(&self.text_hash),
            text_hash: self.text_hash,
            text: self.text,
            count: self.count,
            last_ts: self.last_ts,
            projects: self.projects.into_iter().collect(),
            action,
        }
    }
}

/// Short single-line prompts are aliases; anything longer, or anything whose
/// text was never stored, gets a skill draft.
fn action_for(text: Option<&str>) -> Action {
    let Some(text) = text else {
        return Action::Skill("repeated-prompt".to_string());
    };
    let trimmed = text.trim();
    if !trimmed.contains('\n') && trimmed.chars().count() <= SLASH_COMMAND_MAX_CHARS {
        Action::SlashCommand
    } else {
        Action::Skill(slug(trimmed))
    }
}

/// A kebab-case skill name from the prompt's first few meaningful words.
fn slug(text: &str) -> String {
    const STOPWORDS: [&str; 12] = [
        "a", "an", "and", "any", "for", "in", "of", "on", "the", "then", "to", "with",
    ];
    const MAX_WORDS: usize = 4;

    let words: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| !word.is_empty() && !STOPWORDS.contains(&word.as_str()))
        .take(MAX_WORDS)
        .collect();

    if words.is_empty() {
        "repeated-prompt".to_string()
    } else {
        words.join("-")
    }
}

/// `2d ago`, `5h ago`, `just now` — coarse on purpose; the point is recency,
/// not precision.
pub fn format_age(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3_600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// The `SKILL.md` a `--draft` prints. Returned as a string: the scope is explicit
/// that warden does not write it anywhere.
pub fn draft(group: &DuplicateGroup, now_ms: i64) -> String {
    let name = group
        .action
        .skill_name()
        .map(str::to_string)
        .unwrap_or_else(|| slug(group.text.as_deref().unwrap_or("")));
    let projects = group.projects_label();
    let age = format_age(now_ms, group.last_ts);
    let body = match &group.text {
        Some(text) => format!(
            "## The prompt\n\nRepeated verbatim {} times; last {age}. Projects: {projects}.\n\n\
             ```\n{}\n```\n\n## Steps\n\n1. Restate the request in your own words.\n2. Do the work \
             the prompt describes.\n3. Report what changed.\n\nReplace the steps above with the \
             procedure you actually follow — warden can see that you repeat this prompt, not what \
             you do about it.\n",
            group.count,
            text.trim()
        ),
        None => format!(
            "## The prompt\n\nRepeated {} times; last {age}. Projects: {projects}.\n\nThe prompt \
             text was not stored (`general.index_prompt_text = false`), so warden can report the \
             repetition but not the wording. Paste the prompt here yourself, then write the \
             procedure below.\n\n## Steps\n\n1. …\n",
            group.count
        ),
    };

    format!(
        "---\nname: {name}\ndescription: Repeated prompt detected by warden ({} occurrences, \
         last {age}).\n---\n\n{body}\n<!-- warden: id {} · text_hash {} · this draft was printed, \
         not written; warden does not create files -->\n",
        group.count, group.id, group.text_hash
    )
}

/// Collapse all whitespace runs to single spaces, so a multi-line prompt still
/// fits on one row.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

#[cfg(test)]
pub(crate) mod testkit {
    use crate::store::{text_hash, Event, PromptRecord, StorePaths, StoreWriter};
    use chrono::{TimeZone, Utc};

    pub fn ms(y: i32, mo: u32, d: u32, h: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    /// A store holding one user event per `(id, ts, project, text)`, with the
    /// matching prompt record. `index_text` mirrors `index_prompt_text`.
    pub fn store(
        prompts: &[(&str, i64, &str, &str)],
        index_text: bool,
    ) -> (tempfile::TempDir, StorePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let mut writer = StoreWriter::open(paths.clone()).unwrap();
        for (id, ts, project, text) in prompts {
            let mut event = Event::new(*id, *ts, "claude-code", "anthropic", "user");
            event.project = Some((*project).to_string());
            writer.append_event(&event).unwrap();
            writer
                .append_prompt(
                    *ts,
                    &PromptRecord {
                        event_id: (*id).to_string(),
                        text: index_text.then(|| (*text).to_string()),
                        text_hash: text_hash(text),
                    },
                )
                .unwrap();
        }
        (dir, paths)
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::store::{text_hash, StorePaths};

    const RUN_TESTS: &str = "run the test suite and fix any failures";
    const LONG: &str = "check every migration file in db/migrate for a missing down() and write \
                        one where it is absent";

    fn fixture(index_text: bool) -> (tempfile::TempDir, StorePaths) {
        store(
            &[
                ("a", ms(2026, 8, 1, 9), "acme-api", RUN_TESTS),
                ("b", ms(2026, 8, 2, 9), "acme-api", RUN_TESTS),
                ("c", ms(2026, 8, 3, 9), "warden", RUN_TESTS),
                ("d", ms(2026, 8, 3, 10), "acme-api", LONG),
                ("e", ms(2026, 8, 3, 11), "acme-api", LONG),
                ("f", ms(2026, 8, 3, 12), "acme-api", "a one-off question"),
            ],
            index_text,
        )
    }

    fn detected(index_text: bool) -> Vec<DuplicateGroup> {
        let (_dir, paths) = fixture(index_text);
        detect(&Scanner::new(paths), TimeWindow::all(), None)
            .unwrap()
            .groups
    }

    #[test]
    fn groups_exact_duplicates_and_ignores_one_offs() {
        let groups = detected(true);
        assert_eq!(groups.len(), 2, "{groups:#?}");

        let top = &groups[0];
        assert_eq!(top.count, 3);
        assert_eq!(top.text.as_deref(), Some(RUN_TESTS));
        assert_eq!(top.last_ts, ms(2026, 8, 3, 9));
        assert_eq!(top.projects, vec!["acme-api", "warden"]);
        assert_eq!(top.action, Action::SlashCommand);

        let second = &groups[1];
        assert_eq!(second.count, 2);
        assert_eq!(
            second.action,
            Action::Skill("check-every-migration-file".into())
        );
    }

    #[test]
    fn detects_duplicates_with_prompt_text_disabled() {
        let groups = detected(false);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].count, 3);
        assert!(groups[0].text.is_none());
        // The user is told why there is no text rather than shown a blank.
        assert_eq!(groups[0].preview(), "(text not indexed)");
        // And the id is the same as it is with text on: it comes from the hash.
        let with_text = detected(true);
        assert_eq!(groups[0].id, with_text[0].id);
    }

    #[test]
    fn ids_are_stable_across_runs_and_derived_from_the_hash() {
        let first = detected(true);
        let second = detected(true);
        let ids: Vec<&str> = first.iter().map(|g| g.id.as_str()).collect();
        let again: Vec<&str> = second.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, again);
        assert_eq!(first[0].id, group_id(&text_hash(RUN_TESTS)));
        assert_eq!(first[0].id.len(), ID_LEN);
        assert!(first[0].text_hash.starts_with(&first[0].id));
    }

    #[test]
    fn honours_the_window_and_the_project_filter() {
        let (_dir, paths) = fixture(true);
        let scanner = Scanner::new(paths);

        // Only the last day: two occurrences of LONG, one of RUN_TESTS.
        let window = TimeWindow::new(ms(2026, 8, 3, 0), ms(2026, 8, 4, 0));
        let groups = detect(&scanner, window, None).unwrap().groups;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].count, 2);

        // Only acme-api: the warden occurrence of RUN_TESTS drops out.
        let groups = detect(&scanner, TimeWindow::all(), Some("acme-api"))
            .unwrap()
            .groups;
        let run_tests = groups
            .iter()
            .find(|g| g.text.as_deref() == Some(RUN_TESTS))
            .unwrap();
        assert_eq!(run_tests.count, 2);
        assert_eq!(run_tests.projects, vec!["acme-api"]);
    }

    #[test]
    fn an_empty_store_suggests_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let scanner = Scanner::new(StorePaths::new(dir.path()));
        let detection = detect(&scanner, TimeWindow::all(), None).unwrap();
        assert_eq!(detection, Detection::default());
    }

    #[test]
    fn client_transcript_furniture_is_set_aside_and_counted() {
        let (_dir, paths) = store(
            &[
                ("a", ms(2026, 8, 1, 9), "acme-api", RUN_TESTS),
                ("b", ms(2026, 8, 1, 10), "acme-api", RUN_TESTS),
                ("c", ms(2026, 8, 1, 11), "acme-api", "/compact"),
                ("d", ms(2026, 8, 1, 12), "acme-api", "/compact"),
                (
                    "e",
                    ms(2026, 8, 1, 13),
                    "acme-api",
                    "<command-name>/clear</command-name>",
                ),
                (
                    "f",
                    ms(2026, 8, 1, 14),
                    "acme-api",
                    "<command-name>/clear</command-name>",
                ),
                (
                    "g",
                    ms(2026, 8, 1, 15),
                    "acme-api",
                    "[Request interrupted by user]",
                ),
                (
                    "h",
                    ms(2026, 8, 1, 16),
                    "acme-api",
                    "[Request interrupted by user]",
                ),
            ],
            true,
        );
        let detection = detect(&Scanner::new(paths), TimeWindow::all(), None).unwrap();
        assert_eq!(detection.groups.len(), 1);
        assert_eq!(detection.groups[0].text.as_deref(), Some(RUN_TESTS));
        assert_eq!(detection.furniture, 3);
    }

    #[test]
    fn a_prompt_that_merely_mentions_a_marker_is_still_a_prompt() {
        assert!(!is_furniture(
            "explain what <command-name> means in the log"
        ));
        assert!(!is_furniture("/simplify the parser and then run the tests"));
        assert!(is_furniture(
            "  <local-command-stdout>ok</local-command-stdout>"
        ));
        assert!(is_furniture("/compact"));
    }

    #[test]
    fn the_projects_cell_is_bounded_but_the_list_is_not() {
        let group = DuplicateGroup {
            id: "abcd1234".into(),
            text_hash: "abcd1234".into(),
            text: None,
            count: 2,
            last_ts: 0,
            projects: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
            action: Action::SlashCommand,
        };
        assert_eq!(group.projects_label(), "a, b, c, +2 more");
        assert_eq!(group.projects.len(), 5, "the full list survives for --json");
    }

    #[test]
    fn a_draft_mentions_the_prompt_the_count_and_that_nothing_was_written() {
        let groups = detected(true);
        let now = ms(2026, 8, 5, 9);
        let text = draft(&groups[1], now);
        assert!(
            text.starts_with("---\nname: check-every-migration-file\n"),
            "{text}"
        );
        assert!(text.contains("Repeated verbatim 2 times"), "{text}");
        assert!(text.contains("1d ago"), "{text}");
        assert!(text.contains(&groups[1].text_hash), "{text}");
        assert!(text.contains("not written"), "{text}");
    }

    #[test]
    fn a_draft_without_text_says_so_instead_of_inventing_a_prompt() {
        let groups = detected(false);
        let text = draft(&groups[0], ms(2026, 8, 5, 9));
        assert!(text.contains("index_prompt_text = false"), "{text}");
        assert!(text.contains("Repeated 3 times"), "{text}");
    }

    #[test]
    fn previews_are_one_line_and_bounded() {
        let group = DuplicateGroup {
            id: "abcd1234".into(),
            text_hash: "abcd1234".into(),
            text: Some(format!("first line\nsecond line {}", "x".repeat(200))),
            count: 2,
            last_ts: 0,
            projects: Vec::new(),
            action: Action::SlashCommand,
        };
        let preview = group.preview();
        assert!(!preview.contains('\n'), "{preview}");
        assert!(preview.chars().count() <= PREVIEW_CHARS + 3, "{preview}");
        assert_eq!(group.projects_label(), "(no project)");
    }

    #[test]
    fn ages_are_coarse_and_never_negative() {
        assert_eq!(format_age(1_000_000, 1_000_000), "just now");
        assert_eq!(format_age(1_000_000, 999_000), "just now");
        assert_eq!(format_age(3_600_000, 0), "1h ago");
        assert_eq!(format_age(90_000, 0), "1m ago");
        assert_eq!(format_age(172_800_000, 0), "2d ago");
        // A clock skew must not produce "-3d ago".
        assert_eq!(format_age(0, 5_000), "just now");
    }

    #[test]
    fn slugs_drop_stopwords_and_punctuation() {
        assert_eq!(slug("check the migration files"), "check-migration-files");
        assert_eq!(slug("run  it!!"), "run-it");
        assert_eq!(slug("...???"), "repeated-prompt");
    }
}
