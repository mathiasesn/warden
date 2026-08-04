//! Append-only writer. Routes each record to its month partition and writes
//! whole lines in a single call, so a concurrent reader never sees half a
//! record except as a torn final line.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::paths::{Partition, StorePaths};
use super::record::{Event, IngestCursor, PromptRecord};

/// Appends events, prompts and ingest cursors to the store.
pub struct StoreWriter {
    paths: StorePaths,
    open: HashMap<PathBuf, File>,
}

impl StoreWriter {
    /// Open the store at `paths`, creating `~/.warden/` and its subdirectories
    /// `0700` if missing.
    pub fn open(paths: StorePaths) -> io::Result<Self> {
        create_dir_private(paths.root())?;
        create_dir_private(&paths.events_dir())?;
        create_dir_private(&paths.prompts_dir())?;
        create_dir_private(&paths.state_dir())?;
        Ok(Self {
            paths,
            open: HashMap::new(),
        })
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    /// Append one event to `events/YYYY-MM.jsonl`, chosen by its UTC month.
    pub fn append_event(&mut self, event: &Event) -> io::Result<Partition> {
        let partition = Partition::for_timestamp(event.ts).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "event {} has an unrepresentable timestamp {}",
                    event.id, event.ts
                ),
            )
        })?;
        let path = self.paths.event_partition(partition);
        self.append_line(&path, event)?;
        Ok(partition)
    }

    /// Append prompt text to the partition matching its event's timestamp.
    pub fn append_prompt(&mut self, ts_ms: i64, prompt: &PromptRecord) -> io::Result<Partition> {
        let partition = Partition::for_timestamp(ts_ms).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "prompt for event {} has an unrepresentable timestamp",
                    prompt.event_id
                ),
            )
        })?;
        let path = self.paths.prompt_partition(partition);
        self.append_line(&path, prompt)?;
        Ok(partition)
    }

    /// Append an ingest cursor to `state/ingest.jsonl`.
    pub fn append_cursor(&mut self, cursor: &IngestCursor) -> io::Result<()> {
        let path = self.paths.ingest_state_file();
        self.append_line(&path, cursor)
    }

    /// Serialize, then write the record and its newline in one `write_all`.
    fn append_line<T: serde::Serialize>(&mut self, path: &Path, record: &T) -> io::Result<()> {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        let file = self.file_for(path)?;
        file.write_all(&line)?;
        file.flush()
    }

    fn file_for(&mut self, path: &Path) -> io::Result<&mut File> {
        if !self.open.contains_key(path) {
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            set_private(&file)?;
            self.open.insert(path.to_path_buf(), file);
        }
        Ok(self.open.get_mut(path).expect("just inserted"))
    }
}

/// Create a directory `0700`, tightening the mode if it already exists.
fn create_dir_private(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Store files hold prompt text; keep them owner-only too.
#[allow(unused_variables)]
fn set_private(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::record::Event;
    use chrono::{TimeZone, Utc};

    fn event_at(id: &str, ts: i64) -> Event {
        Event::new(id, ts, "claude-code", "anthropic", "assistant")
    }

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn routes_events_to_month_partitions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let mut writer = StoreWriter::open(paths.clone()).unwrap();

        // Last millisecond of July and the first of August, UTC.
        let july = ms(2026, 7, 31, 23, 59, 59) + 999;
        writer.append_event(&event_at("a", july)).unwrap();
        writer.append_event(&event_at("b", july + 1)).unwrap();
        writer
            .append_event(&event_at("c", ms(2026, 8, 20, 0, 0, 0)))
            .unwrap();

        let july_lines =
            std::fs::read_to_string(paths.event_partition(Partition::new(2026, 7))).unwrap();
        let aug_lines =
            std::fs::read_to_string(paths.event_partition(Partition::new(2026, 8))).unwrap();
        assert_eq!(july_lines.lines().count(), 1);
        assert_eq!(aug_lines.lines().count(), 2);
        assert!(july_lines.contains("\"id\":\"a\""));
        assert!(aug_lines.contains("\"id\":\"b\""));
    }

    #[test]
    fn appends_rather_than_truncates_across_writers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let ts = ms(2026, 8, 1, 0, 0, 0);

        StoreWriter::open(paths.clone())
            .unwrap()
            .append_event(&event_at("a", ts))
            .unwrap();
        StoreWriter::open(paths.clone())
            .unwrap()
            .append_event(&event_at("b", ts))
            .unwrap();

        let text = std::fs::read_to_string(paths.event_partition(Partition::new(2026, 8))).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn writes_prompts_and_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(dir.path());
        let mut writer = StoreWriter::open(paths.clone()).unwrap();
        let ts = ms(2026, 8, 1, 0, 0, 0);

        writer
            .append_prompt(
                ts,
                &PromptRecord {
                    event_id: "a".into(),
                    text: Some("hello".into()),
                    text_hash: "h".into(),
                },
            )
            .unwrap();
        writer
            .append_cursor(&IngestCursor {
                path: "/logs/x.jsonl".into(),
                mtime: ts,
                offset: 42,
                adapter: "claude-code".into(),
                ts,
            })
            .unwrap();

        let prompts =
            std::fs::read_to_string(paths.prompt_partition(Partition::new(2026, 8))).unwrap();
        assert!(prompts.contains("\"text_hash\":\"h\""));
        let cursors = std::fs::read_to_string(paths.ingest_state_file()).unwrap();
        assert!(cursors.contains("\"offset\":42"));
    }

    #[cfg(unix)]
    #[test]
    fn creates_store_dirs_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("warden-store");
        let paths = StorePaths::new(&root);
        StoreWriter::open(paths.clone()).unwrap();
        for path in [
            paths.root().to_path_buf(),
            paths.events_dir(),
            paths.prompts_dir(),
            paths.state_dir(),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", path.display());
        }
    }
}
