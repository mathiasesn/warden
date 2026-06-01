use crate::agent::LogLevel;
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent};
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Everything the main loop can react to: terminal input, timer ticks, and
/// events emitted by background agent runs. Folding them onto one channel is
/// the whole reason input no longer blocks the loop directly.
pub enum Event {
    Input(KeyEvent),
    Tick,
    /// An update from a running agent, addressed by agent **id** (not index —
    /// the list can change while a run is in flight).
    Agent {
        id: String,
        kind: AgentEvent,
    },
}

/// A single update from a runner. The runner reports lifecycle and output;
/// `App` owns the resulting status transitions (see `apply_agent_event`).
#[derive(Debug, PartialEq)]
pub enum AgentEvent {
    /// The run has begun.
    Started,
    /// A structured log line.
    Log(LogLevel, String),
    /// A chunk of streamed output text.
    Token(String),
    /// The run completed successfully.
    Finished { tokens_used: u32 },
    /// The run ended in error.
    Failed(String),
}

/// Forward terminal key events onto the channel until the receiver is gone.
/// Mirrors the old `event::read()` filter — only `Key` events are surfaced.
pub fn spawn_input(tx: UnboundedSender<Event>) {
    tokio::spawn(async move {
        let mut reader = EventStream::new();
        while let Some(Ok(event)) = reader.next().await {
            if let CrosstermEvent::Key(key) = event {
                if tx.send(Event::Input(key)).is_err() {
                    break;
                }
            }
        }
    });
}

/// Emit a `Tick` at a fixed cadence so time-based UI (spinners, live token
/// streams) can redraw while the user is idle. Harmless today — no animation
/// depends on it yet — but the cadence is in place for agent execution.
pub fn spawn_ticker(tx: UnboundedSender<Event>, tick_rate: Duration) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick_rate);
        loop {
            interval.tick().await;
            if tx.send(Event::Tick).is_err() {
                break;
            }
        }
    });
}
