//! The Claude Code adapter: `~/.claude/projects/**/*.jsonl`.
//!
//! The transcript schema is undocumented and drifts between releases, so this
//! parser is deliberately tolerant: it reads through `serde_json::Value`,
//! ignores unknown keys, skips the record types that carry no usage, and counts
//! rather than fails on a line it cannot read.
//!
//! `~/.claude/projects` is also where Claude Code keeps its own memory. Nothing
//! here opens a file for anything but reading.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use chrono::DateTime;
use serde_json::Value;

use super::{usage_key, Adapter, Capabilities, Kpi, Parsed, ParsedRecord};
use crate::config::Config;
use crate::store::{event_id, Event, EventIdentity, ToolCall};

/// Adapter name, and the `agent` field on every event it produces.
pub const AGENT: &str = "claude-code";
/// Every model in these transcripts is an Anthropic one.
pub const PROVIDER: &str = "anthropic";
/// `~/.claude/projects`, unless config overrides it.
pub const DEFAULT_ROOT: &str = "~/.claude/projects";

/// `duration_ms` is deliberately absent: Claude Code does not log per-turn wall
/// clock time, so warden reports the column as unsupported rather than `0`.
const CAPABILITIES: Capabilities = Capabilities::new(&[
    Kpi::Tokens,
    Kpi::CacheTokens,
    Kpi::Cost,
    Kpi::Prompts,
    Kpi::ToolCalls,
    Kpi::StopReason,
    Kpi::Sidechain,
]);

/// Record types that carry usage or a prompt. Every other `type` in these files
/// (`attachment`, `mode`, `file-history-snapshot`, ...) is a UI or bookkeeping
/// record and is skipped, not counted as unparseable.
const INGESTED_TYPES: [&str; 2] = ["assistant", "user"];

/// Keys on a `tool_use` input that name what the tool acted on. Free-form
/// arguments (a shell command, a patch body) are deliberately not treated as
/// targets — attribution only makes sense for a path-like value.
const TARGET_KEYS: [&str; 6] = [
    "file_path",
    "notebook_path",
    "path",
    "pattern",
    "url",
    "file",
];

pub struct ClaudeCodeAdapter;

impl Adapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        AGENT
    }

    fn is_implemented(&self) -> bool {
        true
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn root(&self, config: &Config) -> Option<PathBuf> {
        Some(
            config
                .source(AGENT)
                .path
                .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT)),
        )
    }

    fn discover(&self, root: &Path) -> io::Result<Vec<PathBuf>> {
        super::jsonl_files(root)
    }

    /// One transcript file per session; the file stem is the session id, so
    /// this needs no reads.
    fn session_count(&self, root: &Path) -> io::Result<usize> {
        let sessions: BTreeSet<String> = self
            .discover(root)?
            .iter()
            .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
            .collect();
        Ok(sessions.len())
    }

    fn parse_line(&self, _source: &Path, line: &str) -> Parsed {
        parse_line(line)
    }
}

fn parse_line(line: &str) -> Parsed {
    if line.trim().is_empty() {
        return Parsed::Skipped;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Parsed::Unparseable;
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Parsed::Unparseable;
    };
    if !INGESTED_TYPES.contains(&kind) {
        return Parsed::Skipped;
    }

    // A record of a type we do ingest but cannot place in time is unusable;
    // that is schema drift, and worth surfacing as such.
    let Some(ts) = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(ts_ms)
    else {
        return Parsed::Unparseable;
    };

    let session_id = string(&value, "sessionId");
    let source_id = value.get("uuid").and_then(Value::as_str);
    let turn_id = string(&value, "requestId");

    let id = event_id(EventIdentity {
        agent: AGENT,
        session_id: session_id.as_deref(),
        source_id,
        ts,
        role: kind,
        extra: None,
    });

    let mut event = Event::new(id, ts, AGENT, PROVIDER, kind);
    event.session_id = session_id.clone();
    event.turn_id = turn_id.clone();
    event.project = value
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(project_from_cwd);
    event.is_sidechain = value.get("isSidechain").and_then(Value::as_bool);

    let message = value.get("message");
    event.model = message.and_then(|m| string(m, "model"));
    event.stop_reason = message.and_then(|m| string(m, "stop_reason"));
    event.tool_calls = tool_calls(message);
    // `duration_ms` stays None: the source does not record it.

    let mut usage_dedup_key = None;
    if kind == "assistant" {
        if let Some(usage) = message.and_then(|m| m.get("usage")) {
            event.input_tok = count(usage, "input_tokens");
            event.output_tok = count(usage, "output_tokens");
            event.cache_read_tok = count(usage, "cache_read_input_tokens");
            event.cache_write_tok = count(usage, "cache_creation_input_tokens");
            // Claude Code repeats one request's usage verbatim on every
            // assistant record it produced (a thinking block and a tool_use
            // block share a `requestId` and a usage object). Counting each
            // would roughly double every token figure, so the counts are
            // attributed to the request, once.
            usage_dedup_key = turn_id
                .as_deref()
                .map(|turn| usage_key(AGENT, session_id.as_deref(), turn));
        }
    }

    let prompt_text = if kind == "user" {
        message.and_then(|m| m.get("content")).and_then(prompt_text)
    } else {
        None
    };

    Parsed::Record(Box::new(ParsedRecord {
        event,
        prompt_text,
        usage_key: usage_dedup_key,
    }))
}

/// ISO-8601 to epoch milliseconds.
fn ts_ms(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Project is the basename of `cwd`. A worktree suffix is part of that basename
/// and is preserved as-is, so two worktrees of one repo stay distinguishable.
fn project_from_cwd(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn count(usage: &Value, key: &str) -> Option<u64> {
    usage.get(key).and_then(Value::as_u64)
}

fn tool_calls(message: Option<&Value>) -> Vec<ToolCall> {
    let Some(blocks) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| {
            let name = block.get("name").and_then(Value::as_str)?;
            Some(ToolCall::new(name, tool_target(block.get("input"))))
        })
        .collect()
}

fn tool_target(input: Option<&Value>) -> Option<String> {
    let input = input?;
    TARGET_KEYS
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .filter(|target| !target.is_empty())
        .map(str::to_string)
}

/// The human-authored part of a user record. `content` is either a bare string
/// or a block list; tool results are not prompts and yield nothing.
fn prompt_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(line: &str) -> ParsedRecord {
        match parse_line(line) {
            Parsed::Record(record) => *record,
            other => panic!("expected a record, got {other:?}"),
        }
    }

    const ASSISTANT: &str = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-08-01T10:00:00.000Z",
      "sessionId":"s1","requestId":"req_1","cwd":"/home/me/code/acme-api","isSidechain":false,
      "message":{"model":"claude-sonnet-4-6","stop_reason":"tool_use",
        "usage":{"input_tokens":2,"output_tokens":295,"cache_read_input_tokens":18715,
                 "cache_creation_input_tokens":18472,"service_tier":"standard"},
        "content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs","limit":10}}]}}"#;

    #[test]
    fn maps_an_assistant_record_field_for_field() {
        let parsed = record(ASSISTANT);
        let event = &parsed.event;
        assert_eq!(event.agent, "claude-code");
        assert_eq!(event.provider, "anthropic");
        assert_eq!(event.role, "assistant");
        assert_eq!(event.ts, 1_785_578_400_000);
        assert_eq!(event.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(event.session_id.as_deref(), Some("s1"));
        assert_eq!(event.turn_id.as_deref(), Some("req_1"));
        assert_eq!(event.project.as_deref(), Some("acme-api"));
        assert_eq!(event.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(event.input_tok, Some(2));
        assert_eq!(event.output_tok, Some(295));
        assert_eq!(event.cache_read_tok, Some(18715));
        assert_eq!(event.cache_write_tok, Some(18472));
        assert_eq!(event.is_sidechain, Some(false));
        assert_eq!(
            event.tool_calls,
            vec![ToolCall::new("Read", Some("src/lib.rs".into()))]
        );
        assert_eq!(
            parsed.usage_key.as_deref(),
            Some("claude-code\u{1}s1\u{1}req_1")
        );
        assert_eq!(parsed.prompt_text, None);
    }

    #[test]
    fn duration_is_never_invented() {
        assert_eq!(record(ASSISTANT).event.duration_ms, None);
        assert!(!CAPABILITIES.supports(Kpi::DurationMs));
        assert!(CAPABILITIES.unsupported().contains(&Kpi::DurationMs));
    }

    #[test]
    fn sibling_records_of_one_request_share_a_usage_key() {
        let sibling = ASSISTANT.replace("\"uuid\":\"u1\"", "\"uuid\":\"u2\"");
        assert_ne!(record(&sibling).event.id, record(ASSISTANT).event.id);
        assert_eq!(record(&sibling).usage_key, record(ASSISTANT).usage_key);
    }

    #[test]
    fn user_records_yield_prompt_text_in_both_content_shapes() {
        let bare = r#"{"type":"user","uuid":"u2","timestamp":"2026-08-01T10:00:01Z","sessionId":"s1",
          "cwd":"/home/me/acme-api","message":{"role":"user","content":"run the tests"}}"#;
        let parsed = record(bare);
        assert_eq!(parsed.prompt_text.as_deref(), Some("run the tests"));
        assert_eq!(parsed.event.role, "user");
        assert_eq!(parsed.event.input_tok, None, "user records carry no usage");
        assert_eq!(parsed.usage_key, None);

        let blocks = r#"{"type":"user","uuid":"u3","timestamp":"2026-08-01T10:00:02Z",
          "message":{"content":[{"type":"text","text":"fix it"}]}}"#;
        assert_eq!(record(blocks).prompt_text.as_deref(), Some("fix it"));
    }

    #[test]
    fn tool_results_are_not_prompts() {
        let line = r#"{"type":"user","uuid":"u4","timestamp":"2026-08-01T10:00:03Z",
          "message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(record(line).prompt_text, None);
    }

    #[test]
    fn sidechain_records_are_ingested_and_marked() {
        let line = ASSISTANT.replace("\"isSidechain\":false", "\"isSidechain\":true");
        assert_eq!(record(&line).event.is_sidechain, Some(true));
    }

    #[test]
    fn worktree_suffix_is_preserved_as_is() {
        let line = ASSISTANT.replace("/home/me/code/acme-api", "/home/me/code/acme-api--wt-x");
        assert_eq!(
            record(&line).event.project.as_deref(),
            Some("acme-api--wt-x")
        );
    }

    #[test]
    fn unknown_record_types_are_skipped_not_counted_as_unparseable() {
        for kind in ["attachment", "mode", "file-history-snapshot", "system"] {
            let line = format!(r#"{{"type":"{kind}","uuid":"x"}}"#);
            assert_eq!(parse_line(&line), Parsed::Skipped, "{kind}");
        }
        assert_eq!(parse_line("   "), Parsed::Skipped);
    }

    #[test]
    fn malformed_lines_are_counted_not_fatal() {
        assert_eq!(parse_line("{not json"), Parsed::Unparseable);
        assert_eq!(parse_line("{\"no\":\"type\"}"), Parsed::Unparseable);
        // A known type we cannot place in time is drift worth reporting.
        assert_eq!(parse_line(r#"{"type":"assistant"}"#), Parsed::Unparseable);
    }

    #[test]
    fn tool_target_is_absent_when_the_input_is_not_path_like() {
        let line = ASSISTANT.replace(
            r#"{"file_path":"src/lib.rs","limit":10}"#,
            r#"{"command":"cargo test"}"#,
        );
        assert_eq!(record(&line).event.tool_calls[0].tool_target, None);
    }

    #[test]
    fn missing_optional_fields_stay_absent() {
        let line = r#"{"type":"assistant","uuid":"u9","timestamp":"2026-08-01T10:00:00Z"}"#;
        let event = record(line).event;
        assert_eq!(event.model, None);
        assert_eq!(event.project, None);
        assert_eq!(event.session_id, None);
        assert_eq!(event.input_tok, None);
        assert!(event.tool_calls.is_empty());
    }
}
