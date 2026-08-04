//! Source adapters and the KPIs they can populate.
//!
//! An adapter turns one line of a vendor log into a normalized [`Event`]. It
//! also *declares* which KPIs it is able to fill in, so reports can grey out a
//! column instead of printing a misleading `0` for something the source simply
//! never recorded.
//!
//! Adapters are strictly read-only against their source tree: they open files
//! for reading and never write, rename, or truncate anything under it.

pub mod claude_code;

use std::io;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::store::Event;

/// A measurable a report might want to show. An adapter that does not list a
/// KPI cannot fill it in from its logs — the column is blank because the data
/// does not exist, not because the number is zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kpi {
    /// `input_tok` / `output_tok`.
    Tokens,
    /// `cache_read_tok` / `cache_write_tok`.
    CacheTokens,
    /// `cost_est`, derived from configured pricing.
    Cost,
    /// Prompt text and `text_hash`.
    Prompts,
    /// `tool_calls`.
    ToolCalls,
    /// `stop_reason`.
    StopReason,
    /// Per-turn wall-clock time.
    DurationMs,
    /// Whether an event came from a subagent transcript.
    Sidechain,
}

impl Kpi {
    /// Every KPI warden knows about, in display order.
    pub const ALL: [Kpi; 8] = [
        Kpi::Tokens,
        Kpi::CacheTokens,
        Kpi::Cost,
        Kpi::Prompts,
        Kpi::ToolCalls,
        Kpi::StopReason,
        Kpi::DurationMs,
        Kpi::Sidechain,
    ];

    /// Short label used by `doctor` and report headers.
    pub fn label(self) -> &'static str {
        match self {
            Kpi::Tokens => "tokens",
            Kpi::CacheTokens => "cache",
            Kpi::Cost => "cost",
            Kpi::Prompts => "prompts",
            Kpi::ToolCalls => "tools",
            Kpi::StopReason => "stop_reason",
            Kpi::DurationMs => "duration_ms",
            Kpi::Sidechain => "sidechain",
        }
    }
}

/// The set of KPIs an adapter declares it can populate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    supported: &'static [Kpi],
}

impl Capabilities {
    pub const fn new(supported: &'static [Kpi]) -> Self {
        Self { supported }
    }

    /// Nothing is supported — used by adapters that are not implemented yet.
    pub const fn none() -> Self {
        Self::new(&[])
    }

    pub fn supports(&self, kpi: Kpi) -> bool {
        self.supported.contains(&kpi)
    }

    pub fn supported(&self) -> &'static [Kpi] {
        self.supported
    }

    /// KPIs this adapter cannot fill in. These are the columns a report must
    /// grey out, and the answer `doctor` gives to "why is this empty?".
    pub fn unsupported(&self) -> Vec<Kpi> {
        Kpi::ALL
            .into_iter()
            .filter(|kpi| !self.supports(*kpi))
            .collect()
    }
}

/// One parsed source record.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRecord {
    pub event: Event,
    /// Prompt text, when the record carries one. Whether it is *stored* is the
    /// ingester's decision (`general.index_prompt_text`).
    pub prompt_text: Option<String>,
    /// Usage on this record is reported per-request and may be repeated
    /// verbatim on sibling records; it must be counted once per key. `None`
    /// means the counts on this record stand on their own.
    pub usage_key: Option<String>,
}

/// What one source line yielded.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// A normalized record.
    Record(Box<ParsedRecord>),
    /// A line the adapter understands and deliberately ignores (a record type
    /// that carries no usage). Not an error, and never counted as unparseable.
    Skipped,
    /// A line the adapter could not make sense of. Counted and reported; it
    /// never fails the run, because the source schema is undocumented and
    /// drifts between versions.
    Unparseable,
}

/// A source of events.
pub trait Adapter {
    /// Stable adapter name, used as the `agent` field and the config key.
    fn name(&self) -> &'static str;

    /// Whether this adapter is implemented at all.
    fn is_implemented(&self) -> bool;

    /// KPIs this adapter can populate.
    fn capabilities(&self) -> Capabilities;

    /// Log root for this adapter: the configured path, else its built-in
    /// default. `None` when no default can be determined.
    fn root(&self, config: &Config) -> Option<PathBuf>;

    /// Transcript files under `root`, in a deterministic order. Read-only.
    fn discover(&self, root: &Path) -> io::Result<Vec<PathBuf>>;

    /// Number of distinct sessions visible under `root`, for `doctor`.
    fn session_count(&self, root: &Path) -> io::Result<usize>;

    /// Parse one line of `source`.
    fn parse_line(&self, source: &Path, line: &str) -> Parsed;
}

/// Key under which a per-request usage figure is counted exactly once.
///
/// Both the adapter (when parsing) and the ingester (when rebuilding state from
/// an existing store) must derive the same string, which is what makes usage
/// attribution survive an interrupted run.
pub fn usage_key(agent: &str, session_id: Option<&str>, turn_id: &str) -> String {
    format!("{agent}\u{1}{}\u{1}{turn_id}", session_id.unwrap_or(""))
}

/// Every adapter warden knows about, implemented or not, so `doctor` can list
/// the ones a user might expect to see.
pub fn registry() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(NotImplementedAdapter {
            name: "codex",
            default_root: "~/.codex/sessions",
        }),
        Box::new(NotImplementedAdapter {
            name: "cursor",
            default_root: "",
        }),
    ]
}

/// Adapters that are enabled in config and can actually ingest.
pub fn enabled(config: &Config) -> Vec<Box<dyn Adapter>> {
    registry()
        .into_iter()
        .filter(|adapter| adapter.is_implemented() && config.source(adapter.name()).enabled)
        .collect()
}

/// A placeholder for a source warden does not read yet. It declares no KPIs, so
/// nothing downstream can mistake it for a source of zeroes.
struct NotImplementedAdapter {
    name: &'static str,
    /// Where its logs are known to live, for `doctor`'s benefit. Empty when
    /// even that is not settled.
    default_root: &'static str,
}

impl Adapter for NotImplementedAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn is_implemented(&self) -> bool {
        false
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    fn root(&self, config: &Config) -> Option<PathBuf> {
        config
            .source(self.name)
            .path
            .or_else(|| (!self.default_root.is_empty()).then(|| PathBuf::from(self.default_root)))
    }

    fn discover(&self, _root: &Path) -> io::Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    fn session_count(&self, _root: &Path) -> io::Result<usize> {
        Ok(0)
    }

    fn parse_line(&self, _source: &Path, _line: &str) -> Parsed {
        Parsed::Skipped
    }
}

/// Collect `*.jsonl` files under `root`, recursively, sorted. Read-only:
/// unreadable subdirectories are skipped rather than failing the walk.
pub(crate) fn jsonl_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(_) if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") => {
                    found.push(path)
                }
                _ => {}
            }
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_every_adapter_a_user_might_expect() {
        let names: Vec<_> = registry().iter().map(|a| a.name()).collect();
        assert_eq!(names, ["claude-code", "codex", "cursor"]);
    }

    #[test]
    fn unimplemented_adapters_declare_no_kpis() {
        for adapter in registry().iter().filter(|a| !a.is_implemented()) {
            assert!(adapter.capabilities().supported().is_empty());
            assert_eq!(adapter.capabilities().unsupported().len(), Kpi::ALL.len());
        }
    }

    #[test]
    fn usage_key_boundaries_are_unambiguous() {
        assert_ne!(
            usage_key("a", Some("b"), "c"),
            usage_key("a", Some("bc"), "")
        );
        assert_eq!(usage_key("a", None, "c"), usage_key("a", Some(""), "c"));
    }

    #[test]
    fn jsonl_walk_is_recursive_sorted_and_extension_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("proj/deeper");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("b.jsonl"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(nested.join("a.jsonl"), "").unwrap();

        let found = jsonl_files(dir.path()).unwrap();
        assert_eq!(
            found,
            vec![dir.path().join("b.jsonl"), nested.join("a.jsonl")]
        );
    }
}
