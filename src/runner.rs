use crate::agent::LogLevel;
use crate::event::{AgentEvent, Event};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::sleep;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const MAX_TOKENS: u32 = 1024;
const SYSTEM_PROMPT: &str =
    "You are an autonomous agent running inside a monitoring dashboard. Carry out \
     the user's task directly and concisely.";

/// Everything a backend needs to execute one agent. A snapshot of the agent's
/// fields, so the background task never has to reach back into `App` state.
pub struct RunSpec {
    pub id: String,
    pub model: String,
    pub task: String,
}

/// A pluggable execution backend. The `spawn` method launches the work on the
/// runtime and returns immediately; the task reports progress by sending
/// `Event::Agent` messages tagged with `spec.id`. There is no async in the
/// trait itself — the async lives inside the spawned task — so no `async_trait`
/// is needed and `App` can hold a plain `Box<dyn Backend>`.
pub trait Backend: Send + Sync {
    fn spawn(&self, spec: RunSpec, tx: UnboundedSender<Event>) -> JoinHandle<()>;
}

// ─── Mock backend ───────────────────────────────────────────────────────────

/// A stand-in for a real LLM backend, kept for offline use and tests. It emits
/// the same event sequence a real backend does — `Started` → logs → streamed
/// `Token`s → `Finished` — with no network or API key. A task whose text
/// contains "fail" (and is at least three words long) takes the error path.
pub struct MockBackend;

impl Backend for MockBackend {
    fn spawn(&self, spec: RunSpec, tx: UnboundedSender<Event>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let id = spec.id.clone();
            let send = |kind| {
                tx.send(Event::Agent {
                    id: id.clone(),
                    kind,
                })
                .is_ok()
            };

            if !send(AgentEvent::Started) {
                return;
            }
            if !send(AgentEvent::Log(
                LogLevel::Info,
                format!("Connecting to {}…", spec.model),
            )) {
                return;
            }
            sleep(Duration::from_millis(300)).await;

            let should_fail = spec.task.to_lowercase().contains("fail");
            let mut emitted = 0u32;
            for word in spec.task.split_whitespace() {
                if !send(AgentEvent::Token(format!("{word} "))) {
                    return;
                }
                emitted += 1;
                if should_fail && emitted >= 3 {
                    send(AgentEvent::Failed(
                        "simulated backend error after 3 tokens".into(),
                    ));
                    return;
                }
                sleep(Duration::from_millis(120)).await;
            }
            send(AgentEvent::Finished {
                tokens_used: emitted,
            });
        })
    }
}

// ─── Anthropic backend ────────────────────────────────────────────────────────

/// Streams completions from the Anthropic Messages API. Output text arrives as
/// `Token` events; the final `output_tokens` count rides along on `Finished`.
pub struct AnthropicBackend {
    client: reqwest::Client,
    api_key: String,
    /// The messages endpoint. A field (not the const directly) so tests can
    /// point it at a local mock server.
    url: String,
}

impl AnthropicBackend {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            url: ANTHROPIC_URL.to_string(),
        }
    }
}

impl Backend for AnthropicBackend {
    fn spawn(&self, spec: RunSpec, tx: UnboundedSender<Event>) -> JoinHandle<()> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url = self.url.clone();
        tokio::spawn(async move {
            let id = spec.id.clone();
            let send = |kind| {
                tx.send(Event::Agent {
                    id: id.clone(),
                    kind,
                })
                .is_ok()
            };

            let model = resolve_model(&spec.model);
            if !send(AgentEvent::Started) {
                return;
            }
            send(AgentEvent::Log(
                LogLevel::Info,
                format!("Calling Anthropic ({model})…"),
            ));

            // A cacheable system block — the variable part is the user task, so
            // the prefix can be reused across runs (prompt caching).
            let body = json!({
                "model": model,
                "max_tokens": MAX_TOKENS,
                "stream": true,
                "system": [{
                    "type": "text",
                    "text": SYSTEM_PROMPT,
                    "cache_control": { "type": "ephemeral" }
                }],
                "messages": [{ "role": "user", "content": spec.task }],
            });

            let resp = client
                .post(url)
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    send(AgentEvent::Failed(format!("request failed: {e}")));
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let detail = resp.text().await.unwrap_or_default();
                send(AgentEvent::Failed(format!(
                    "HTTP {status}: {}",
                    truncate(&detail, 200)
                )));
                return;
            }

            // SSE: events are separated by a blank line. Accumulate raw bytes
            // (chunks split at arbitrary boundaries) and decode only whole
            // events, so a multi-byte char can't be torn across a chunk edge.
            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut tokens = 0u32;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        send(AgentEvent::Failed(format!("stream error: {e}")));
                        return;
                    }
                };
                buf.extend_from_slice(&chunk);

                while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                    let block = String::from_utf8_lossy(&buf[..pos]).into_owned();
                    buf.drain(..pos + 2);

                    match parse_sse(&block) {
                        Some(StreamEvent::ContentBlockDelta { delta }) => {
                            if let Some(text) = delta.text {
                                if !text.is_empty() && !send(AgentEvent::Token(text)) {
                                    return;
                                }
                            }
                        }
                        Some(StreamEvent::MessageDelta { usage: Some(u) }) => {
                            tokens = u.output_tokens;
                        }
                        Some(StreamEvent::Error { error }) => {
                            send(AgentEvent::Failed(error.message));
                            return;
                        }
                        Some(StreamEvent::MessageStop) => {
                            send(AgentEvent::Finished {
                                tokens_used: tokens,
                            });
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // Stream ended without an explicit message_stop.
            send(AgentEvent::Finished {
                tokens_used: tokens,
            });
        })
    }
}

/// Use the agent's model verbatim, falling back to a default when it is blank
/// or the "unknown" placeholder the add-agent form inserts.
fn resolve_model(model: &str) -> String {
    let m = model.trim();
    if m.is_empty() || m == "unknown" {
        DEFAULT_MODEL.to_string()
    } else {
        m.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Pull the JSON payload out of one SSE event block and parse it. The SSE
/// `event:` line is ignored — we dispatch on the JSON's own `type` field.
fn parse_sse(block: &str) -> Option<StreamEvent> {
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim());
        }
    }
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    serde_json::from_str(&data).ok()
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StreamEvent {
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { delta: Delta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        #[serde(default)]
        usage: Option<Usage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "error")]
    Error { error: ApiError },
    /// Catch-all for ping, message_start, content_block_start/stop, etc.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Delta {
    /// Present on `text_delta`; absent for other delta kinds (e.g. thinking).
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_falls_back_for_blank_and_placeholder() {
        assert_eq!(resolve_model(""), DEFAULT_MODEL);
        assert_eq!(resolve_model("  "), DEFAULT_MODEL);
        assert_eq!(resolve_model("unknown"), DEFAULT_MODEL);
        assert_eq!(resolve_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(resolve_model("  gpt-4o  "), "gpt-4o");
    }

    #[test]
    fn parse_sse_extracts_text_delta() {
        let block = "event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
        match parse_sse(block) {
            Some(StreamEvent::ContentBlockDelta { delta }) => {
                assert_eq!(delta.text.as_deref(), Some("Hello"));
            }
            other => panic!("expected a text delta, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_reads_message_stop_and_usage() {
        assert!(matches!(
            parse_sse("data: {\"type\":\"message_stop\"}"),
            Some(StreamEvent::MessageStop)
        ));
        match parse_sse("data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}") {
            Some(StreamEvent::MessageDelta { usage: Some(u) }) => assert_eq!(u.output_tokens, 42),
            other => panic!("expected message_delta with usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_surfaces_errors() {
        let block =
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"slow down\"}}";
        match parse_sse(block) {
            Some(StreamEvent::Error { error }) => assert_eq!(error.message, "slow down"),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_ignores_done_blank_and_unknown_types() {
        assert!(parse_sse("data: [DONE]").is_none());
        assert!(parse_sse("event: ping\n").is_none()); // no data line
        assert!(matches!(
            parse_sse("event: ping\ndata: {\"type\":\"ping\"}"),
            Some(StreamEvent::Other)
        ));
    }

    // ── Integration: the Anthropic backend against a mock HTTP server ─────
    //
    // These exercise the full network path — request, streamed SSE response,
    // event emission — against a local `wiremock` server, so no real API key
    // or external connectivity is needed.

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a backend pointed at a mock server, run one spec, and collect the
    /// emitted `AgentEvent`s (asserting every event carries the spec's id).
    async fn run_against(server: &MockServer, task: &str) -> Vec<AgentEvent> {
        let backend = AnthropicBackend {
            client: reqwest::Client::new(),
            api_key: "test-key".into(),
            url: format!("{}/v1/messages", server.uri()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let spec = RunSpec {
            id: "agent-1".into(),
            model: "claude-sonnet-4-6".into(),
            task: task.into(),
        };
        backend.spawn(spec, tx).await.unwrap();

        let mut events = Vec::new();
        while let Ok(Event::Agent { id, kind }) = rx.try_recv() {
            assert_eq!(id, "agent-1");
            events.push(kind);
        }
        events
    }

    #[tokio::test]
    async fn streams_tokens_and_reports_usage_on_finish() {
        let server = MockServer::start().await;
        let sse = "\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

        // The request must carry the auth + version headers we set.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let events = run_against(&server, "hi").await;

        assert!(matches!(events.first(), Some(AgentEvent::Started)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Log(..))));

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Token(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert_eq!(
            events.last(),
            Some(&AgentEvent::Finished { tokens_used: 7 })
        );
    }

    #[tokio::test]
    async fn http_error_status_becomes_a_failed_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;

        let events = run_against(&server, "hi").await;

        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Failed(m) if m.contains("400"))));
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::Finished { .. })));
    }

    #[tokio::test]
    async fn error_event_mid_stream_becomes_a_failed_event() {
        let server = MockServer::start().await;
        let sse = "\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"slow down\"}}\n\n";

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let events = run_against(&server, "hi").await;

        assert!(events.contains(&AgentEvent::Token("partial".into())));
        assert!(events.contains(&AgentEvent::Failed("slow down".into())));
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::Finished { .. })));
    }
}
