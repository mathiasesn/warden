//! `warden suggest`.
//!
//! Two modes over one detector: a list of repeated prompts, and the `SKILL.md`
//! draft for one of them. The draft goes to **stdout and nowhere else** — warden
//! is explicit that writing files waits for v0.2, so this module opens no file
//! for writing and creates no directory.

use std::io::{self, Write};

use chrono::Utc;

use crate::output::{emit, Cell, Report, Table};
use crate::store::Scanner;
use crate::suggest::{self, Detection, DuplicateGroup, MIN_OCCURRENCES};

use super::Env;

/// Rows the table shows before it stops being readable. `--json` is never
/// truncated: a harness wants the whole list.
const TABLE_ROWS: usize = 20;

/// What a run produced, so callers (and tests) can assert on it.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The list of repeated prompts.
    Listed(Report),
    /// A `SKILL.md`, exactly as printed.
    Drafted(String),
}

/// List repeated prompts, or print one draft.
pub fn run(env: &Env<'_>, draft_id: Option<&str>) -> io::Result<Outcome> {
    env.pre_ingest()?;
    let detection = suggest::detect(&Scanner::new(env.paths.clone()), env.window, env.project)?;
    let groups = &detection.groups;
    let now = Utc::now().timestamp_millis();

    match draft_id {
        Some(id) => {
            let group = find(groups, id)?;
            let text = suggest::draft(group, now);
            let mut stdout = io::stdout().lock();
            write!(stdout, "{text}")?;
            stdout.flush()?;
            Ok(Outcome::Drafted(text))
        }
        None => {
            let report = build(&detection, env.window, now);
            if !env.json {
                headline(&mut io::stdout().lock(), groups.len())?;
            }
            emit(&report, env.json)?;
            Ok(Outcome::Listed(report))
        }
    }
}

/// Resolve a `--draft` argument against the detected groups.
///
/// Accepts the short id or any unambiguous prefix of the full `text_hash`, and
/// says what the valid ids are when it cannot.
fn find<'a>(groups: &'a [DuplicateGroup], id: &str) -> io::Result<&'a DuplicateGroup> {
    let id = id.trim();
    let matches: Vec<&DuplicateGroup> = groups
        .iter()
        .filter(|group| group.id == id || group.text_hash.starts_with(id))
        .collect();

    match matches.as_slice() {
        [group] => Ok(group),
        [] => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "no repeated prompt with id {id:?} in this window{}\nrun `warden suggest` to see \
                 the ids, or widen --since",
                known_ids(groups)
            ),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "id {id:?} is ambiguous; use the full 8-character id{}",
                known_ids(groups)
            ),
        )),
    }
}

fn known_ids(groups: &[DuplicateGroup]) -> String {
    if groups.is_empty() {
        return String::new();
    }
    let ids: Vec<&str> = groups.iter().map(|group| group.id.as_str()).collect();
    format!(" (known ids: {})", ids.join(", "))
}

/// `3 repeated prompts found`. Table mode only: `--json` must stay one document.
fn headline<W: Write>(out: &mut W, count: usize) -> io::Result<()> {
    if count == 0 {
        return writeln!(
            out,
            "no repeated prompts found (a prompt must appear at least {MIN_OCCURRENCES}x)\n"
        );
    }
    let plural = if count == 1 { "prompt" } else { "prompts" };
    let capped = if count > TABLE_ROWS {
        format!(", showing the {TABLE_ROWS} most repeated — --json has them all")
    } else {
        String::new()
    };
    writeln!(out, "{count} repeated {plural} found{capped}\n")
}

fn build(detection: &Detection, window: crate::cli::TimeWindow, now: i64) -> Report {
    let groups = &detection.groups;
    let mut table = Table::new(["id", "count", "last", "projects", "suggestion", "prompt"]);
    let mut rows = Vec::new();
    let mut any_unindexed = false;

    for (rank, group) in groups.iter().enumerate() {
        any_unindexed |= group.text.is_none();
        if rank < TABLE_ROWS {
            table.push(vec![
                Cell::text(&group.id),
                Cell::Int(i64::try_from(group.count).unwrap_or(i64::MAX)),
                Cell::text(suggest::format_age(now, group.last_ts)),
                Cell::text(group.projects_label()),
                Cell::text(format!("→ {}", group.action.label())),
                Cell::text(group.preview()),
            ]);
        }

        rows.push(serde_json::json!({
            "id": group.id,
            "text_hash": group.text_hash,
            "count": group.count,
            "text": group.text,
            "text_indexed": group.text.is_some(),
            "last_ts": group.last_ts,
            "last_age": suggest::format_age(now, group.last_ts),
            "projects": group.projects,
            "suggestion": {
                "kind": group.action.kind(),
                "skill_name": group.action.skill_name(),
            },
        }));
    }

    let mut notes = vec![
        format!(
            "prompts are grouped by exact text hash — no fuzzy matching, so every group is a \
             byte-identical repeat of at least {MIN_OCCURRENCES} occurrences"
        ),
        "a short single-line prompt is suggested as a slash command; a longer one gets a skill \
         draft — run `warden suggest --draft <id>` to print it"
            .to_string(),
    ];
    if any_unindexed {
        notes.push(
            "some prompts show no text: `general.index_prompt_text = false` stores only the hash, \
             which still detects repeats but cannot show the wording"
                .to_string(),
        );
    }
    if detection.furniture > 0 {
        notes.push(format!(
            "{} repeated groups were set aside as client transcript furniture (slash-command \
             expansions, compaction notices, bash echoes, interrupt markers) — repeated, but not \
             prompts anyone typed",
            detection.furniture
        ));
    }
    if groups.len() > TABLE_ROWS {
        notes.push(format!(
            "the table shows the {TABLE_ROWS} most repeated of {}; these JSON rows are complete",
            groups.len()
        ));
    }
    if groups.is_empty() {
        notes.push(
            "nothing repeated in this window; widen --since, or check `warden doctor` if \
             prompts are not being ingested at all"
                .to_string(),
        );
    }

    Report::new("suggest", window, table)
        .with_json_rows(rows)
        .with_notes(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::TimeWindow;
    use crate::config::Config;
    use crate::output::Style;
    use crate::store::StorePaths;
    use crate::suggest::testkit::{ms, store};

    const RUN_TESTS: &str = "run the test suite and fix any failures";

    fn env<'a>(paths: &'a StorePaths, config: &'a Config) -> Env<'a> {
        Env {
            config,
            paths,
            window: TimeWindow::all(),
            project: None,
            json: false,
            no_ingest: true,
            include_sidechain: true,
        }
    }

    fn fixture(index_text: bool) -> (tempfile::TempDir, StorePaths) {
        store(
            &[
                ("a", ms(2026, 8, 1, 9), "acme-api", RUN_TESTS),
                ("b", ms(2026, 8, 2, 9), "acme-api", RUN_TESTS),
                ("c", ms(2026, 8, 3, 9), "acme-api", "a one-off"),
            ],
            index_text,
        )
    }

    fn detection(index_text: bool) -> Detection {
        let (_dir, paths) = fixture(index_text);
        suggest::detect(&Scanner::new(paths), TimeWindow::all(), None).unwrap()
    }

    #[test]
    fn lists_repeated_prompts_as_a_table_and_as_json() {
        let (_dir, paths) = fixture(true);
        let config = Config::default();
        let Outcome::Listed(report) = run(&env(&paths, &config), None).unwrap() else {
            panic!("expected a listing");
        };
        assert_eq!(report.name, "suggest");
        assert_eq!(report.json_rows.len(), 1);
        assert_eq!(report.json_rows[0]["count"], 2);
        assert_eq!(report.json_rows[0]["text"], RUN_TESTS);
        assert_eq!(report.json_rows[0]["suggestion"]["kind"], "slash-command");

        let rendered = report.table.render(Style::plain());
        assert!(rendered.starts_with("ID"), "{rendered}");
        assert!(rendered.contains("save as a slash command"), "{rendered}");
        assert!(rendered.contains("acme-api"), "{rendered}");

        let envelope = serde_json::to_value(report.envelope()).unwrap();
        assert_eq!(envelope["report"], "suggest");
        assert!(envelope["rows"].is_array());
    }

    #[test]
    fn json_rows_say_when_text_was_never_stored() {
        let report = build(&detection(false), TimeWindow::all(), ms(2026, 8, 4, 9));
        assert!(report.json_rows[0]["text"].is_null());
        assert_eq!(report.json_rows[0]["text_indexed"], false);
        assert!(report.notes.iter().any(|n| n.contains("index_prompt_text")));
        assert!(report
            .table
            .render(Style::plain())
            .contains("(text not indexed)"));
    }

    #[test]
    fn a_draft_is_addressed_by_the_short_id() {
        let groups = detection(true).groups;
        let (_dir, paths) = fixture(true);
        let config = Config::default();
        let Outcome::Drafted(text) = run(&env(&paths, &config), Some(&groups[0].id)).unwrap()
        else {
            panic!("expected a draft");
        };
        assert!(text.starts_with("---\n"), "{text}");
        assert!(text.contains(RUN_TESTS), "{text}");
    }

    #[test]
    fn a_draft_writes_no_files() {
        let (dir, paths) = fixture(true);
        let before = snapshot(dir.path());
        let groups = suggest::detect(&Scanner::new(paths.clone()), TimeWindow::all(), None)
            .unwrap()
            .groups;
        let config = Config::default();
        run(&env(&paths, &config), Some(&groups[0].id)).unwrap();
        assert_eq!(snapshot(dir.path()), before, "--draft must not touch disk");
    }

    #[test]
    fn an_unknown_draft_id_errors_helpfully() {
        let (_dir, paths) = fixture(true);
        let config = Config::default();
        let err = run(&env(&paths, &config), Some("deadbeef")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("known ids"), "{err}");
    }

    #[test]
    fn a_full_hash_and_an_unambiguous_prefix_both_resolve() {
        let groups = detection(true).groups;
        assert_eq!(
            find(&groups, &groups[0].text_hash).unwrap().id,
            groups[0].id
        );
        assert_eq!(find(&groups, &groups[0].id[..4]).unwrap().id, groups[0].id);
    }

    #[test]
    fn the_headline_counts_and_pluralises() {
        let render = |n| {
            let mut buf = Vec::new();
            headline(&mut buf, n).unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert!(render(0).starts_with("no repeated prompts found"));
        assert!(render(1).starts_with("1 repeated prompt found"));
        assert!(render(3).starts_with("3 repeated prompts found"));
    }

    /// Every path under `root`, with its size — enough to catch a stray write.
    fn snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(entry.path());
                }
                out.push((entry.path(), meta.len()));
            }
        }
        out.sort();
        out
    }
}
