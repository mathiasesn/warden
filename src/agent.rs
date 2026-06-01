use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn gen_id() -> String {
    // Timestamp plus a per-process counter so IDs stay unique even when several
    // agents are created within the same nanosecond.
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:08x}{:04x}", t.as_secs(), t.subsec_nanos(), n)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentStatus {
    Running,
    Idle,
    Error,
    Completed,
}

impl AgentStatus {
    pub fn label(&self) -> &'static str {
        match self {
            AgentStatus::Running => "RUNNING",
            AgentStatus::Idle => "IDLE",
            AgentStatus::Error => "ERROR",
            AgentStatus::Completed => "DONE",
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            AgentStatus::Running => "◉",
            AgentStatus::Idle => "◌",
            AgentStatus::Error => "✗",
            AgentStatus::Completed => "✓",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
    Debug,
}

impl LogLevel {
    pub fn label(&self) -> &'static str {
        match self {
            LogLevel::Info => "INFO ",
            LogLevel::Warning => "WARN ",
            LogLevel::Error => "ERROR",
            LogLevel::Debug => "DEBUG",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub message: String,
}

impl LogEntry {
    pub fn new(level: LogLevel, message: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub model: String,
    pub task: String,
    pub status: AgentStatus,
    pub logs: Vec<LogEntry>,
    pub created_at: DateTime<Utc>,
    /// Tokens reported by the most recent run. `default` so saves written
    /// before this field existed still load.
    #[serde(default)]
    pub tokens_used: u32,
    /// In-flight streamed output while a run is active. Never persisted; on
    /// completion it is flushed into a log entry.
    #[serde(skip)]
    pub partial: String,
}

impl Agent {
    pub fn new(name: impl Into<String>, model: impl Into<String>, task: impl Into<String>) -> Self {
        let name = name.into();
        let task = task.into();
        let model = model.into();
        let mut agent = Self {
            id: gen_id(),
            name: name.clone(),
            model,
            task,
            status: AgentStatus::Idle,
            logs: Vec::new(),
            created_at: Utc::now(),
            tokens_used: 0,
            partial: String::new(),
        };
        agent.logs.push(LogEntry::new(
            LogLevel::Info,
            format!("Agent '{}' initialized.", name),
        ));
        agent
    }

    pub fn add_log(&mut self, level: LogLevel, message: impl Into<String>) {
        self.logs.push(LogEntry::new(level, message));
    }

    pub fn set_status(&mut self, status: AgentStatus) {
        let label = status.label().to_string();
        self.status = status;
        self.logs
            .push(LogEntry::new(LogLevel::Info, format!("Status → {}", label)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_and_symbol_cover_all_variants() {
        assert_eq!(AgentStatus::Running.label(), "RUNNING");
        assert_eq!(AgentStatus::Idle.label(), "IDLE");
        assert_eq!(AgentStatus::Error.label(), "ERROR");
        assert_eq!(AgentStatus::Completed.label(), "DONE");

        assert_eq!(AgentStatus::Running.symbol(), "◉");
        assert_eq!(AgentStatus::Idle.symbol(), "◌");
        assert_eq!(AgentStatus::Error.symbol(), "✗");
        assert_eq!(AgentStatus::Completed.symbol(), "✓");
    }

    #[test]
    fn log_level_labels_cover_all_variants() {
        assert_eq!(LogLevel::Info.label(), "INFO ");
        assert_eq!(LogLevel::Warning.label(), "WARN ");
        assert_eq!(LogLevel::Error.label(), "ERROR");
        assert_eq!(LogLevel::Debug.label(), "DEBUG");
    }

    #[test]
    fn log_entry_new_sets_fields() {
        let before = chrono::Utc::now();
        let e = LogEntry::new(LogLevel::Warning, "hi");
        assert_eq!(e.level, LogLevel::Warning);
        assert_eq!(e.message, "hi");
        assert!(e.timestamp >= before && e.timestamp <= chrono::Utc::now());
    }

    #[test]
    fn gen_id_is_nonempty_and_unique() {
        let a = gen_id();
        let b = gen_id();
        assert!(!a.is_empty());
        assert_ne!(a, b, "consecutive ids must differ");
    }

    #[test]
    fn agent_new_sets_defaults_and_initial_log() {
        let a = Agent::new("Bot", "gpt", "do things");
        assert_eq!(a.name, "Bot");
        assert_eq!(a.model, "gpt");
        assert_eq!(a.task, "do things");
        assert_eq!(a.status, AgentStatus::Idle);
        assert!(!a.id.is_empty());
        assert_eq!(a.logs.len(), 1);
        assert_eq!(a.logs[0].level, LogLevel::Info);
        assert!(a.logs[0].message.contains("initialized"));
    }

    #[test]
    fn agent_new_accepts_empty_model() {
        let a = Agent::new("Bot", "", "task");
        assert_eq!(a.model, "");
    }

    #[test]
    fn add_log_appends_entry() {
        let mut a = Agent::new("B", "m", "t");
        a.add_log(LogLevel::Error, "boom");
        assert_eq!(a.logs.len(), 2);
        assert_eq!(a.logs[1].level, LogLevel::Error);
        assert_eq!(a.logs[1].message, "boom");
    }

    #[test]
    fn set_status_updates_and_logs_the_change() {
        let mut a = Agent::new("B", "m", "t");
        let before = a.logs.len();
        a.set_status(AgentStatus::Running);
        assert_eq!(a.status, AgentStatus::Running);
        assert_eq!(a.logs.len(), before + 1);
        let last = a.logs.last().unwrap();
        assert_eq!(last.level, LogLevel::Info);
        assert!(last.message.contains("RUNNING"));
    }

    #[test]
    fn agent_serde_roundtrip_preserves_state() {
        let mut a = Agent::new("B", "m", "t");
        a.set_status(AgentStatus::Completed);
        a.add_log(LogLevel::Debug, "trace");
        let json = serde_json::to_string(&a).unwrap();
        let back: Agent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, a.id);
        assert_eq!(back.name, a.name);
        assert_eq!(back.status, AgentStatus::Completed);
        assert_eq!(back.logs.len(), a.logs.len());
        assert_eq!(back.logs.last().unwrap().message, "trace");
    }
}
