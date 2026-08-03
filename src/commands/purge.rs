//! `warden purge --prompts` (MVP §6).
//!
//! The store is append-only; purge is one of the very few commands that removes
//! anything. MVP §6 therefore asks for two properties, and this module exists to
//! guarantee them:
//!
//! - **Explicit.** Nothing is removed without `--yes` or an interactive `y/N`.
//! - **It says what it removed.** File count, prompt-record count, and bytes are
//!   measured *before* the delete, because afterwards they are unknowable.
//!
//! Exactly one directory is ever removed: `<data-dir>/prompts`. The path is
//! re-derived from [`StorePaths`] and re-checked against the store root, so no
//! argument, config value, or symlink can point the delete elsewhere. `events/`,
//! `state/`, and `config.toml` are never touched.

use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::cli::TimeWindow;
use crate::output::{emit, Cell, Report, Table};
use crate::store::StorePaths;

use super::{doctor::human_bytes, thousands, Env};

/// What a purge removed. Measured before the delete.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PurgeSummary {
    pub dir: PathBuf,
    pub files: usize,
    /// Prompt records, i.e. non-empty JSONL lines.
    pub records: u64,
    pub bytes: u64,
    /// False when the user declined, or when there was nothing there.
    pub removed: bool,
}

/// Run the purge, print what happened, and return it.
pub fn run(env: &Env<'_>, prompts: bool, assume_yes: bool) -> io::Result<PurgeSummary> {
    if !prompts {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nothing to purge: pass --prompts to delete stored prompt text \
             (events/ and state/ are never purged)",
        ));
    }

    let summary = purge_prompts(
        env.paths,
        assume_yes,
        &mut io::stdin().lock(),
        &mut io::stderr(),
        io::stdin().is_terminal(),
    )?;
    emit(&report(&summary, env.window), env.json)?;
    Ok(summary)
}

/// The testable core: confirm, measure, delete.
///
/// `interactive` is passed in rather than probed so a test can exercise both the
/// prompt and the refusal without a terminal.
pub fn purge_prompts<R: BufRead, W: Write>(
    paths: &StorePaths,
    assume_yes: bool,
    input: &mut R,
    prompt_to: &mut W,
    interactive: bool,
) -> io::Result<PurgeSummary> {
    let dir = safe_prompts_dir(paths)?;
    let mut summary = measure(&dir)?;
    summary.dir = dir.clone();

    if !dir.exists() {
        writeln!(
            prompt_to,
            "nothing to purge: {} does not exist",
            dir.display()
        )?;
        return Ok(summary);
    }

    if !assume_yes && !confirm(&summary, input, prompt_to, interactive)? {
        writeln!(prompt_to, "aborted; nothing was removed")?;
        return Ok(summary);
    }

    std::fs::remove_dir_all(&dir)?;
    summary.removed = true;
    Ok(summary)
}

/// Re-derive the one removable path and prove it is the one we mean.
///
/// A store root of `/` or a `prompts` entry that is a symlink is refused
/// outright: this function is the whole safety story for a recursive delete.
fn safe_prompts_dir(paths: &StorePaths) -> io::Result<PathBuf> {
    let root = paths.root();
    let dir = paths.prompts_dir();

    let looks_right = dir.parent() == Some(root)
        && dir.file_name() == Some(std::ffi::OsStr::new("prompts"))
        && root.parent().is_some();
    if !looks_right {
        return Err(refuse(&dir, "it is not <data-dir>/prompts"));
    }

    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(refuse(
            &dir,
            "it is a symlink, so the delete would follow it elsewhere",
        )),
        Ok(meta) if !meta.is_dir() => Err(refuse(&dir, "it is not a directory")),
        _ => Ok(dir),
    }
}

fn refuse(dir: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("refusing to purge {}: {why}", dir.display()),
    )
}

/// Count files, records, and bytes directly under `dir`.
fn measure(dir: &Path) -> io::Result<PurgeSummary> {
    let mut summary = PurgeSummary::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(summary),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        summary.files += 1;
        summary.bytes += entry.metadata()?.len();
        let file = std::fs::File::open(entry.path())?;
        for line in BufReader::new(file).lines() {
            if !line?.trim().is_empty() {
                summary.records += 1;
            }
        }
    }
    Ok(summary)
}

/// `y/N` on a terminal; a hard refusal when there is nobody to ask.
fn confirm<R: BufRead, W: Write>(
    summary: &PurgeSummary,
    input: &mut R,
    out: &mut W,
    interactive: bool,
) -> io::Result<bool> {
    if !interactive {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to purge without confirmation: stdin is not a terminal, so pass --yes",
        ));
    }
    write!(
        out,
        "delete {} ({} files, {} prompt records, {})? this cannot be undone [y/N] ",
        summary.dir.display(),
        summary.files,
        thousands(summary.records),
        human_bytes(summary.bytes),
    )?;
    out.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The receipt, through the same renderer every other command uses.
fn report(summary: &PurgeSummary, window: TimeWindow) -> Report {
    let table = Table::new(["removed", "files", "prompt records", "bytes"]).with_row(vec![
        Cell::text(if summary.removed {
            summary.dir.display().to_string()
        } else {
            "(nothing)".to_string()
        }),
        Cell::Int(i64::try_from(summary.files).unwrap_or(i64::MAX)),
        // Exact, not `5.6k`: a receipt for a deletion has to be countable.
        Cell::text(thousands(summary.records)),
        Cell::text(human_bytes(if summary.removed { summary.bytes } else { 0 })),
    ]);

    let rows = vec![serde_json::json!({
        "removed": summary.removed,
        "dir": summary.dir.display().to_string(),
        "files": summary.files,
        "records": summary.records,
        "bytes": summary.bytes,
    })];

    Report::new("purge", window, table)
        .with_json_rows(rows)
        .with_notes([
            "only prompts/ is removed; events/, state/ and config.toml are untouched".to_string(),
            "prompt text can be kept out of the store in the first place with \
             `index_prompt_text = false` under [general] in config.toml"
                .to_string(),
        ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{text_hash, Event, Partition, PromptRecord, StoreWriter};
    use chrono::{TimeZone, Utc};

    /// A store with events and prompts in two different months, plus the state
    /// file and config that must survive the purge.
    fn store() -> (tempfile::TempDir, StorePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let mut writer = StoreWriter::open(paths.clone()).unwrap();
        for (id, month) in [("a", 7u32), ("b", 8u32)] {
            let ts = Utc
                .with_ymd_and_hms(2026, month, 2, 9, 0, 0)
                .unwrap()
                .timestamp_millis();
            let mut event = Event::new(id, ts, "claude-code", "anthropic", "user");
            event.project = Some("acme".into());
            writer.append_event(&event).unwrap();
            writer
                .append_prompt(
                    ts,
                    &PromptRecord {
                        event_id: id.to_string(),
                        text: Some("hello".into()),
                        text_hash: text_hash("hello"),
                    },
                )
                .unwrap();
        }
        writer
            .append_cursor(&crate::store::IngestCursor {
                path: "/src/a.jsonl".into(),
                mtime: 0,
                offset: 12,
                adapter: "claude-code".into(),
                ts: 0,
            })
            .unwrap();
        std::fs::write(paths.config_file(), "").unwrap();
        (dir, paths)
    }

    fn purge(
        paths: &StorePaths,
        yes: bool,
        answer: &str,
        interactive: bool,
    ) -> io::Result<PurgeSummary> {
        let mut input = answer.as_bytes();
        let mut out = Vec::new();
        purge_prompts(paths, yes, &mut input, &mut out, interactive)
    }

    #[test]
    fn removes_only_prompts_and_reports_what_it_removed() {
        let (_dir, paths) = store();
        let events = paths.event_partition(Partition::new(2026, 8));
        let events_before = std::fs::read(&events).unwrap();

        let summary = purge(&paths, true, "", false).unwrap();
        assert!(summary.removed);
        assert_eq!(summary.files, 2, "two monthly prompt partitions");
        assert_eq!(summary.records, 2);
        assert!(summary.bytes > 0);

        assert!(!paths.prompts_dir().exists());
        assert!(paths.events_dir().exists());
        assert_eq!(std::fs::read(&events).unwrap(), events_before);
        assert!(paths.state_dir().exists());
        assert!(paths.config_file().exists());
    }

    #[test]
    fn declining_the_prompt_removes_nothing() {
        let (_dir, paths) = store();
        let summary = purge(&paths, false, "n\n", true).unwrap();
        assert!(!summary.removed);
        assert!(paths.prompts_dir().exists());

        let summary = purge(&paths, false, "y\n", true).unwrap();
        assert!(summary.removed);
        assert!(!paths.prompts_dir().exists());
    }

    #[test]
    fn refuses_to_purge_unattended_without_yes() {
        let (_dir, paths) = store();
        let err = purge(&paths, false, "", false).unwrap_err();
        assert!(err.to_string().contains("--yes"), "{err}");
        assert!(paths.prompts_dir().exists());
    }

    #[test]
    fn a_missing_prompts_dir_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let summary = purge(&paths, true, "", false).unwrap();
        assert!(!summary.removed);
        assert_eq!(summary.records, 0);
    }

    #[test]
    fn refuses_a_symlinked_prompts_dir() {
        let (_dir, paths) = store();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::remove_dir_all(paths.prompts_dir()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(elsewhere.path(), paths.prompts_dir()).unwrap();

        let err = purge(&paths, true, "", false).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(elsewhere.path().exists(), "the symlink target survives");
    }

    #[test]
    fn refuses_a_root_that_is_not_a_store() {
        let paths = StorePaths::new("/");
        let err = safe_prompts_dir(&paths).unwrap_err();
        assert!(err.to_string().contains("refusing to purge"), "{err}");
    }

    #[test]
    fn the_receipt_names_the_directory_and_the_counts() {
        let summary = PurgeSummary {
            dir: PathBuf::from("/store/prompts"),
            files: 2,
            records: 1_443,
            bytes: 14_300_000,
            removed: true,
        };
        let report = report(&summary, TimeWindow::all());
        assert_eq!(report.json_rows[0]["records"], 1_443);
        assert!(
            report
                .table
                .render(crate::output::Style::plain())
                .contains("1,443"),
            "the record count is exact, not abbreviated"
        );
        assert_eq!(report.json_rows[0]["removed"], true);
        let rendered = report.table.render(crate::output::Style::plain());
        assert!(rendered.contains("/store/prompts"), "{rendered}");
        assert!(rendered.contains("13.6 MB"), "{rendered}");
    }
}
