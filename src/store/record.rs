//! Normalized record shapes (MVP §2.2, §2.3).
//!
//! Fields an adapter cannot populate are `Option` and serialize as absent, so a
//! reader can tell "not supported" from "genuinely zero".

use serde::{Deserialize, Serialize};

/// Current record schema version, written on every event line.
pub const RECORD_VERSION: u32 = 1;

/// One normalized event. Unknown fields are tolerated on read so newer stores
/// can be consumed by older binaries (MVP §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Record schema version, per line.
    pub v: u32,
    /// Content-derived id; identical content always yields the same id.
    pub id: String,
    /// Event time, epoch milliseconds UTC. Decides the month partition.
    pub ts: i64,
    /// Source agent, e.g. `claude-code`.
    pub agent: String,
    /// Model provider, e.g. `anthropic`.
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// `assistant`, `user`, ...
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tok: Option<u64>,
    /// Unsupported by adapters that do not log it — absent, never `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Estimated cost; absent when the model has no configured price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_est: Option<f64>,
    /// Nested rather than a separate file, to avoid a join on every scan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Whether this event came from a subagent/sidechain transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_sidechain: Option<bool>,
}

impl Event {
    /// A minimally populated event at the current record version.
    pub fn new(id: impl Into<String>, ts: i64, agent: &str, provider: &str, role: &str) -> Self {
        Self {
            v: RECORD_VERSION,
            id: id.into(),
            ts,
            agent: agent.to_string(),
            provider: provider.to_string(),
            model: None,
            project: None,
            session_id: None,
            turn_id: None,
            role: role.to_string(),
            input_tok: None,
            output_tok: None,
            cache_read_tok: None,
            cache_write_tok: None,
            duration_ms: None,
            stop_reason: None,
            cost_est: None,
            tool_calls: Vec::new(),
            is_sidechain: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_name: String,
    /// File path or other target the tool acted on, when one is discernible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_target: Option<String>,
}

impl ToolCall {
    pub fn new(tool_name: impl Into<String>, tool_target: Option<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_target,
        }
    }
}

/// Prompt text, kept out of `events/` so it can be disabled independently.
/// `text_hash` is always written so dedup works with text off (MVP §2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRecord {
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub text_hash: String,
}

/// Per-source ingest cursor. Append-only, last record for a `path` wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestCursor {
    /// Source file the cursor refers to.
    pub path: String,
    /// Source mtime, epoch milliseconds.
    pub mtime: i64,
    /// Byte offset consumed so far.
    pub offset: u64,
    /// Adapter that produced it.
    pub adapter: String,
    /// When the cursor was written, epoch milliseconds.
    pub ts: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_fields_serialize_as_absent_not_zero() {
        let event = Event::new("abc", 1_754_300_000_000, "claude-code", "anthropic", "user");
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("duration_ms"), "{json}");
        assert!(!json.contains("cost_est"), "{json}");
        assert!(!json.contains("tool_calls"), "{json}");
        assert!(json.contains("\"v\":1"));
    }

    #[test]
    fn tolerates_unknown_fields_and_missing_optionals() {
        let line = r#"{"v":1,"id":"x","ts":5,"agent":"claude-code","provider":"anthropic",
            "role":"assistant","future_field":{"nested":true}}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        assert_eq!(event.id, "x");
        assert_eq!(event.input_tok, None);
        assert!(event.tool_calls.is_empty());
    }

    #[test]
    fn round_trips_a_fully_populated_event() {
        let mut event = Event::new("id", 1, "claude-code", "anthropic", "assistant");
        event.model = Some("claude-sonnet-4-6".into());
        event.input_tok = Some(412);
        event.cache_write_tok = Some(0);
        event.cost_est = Some(0.0412);
        event.is_sidechain = Some(true);
        event.tool_calls = vec![ToolCall::new("Read", Some("src/lib.rs".into()))];
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        // An explicit zero survives; it means "measured zero".
        assert!(json.contains("\"cache_write_tok\":0"));
    }
}
