use crate::agent::{Agent, AgentStatus, LogLevel};
use crate::event::{AgentEvent, Event};
use crate::runner::{Backend, MockBackend, RunSpec};
use crate::storage;
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

#[derive(Debug, PartialEq)]
pub enum AppMode {
    Normal,
    AddAgent,
    ConfirmDelete,
    ChangeStatus,
    AddLog,
}

#[derive(Debug, PartialEq)]
pub enum Focus {
    AgentList,
    LogViewer,
}

#[derive(Debug, Default)]
pub struct AddAgentForm {
    pub name: String,
    pub model: String,
    pub task: String,
    pub field: u8, // 0=name, 1=model, 2=task
}

impl AddAgentForm {
    pub fn reset(&mut self) {
        self.name.clear();
        self.model.clear();
        self.task.clear();
        self.field = 0;
    }

    pub fn current_value_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.name,
            1 => &mut self.model,
            _ => &mut self.task,
        }
    }

    pub fn is_valid(&self) -> bool {
        !self.name.trim().is_empty() && !self.task.trim().is_empty()
    }
}

pub struct App {
    pub agents: Vec<Agent>,
    pub selected: usize,
    pub mode: AppMode,
    pub focus: Focus,
    pub log_scroll: usize,
    pub form: AddAgentForm,
    pub should_quit: bool,
    pub status_msg: String,
    // Log entry modal
    pub log_input: String,
    pub log_level: u8, // 0=Info 1=Warn 2=Error 3=Debug
    /// Live agent-runner tasks, keyed by agent id. Not part of saved state;
    /// used to dedupe runs, abort on stop, and gate out late events.
    runs: HashMap<String, JoinHandle<()>>,
    /// Channel for spawning runs from key handlers. `None` in tests/headless,
    /// where `r`/`x` become no-ops.
    tx: Option<UnboundedSender<Event>>,
    /// Monotonically increasing animation frame, advanced on each timer tick.
    pub frame: usize,
    /// Execution backend behind `r`. Defaults to the mock; replaced at startup
    /// with a real one when an API key is configured.
    backend: Box<dyn Backend>,
}

impl App {
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            selected: 0,
            mode: AppMode::Normal,
            focus: Focus::AgentList,
            log_scroll: 0,
            form: AddAgentForm::default(),
            should_quit: false,
            status_msg: String::new(),
            log_input: String::new(),
            log_level: 0,
            runs: HashMap::new(),
            tx: None,
            frame: 0,
            backend: Box::new(MockBackend),
        }
    }

    pub fn init(&mut self) {
        match storage::load() {
            Ok(agents) => {
                self.agents = agents;
                // A run cannot survive a restart, so an agent persisted as
                // RUNNING is stale (e.g. the process was killed mid-run). Reset
                // it to Idle so the displayed state matches reality.
                let mut recovered = 0;
                for a in &mut self.agents {
                    if a.status == AgentStatus::Running {
                        a.add_log(
                            LogLevel::Warning,
                            "Was running at last save; reset to idle.",
                        );
                        a.status = AgentStatus::Idle;
                        recovered += 1;
                    }
                }
                self.status_msg = if recovered > 0 {
                    format!(
                        "Loaded {} agent(s); reset {recovered} stale running.",
                        self.agents.len()
                    )
                } else {
                    format!("Loaded {} agent(s) from disk.", self.agents.len())
                };
            }
            Err(_) => {
                // Seed with demo data on first run
                let mut a1 = Agent::new(
                    "GPT-4o Researcher",
                    "gpt-4o",
                    "Summarize arXiv papers on diffusion models",
                );
                a1.add_log(LogLevel::Info, "Connecting to OpenAI API…");
                a1.add_log(LogLevel::Info, "Fetching batch #1 (12 papers)");
                a1.add_log(LogLevel::Warning, "Rate limit approaching — backing off 2s");
                a1.add_log(LogLevel::Info, "Resuming. Processed 8/12 papers.");
                a1.set_status(AgentStatus::Running);
                self.agents.push(a1);

                let mut a2 = Agent::new(
                    "Code Reviewer",
                    "claude-sonnet-4-6",
                    "Review open pull requests",
                );
                a2.add_log(
                    LogLevel::Info,
                    "Idle. Polling GitHub for new PRs every 60s.",
                );
                self.agents.push(a2);

                let mut a3 = Agent::new(
                    "Data Extractor",
                    "gpt-4o-mini",
                    "Extract structured data from PDFs",
                );
                a3.add_log(LogLevel::Info, "Job finished. 47 records extracted.");
                a3.add_log(
                    LogLevel::Debug,
                    "Token usage: 18,423 prompt / 4,211 completion",
                );
                a3.set_status(AgentStatus::Completed);
                self.agents.push(a3);

                let mut a4 = Agent::new(
                    "Email Classifier",
                    "claude-haiku-4-5",
                    "Tag and route incoming support emails",
                );
                a4.add_log(
                    LogLevel::Error,
                    "Auth token expired — could not reach IMAP server",
                );
                a4.add_log(LogLevel::Error, "Retried 3× — giving up");
                a4.set_status(AgentStatus::Error);
                self.agents.push(a4);

                self.status_msg = "Welcome! Demo agents loaded. Press 'a' to add your own.".into();
            }
        }
    }

    pub fn save(&mut self) {
        match storage::save(&self.agents) {
            Ok(path) => self.status_msg = format!("Saved to {}", path.display()),
            Err(e) => self.status_msg = format!("Save failed: {e}"),
        }
    }

    /// Persist state and return to normal mode — the common tail of every
    /// modal that commits a change.
    fn save_and_close(&mut self) {
        self.save();
        self.mode = AppMode::Normal;
    }

    /// Wire up the channel used to launch background runs. Called once at
    /// startup; left `None` in tests so `r`/`x` stay inert.
    pub fn set_event_sender(&mut self, tx: UnboundedSender<Event>) {
        self.tx = Some(tx);
    }

    /// Replace the execution backend (e.g. with a real LLM backend once an API
    /// key is known). Defaults to the mock otherwise.
    pub fn set_backend(&mut self, backend: Box<dyn Backend>) {
        self.backend = backend;
    }

    /// Launch a background run for the selected agent. No-op if there is no
    /// sender (tests), no selection, or a run is already in flight.
    fn run_selected(&mut self) {
        let Some(tx) = self.tx.clone() else {
            return;
        };
        let (id, name, model, task) = match self.agents.get(self.selected) {
            Some(a) => (
                a.id.clone(),
                a.name.clone(),
                a.model.clone(),
                a.task.clone(),
            ),
            None => return,
        };
        if self.runs.contains_key(&id) {
            self.status_msg = format!("'{name}' is already running.");
            return;
        }
        let handle = self.backend.spawn(
            RunSpec {
                id: id.clone(),
                model,
                task,
            },
            tx,
        );
        self.runs.insert(id, handle);
        self.status_msg = format!("Running '{name}'…");
    }

    /// Stop the selected agent's run, if any. Aborting the task and dropping it
    /// from `runs` makes `apply_agent_event` discard events still in flight.
    fn stop_selected(&mut self) {
        let id = match self.agents.get(self.selected) {
            Some(a) => a.id.clone(),
            None => return,
        };
        if let Some(handle) = self.runs.remove(&id) {
            handle.abort();
            if let Some(a) = self.agents.get_mut(self.selected) {
                a.partial.clear();
                a.add_log(LogLevel::Warning, "Run stopped by user.");
                a.set_status(AgentStatus::Idle);
                self.log_scroll = a.logs.len().saturating_sub(1);
            }
            self.status_msg = "Run stopped.".into();
        }
    }

    /// Abort every in-flight run and reset its agent to Idle. Called on quit so
    /// the persisted state never claims an agent is running when it isn't.
    fn abort_all_runs(&mut self) {
        let running: Vec<String> = self.runs.keys().cloned().collect();
        for (_, handle) in self.runs.drain() {
            handle.abort();
        }
        for id in &running {
            if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                a.partial.clear();
                if a.status == AgentStatus::Running {
                    a.add_log(LogLevel::Warning, "Run interrupted on quit; reset to idle.");
                    a.status = AgentStatus::Idle;
                }
            }
        }
    }

    /// Apply an event from a background runner. Routed by agent **id**, never
    /// by selection index — the list can be reordered or the agent deleted
    /// while a run is in flight. Events for a run that is no longer tracked
    /// (stopped, or already finished) are ignored.
    pub fn apply_agent_event(&mut self, id: &str, kind: AgentEvent) {
        if !self.runs.contains_key(id) {
            return;
        }
        let Some(agent) = self.agents.iter_mut().find(|a| a.id == id) else {
            // Agent was deleted mid-run; drop the orphaned handle.
            self.runs.remove(id);
            return;
        };
        let done = match kind {
            AgentEvent::Started => {
                agent.set_status(AgentStatus::Running);
                false
            }
            AgentEvent::Log(level, msg) => {
                agent.add_log(level, msg);
                false
            }
            AgentEvent::Token(delta) => {
                agent.partial.push_str(&delta);
                false
            }
            AgentEvent::Finished { tokens_used } => {
                let text = std::mem::take(&mut agent.partial);
                let text = text.trim();
                if !text.is_empty() {
                    agent.add_log(LogLevel::Info, text.to_string());
                }
                agent.tokens_used = tokens_used;
                agent.set_status(AgentStatus::Completed);
                true
            }
            AgentEvent::Failed(err) => {
                agent.partial.clear();
                agent.add_log(LogLevel::Error, err);
                agent.set_status(AgentStatus::Error);
                true
            }
        };
        if done {
            self.runs.remove(id);
            // If the user is watching this agent, pin the view to the freshly
            // appended tail so the result stays put once streaming stops.
            if self.selected_agent().is_some_and(|a| a.id == id) {
                self.log_scroll = self.agents[self.selected].logs.len().saturating_sub(1);
            }
            self.save();
        }
    }

    /// Whether a background run is currently tracked for this agent id. Drives
    /// the spinner and live-output rendering — true only while a task is
    /// actually executing, independent of the displayed status label.
    pub fn is_running(&self, id: &str) -> bool {
        self.runs.contains_key(id)
    }

    /// Advance the animation frame. Called on every timer tick so spinners and
    /// streamed output animate while the user is idle.
    pub fn on_tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn mark_running_for_test(&mut self, id: &str) {
        self.runs.insert(id.to_string(), tokio::spawn(async {}));
    }

    pub fn selected_agent(&self) -> Option<&Agent> {
        self.agents.get(self.selected)
    }

    pub fn selected_agent_mut(&mut self) -> Option<&mut Agent> {
        self.agents.get_mut(self.selected)
    }

    // ── Key dispatch ──────────────────────────────────────────────────

    pub fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            AppMode::Normal => self.key_normal(key),
            AppMode::AddAgent => self.key_add_agent(key),
            AppMode::ConfirmDelete => self.key_confirm_delete(key),
            AppMode::ChangeStatus => self.key_change_status(key),
            AppMode::AddLog => self.key_add_log(key),
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => {
                self.abort_all_runs();
                self.save();
                self.should_quit = true;
            }

            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::AgentList => Focus::LogViewer,
                    Focus::LogViewer => Focus::AgentList,
                };
            }

            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                Focus::AgentList => {
                    if !self.agents.is_empty() {
                        self.selected = self.selected.saturating_sub(1);
                        self.log_scroll = 0;
                    }
                }
                Focus::LogViewer => {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                }
            },

            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                Focus::AgentList => {
                    if !self.agents.is_empty() {
                        self.selected = (self.selected + 1).min(self.agents.len() - 1);
                        self.log_scroll = 0;
                    }
                }
                Focus::LogViewer => {
                    let max = self
                        .selected_agent()
                        .map(|a| a.logs.len().saturating_sub(1))
                        .unwrap_or(0);
                    self.log_scroll = (self.log_scroll + 1).min(max);
                }
            },

            KeyCode::Char('G') => {
                // Jump to bottom of log
                if let Some(a) = self.selected_agent() {
                    self.log_scroll = a.logs.len().saturating_sub(1);
                }
            }
            KeyCode::Char('g') => {
                self.log_scroll = 0;
            }

            KeyCode::Char('a') => {
                self.form.reset();
                self.mode = AppMode::AddAgent;
            }

            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace
                if !self.agents.is_empty() =>
            {
                self.mode = AppMode::ConfirmDelete;
            }

            KeyCode::Char('s') if !self.agents.is_empty() => {
                self.mode = AppMode::ChangeStatus;
            }

            KeyCode::Char('l') if !self.agents.is_empty() => {
                self.log_input.clear();
                self.log_level = 0;
                self.mode = AppMode::AddLog;
            }

            KeyCode::Char('S') => {
                self.save();
            }

            KeyCode::Char('r') if !self.agents.is_empty() => {
                self.run_selected();
            }

            KeyCode::Char('x') if !self.agents.is_empty() => {
                self.stop_selected();
            }

            _ => {}
        }
    }

    fn key_add_agent(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
            }

            KeyCode::Tab => {
                self.form.field = (self.form.field + 1) % 3;
            }
            KeyCode::BackTab => {
                self.form.field = self.form.field.saturating_sub(1);
            }

            KeyCode::Enter => {
                if self.form.field < 2 {
                    self.form.field += 1;
                } else if self.form.is_valid() {
                    let model = if self.form.model.trim().is_empty() {
                        "unknown".to_string()
                    } else {
                        self.form.model.trim().to_string()
                    };
                    let agent = Agent::new(self.form.name.trim(), model, self.form.task.trim());
                    let name = agent.name.clone();
                    self.agents.push(agent);
                    self.selected = self.agents.len() - 1;
                    self.log_scroll = 0;
                    self.save_and_close();
                    self.status_msg = format!("Agent '{name}' added.");
                }
            }

            KeyCode::Backspace => {
                self.form.current_value_mut().pop();
            }
            KeyCode::Char(c) => {
                self.form.current_value_mut().push(c);
            }
            _ => {}
        }
    }

    fn key_confirm_delete(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if !self.agents.is_empty() {
                    let name = self.agents[self.selected].name.clone();
                    self.agents.remove(self.selected);
                    if self.selected >= self.agents.len() && !self.agents.is_empty() {
                        self.selected = self.agents.len() - 1;
                    }
                    self.log_scroll = 0;
                    self.save();
                    self.status_msg = format!("Agent '{name}' removed.");
                }
                self.mode = AppMode::Normal;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }

    fn key_change_status(&mut self, key: KeyEvent) {
        let new_status = match key.code {
            KeyCode::Char('1') => Some(AgentStatus::Running),
            KeyCode::Char('2') => Some(AgentStatus::Idle),
            KeyCode::Char('3') => Some(AgentStatus::Error),
            KeyCode::Char('4') => Some(AgentStatus::Completed),
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                return;
            }
            _ => None,
        };
        if let Some(status) = new_status {
            if let Some(a) = self.selected_agent_mut() {
                a.set_status(status);
            }
            self.save_and_close();
        }
    }

    fn key_add_log(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
            }
            KeyCode::Tab => {
                self.log_level = (self.log_level + 1) % 4;
            }
            KeyCode::Enter if !self.log_input.trim().is_empty() => {
                let level = match self.log_level {
                    1 => LogLevel::Warning,
                    2 => LogLevel::Error,
                    3 => LogLevel::Debug,
                    _ => LogLevel::Info,
                };
                let msg = self.log_input.trim().to_string();
                if let Some(a) = self.selected_agent_mut() {
                    a.add_log(level, msg);
                    self.log_scroll = a.logs.len().saturating_sub(1);
                }
                self.save_and_close();
            }
            KeyCode::Backspace => {
                self.log_input.pop();
            }
            KeyCode::Char(c) => {
                self.log_input.push(c);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempHome;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn app_with_agents(n: usize) -> App {
        let mut app = App::new();
        for i in 0..n {
            app.agents
                .push(Agent::new(format!("agent{i}"), "m", "task"));
        }
        app
    }

    // ── App construction & accessors ──────────────────────────────────

    #[test]
    fn new_app_has_expected_defaults() {
        let app = App::new();
        assert!(app.agents.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::AgentList);
        assert_eq!(app.log_scroll, 0);
        assert!(!app.should_quit);
        assert!(app.status_msg.is_empty());
    }

    #[test]
    fn selected_agent_is_none_when_empty() {
        let app = App::new();
        assert!(app.selected_agent().is_none());
    }

    #[test]
    fn selected_agent_tracks_selection() {
        let mut app = app_with_agents(2);
        assert_eq!(app.selected_agent().unwrap().name, "agent0");
        app.selected = 1;
        assert_eq!(app.selected_agent().unwrap().name, "agent1");
        assert_eq!(app.selected_agent_mut().unwrap().name, "agent1");
    }

    // ── AddAgentForm ──────────────────────────────────────────────────

    #[test]
    fn form_reset_clears_all_fields() {
        let mut f = AddAgentForm {
            name: "x".into(),
            model: "y".into(),
            task: "z".into(),
            field: 2,
        };
        f.reset();
        assert!(f.name.is_empty() && f.model.is_empty() && f.task.is_empty());
        assert_eq!(f.field, 0);
    }

    #[test]
    fn form_current_value_mut_selects_by_field() {
        let mut f = AddAgentForm::default(); // field defaults to 0
        f.current_value_mut().push('n');
        f.field = 1;
        f.current_value_mut().push('m');
        f.field = 2;
        f.current_value_mut().push('t');
        assert_eq!(f.name, "n");
        assert_eq!(f.model, "m");
        assert_eq!(f.task, "t");
    }

    #[test]
    fn form_is_valid_requires_nonblank_name_and_task() {
        let mut f = AddAgentForm::default();
        assert!(!f.is_valid());
        f.name = "  ".into();
        f.task = "t".into();
        assert!(!f.is_valid());
        f.name = "n".into();
        f.task = "   ".into();
        assert!(!f.is_valid());
        f.task = "t".into();
        assert!(f.is_valid());
    }

    // ── Normal-mode navigation ────────────────────────────────────────

    #[test]
    fn down_and_up_navigate_and_clamp() {
        let mut app = app_with_agents(3);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected, 1);
        app.handle_key(ch('j'));
        assert_eq!(app.selected, 2);
        app.handle_key(key(KeyCode::Down)); // clamp at last
        assert_eq!(app.selected, 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected, 1);
        app.handle_key(ch('k'));
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Up)); // saturate at first
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn changing_agent_resets_log_scroll() {
        let mut app = app_with_agents(2);
        app.log_scroll = 5;
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn navigation_is_noop_when_no_agents() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn tab_toggles_focus() {
        let mut app = app_with_agents(1);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::LogViewer);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::AgentList);
    }

    #[test]
    fn log_viewer_scroll_clamps_to_log_bounds() {
        let mut app = app_with_agents(1);
        for i in 0..3 {
            app.agents[0].add_log(LogLevel::Info, format!("l{i}"));
        }
        // agent now has 4 logs → max scroll index 3
        app.focus = Focus::LogViewer;
        for _ in 0..10 {
            app.handle_key(ch('j'));
        }
        assert_eq!(app.log_scroll, 3);
        for _ in 0..10 {
            app.handle_key(ch('k'));
        }
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn g_keys_jump_to_top_and_bottom_of_log() {
        let mut app = app_with_agents(1);
        for i in 0..3 {
            app.agents[0].add_log(LogLevel::Info, format!("l{i}"));
        }
        app.handle_key(ch('G'));
        assert_eq!(app.log_scroll, app.agents[0].logs.len() - 1);
        app.handle_key(ch('g'));
        assert_eq!(app.log_scroll, 0);
    }

    // ── Normal-mode transitions ───────────────────────────────────────

    #[test]
    fn a_enters_add_agent_and_resets_form() {
        let mut app = App::new();
        app.form.name = "stale".into();
        app.handle_key(ch('a'));
        assert_eq!(app.mode, AppMode::AddAgent);
        assert!(app.form.name.is_empty());
    }

    #[test]
    fn delete_keys_require_agents() {
        let mut empty = App::new();
        empty.handle_key(ch('d'));
        assert_eq!(empty.mode, AppMode::Normal);

        let mut app = app_with_agents(1);
        app.handle_key(ch('d'));
        assert_eq!(app.mode, AppMode::ConfirmDelete);

        let mut app = app_with_agents(1);
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.mode, AppMode::ConfirmDelete);
    }

    #[test]
    fn s_enters_change_status_only_with_agents() {
        let mut empty = App::new();
        empty.handle_key(ch('s'));
        assert_eq!(empty.mode, AppMode::Normal);

        let mut app = app_with_agents(1);
        app.handle_key(ch('s'));
        assert_eq!(app.mode, AppMode::ChangeStatus);
    }

    #[test]
    fn l_enters_add_log_and_resets_inputs() {
        let mut empty = App::new();
        empty.handle_key(ch('l'));
        assert_eq!(empty.mode, AppMode::Normal);

        let mut app = app_with_agents(1);
        app.log_input = "stale".into();
        app.log_level = 3;
        app.handle_key(ch('l'));
        assert_eq!(app.mode, AppMode::AddLog);
        assert!(app.log_input.is_empty());
        assert_eq!(app.log_level, 0);
    }

    #[test]
    fn q_saves_and_quits() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        app.handle_key(ch('q'));
        assert!(app.should_quit);
        assert!(app.status_msg.starts_with("Saved to"));
    }

    #[test]
    fn shift_s_saves_without_quitting() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        app.handle_key(ch('S'));
        assert!(!app.should_quit);
        assert!(app.status_msg.starts_with("Saved to"));
    }

    // ── Add-agent modal ───────────────────────────────────────────────

    #[test]
    fn add_agent_tab_cycles_fields() {
        let mut app = App::new();
        app.mode = AppMode::AddAgent;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.form.field, 1);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.form.field, 2);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.form.field, 0);
    }

    #[test]
    fn add_agent_backtab_saturates_at_zero() {
        let mut app = App::new();
        app.mode = AppMode::AddAgent;
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.form.field, 0);
        app.form.field = 2;
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.form.field, 1);
    }

    #[test]
    fn add_agent_typing_and_backspace_edit_current_field() {
        let mut app = App::new();
        app.mode = AppMode::AddAgent;
        app.handle_key(ch('h'));
        app.handle_key(ch('i'));
        assert_eq!(app.form.name, "hi");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.form.name, "h");
    }

    #[test]
    fn add_agent_esc_cancels() {
        let mut app = App::new();
        app.mode = AppMode::AddAgent;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn add_agent_enter_advances_fields_then_creates() {
        let _h = TempHome::new();
        let mut app = App::new();
        app.mode = AppMode::AddAgent;
        for c in "Bot".chars() {
            app.handle_key(ch(c));
        }
        app.handle_key(key(KeyCode::Enter)); // name → model
        assert_eq!(app.form.field, 1);
        app.handle_key(key(KeyCode::Enter)); // model (blank) → task
        assert_eq!(app.form.field, 2);
        app.handle_key(ch('T'));
        app.handle_key(key(KeyCode::Enter)); // valid → create
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agents[0].name, "Bot");
        assert_eq!(app.agents[0].model, "unknown"); // blank model defaults
        assert_eq!(app.selected, 0);
        assert!(app.status_msg.contains("added"));
    }

    #[test]
    fn add_agent_enter_on_invalid_form_does_nothing() {
        let mut app = App::new();
        app.mode = AppMode::AddAgent;
        app.form.field = 2; // on task field but name & task empty
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, AppMode::AddAgent);
        assert!(app.agents.is_empty());
    }

    #[test]
    fn add_agent_trims_supplied_model() {
        let _h = TempHome::new();
        let mut app = App::new();
        app.mode = AppMode::AddAgent;
        app.form.name = "N".into();
        app.form.model = "  gpt  ".into();
        app.form.task = "T".into();
        app.form.field = 2;
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.agents[0].model, "gpt");
    }

    // ── Confirm-delete modal ──────────────────────────────────────────

    #[test]
    fn confirm_delete_yes_removes_and_adjusts_selection() {
        let _h = TempHome::new();
        let mut app = app_with_agents(2);
        app.selected = 1;
        app.mode = AppMode::ConfirmDelete;
        app.handle_key(ch('y'));
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.selected, 0); // clamped after removing the last row
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.status_msg.contains("removed"));
    }

    #[test]
    fn confirm_delete_from_middle_keeps_index() {
        let _h = TempHome::new();
        let mut app = app_with_agents(3);
        app.selected = 0;
        app.mode = AppMode::ConfirmDelete;
        app.handle_key(ch('Y'));
        assert_eq!(app.agents.len(), 2);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn confirm_delete_no_and_esc_cancel() {
        for cancel in [ch('n'), ch('N'), key(KeyCode::Esc)] {
            let mut app = app_with_agents(1);
            app.mode = AppMode::ConfirmDelete;
            app.handle_key(cancel);
            assert_eq!(app.agents.len(), 1);
            assert_eq!(app.mode, AppMode::Normal);
        }
    }

    // ── Change-status modal ───────────────────────────────────────────

    #[test]
    fn change_status_maps_every_key() {
        let _h = TempHome::new();
        for (k, expected) in [
            ('1', AgentStatus::Running),
            ('2', AgentStatus::Idle),
            ('3', AgentStatus::Error),
            ('4', AgentStatus::Completed),
        ] {
            let mut app = app_with_agents(1);
            app.mode = AppMode::ChangeStatus;
            app.handle_key(ch(k));
            assert_eq!(app.agents[0].status, expected);
            assert_eq!(app.mode, AppMode::Normal);
        }
    }

    #[test]
    fn change_status_esc_cancels() {
        let mut app = app_with_agents(1);
        app.mode = AppMode::ChangeStatus;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn change_status_unknown_key_stays_open() {
        let mut app = app_with_agents(1);
        app.mode = AppMode::ChangeStatus;
        app.handle_key(ch('x'));
        assert_eq!(app.mode, AppMode::ChangeStatus);
    }

    // ── Add-log modal ─────────────────────────────────────────────────

    #[test]
    fn add_log_enter_appends_and_scrolls_to_bottom() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        let before = app.agents[0].logs.len();
        app.mode = AppMode::AddLog;
        app.log_input = "hello".into();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.agents[0].logs.len(), before + 1);
        let last = app.agents[0].logs.last().unwrap();
        assert_eq!(last.message, "hello");
        assert_eq!(last.level, LogLevel::Info);
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.log_scroll, app.agents[0].logs.len() - 1);
    }

    #[test]
    fn add_log_level_picker_maps_to_level() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        app.mode = AppMode::AddLog;
        app.handle_key(key(KeyCode::Tab)); // 0 → 1 (Warning)
        assert_eq!(app.log_level, 1);
        app.log_input = "warn".into();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.agents[0].logs.last().unwrap().level, LogLevel::Warning);
    }

    #[test]
    fn add_log_tab_wraps_through_four_levels() {
        let mut app = app_with_agents(1);
        app.mode = AppMode::AddLog;
        for _ in 0..4 {
            app.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(app.log_level, 0);
    }

    #[test]
    fn add_log_blank_input_does_nothing() {
        let mut app = app_with_agents(1);
        let before = app.agents[0].logs.len();
        app.mode = AppMode::AddLog;
        app.log_input = "   ".into();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.agents[0].logs.len(), before);
        assert_eq!(app.mode, AppMode::AddLog);
    }

    #[test]
    fn add_log_typing_and_backspace_edit_input() {
        let mut app = app_with_agents(1);
        app.mode = AppMode::AddLog;
        app.handle_key(ch('h'));
        app.handle_key(ch('i'));
        assert_eq!(app.log_input, "hi");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.log_input, "h");
    }

    #[test]
    fn add_log_esc_cancels() {
        let mut app = app_with_agents(1);
        app.mode = AppMode::AddLog;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, AppMode::Normal);
    }

    // ── init() persistence behaviour ──────────────────────────────────

    #[test]
    fn init_seeds_demo_agents_when_no_file() {
        let _h = TempHome::new();
        let mut app = App::new();
        app.init();
        assert_eq!(app.agents.len(), 4);
        assert!(app.status_msg.contains("Welcome") || app.status_msg.contains("Demo"));
    }

    #[test]
    fn init_loads_existing_file() {
        let _h = TempHome::new();
        let mut writer = App::new();
        writer.agents.push(Agent::new("Persisted", "m", "t"));
        writer.save();

        let mut reader = App::new();
        reader.init();
        assert!(reader.agents.iter().any(|a| a.name == "Persisted"));
        assert!(reader.status_msg.contains("Loaded"));
    }

    // ── Agent-execution event routing ─────────────────────────────────

    #[tokio::test]
    async fn started_event_sets_running() {
        let mut app = app_with_agents(1);
        let id = app.agents[0].id.clone();
        app.mark_running_for_test(&id);
        app.apply_agent_event(&id, AgentEvent::Started);
        assert_eq!(app.agents[0].status, AgentStatus::Running);
    }

    #[tokio::test]
    async fn tokens_accumulate_then_finish_flushes_to_log() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        let id = app.agents[0].id.clone();
        app.mark_running_for_test(&id);
        app.apply_agent_event(&id, AgentEvent::Token("hello ".into()));
        app.apply_agent_event(&id, AgentEvent::Token("world".into()));
        assert_eq!(app.agents[0].partial, "hello world");

        app.apply_agent_event(&id, AgentEvent::Finished { tokens_used: 2 });
        assert_eq!(app.agents[0].status, AgentStatus::Completed);
        assert_eq!(app.agents[0].tokens_used, 2);
        assert!(app.agents[0].partial.is_empty());
        assert!(app.agents[0]
            .logs
            .iter()
            .any(|l| l.message == "hello world"));
        // run handle cleared on completion
        assert!(!app.is_running(&id));
    }

    #[tokio::test]
    async fn failed_event_sets_error_and_clears_partial() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        let id = app.agents[0].id.clone();
        app.mark_running_for_test(&id);
        app.apply_agent_event(&id, AgentEvent::Token("partial".into()));
        app.apply_agent_event(&id, AgentEvent::Failed("boom".into()));
        assert_eq!(app.agents[0].status, AgentStatus::Error);
        assert!(app.agents[0].partial.is_empty());
        assert!(app.agents[0].logs.iter().any(|l| l.message == "boom"));
        assert!(!app.is_running(&id));
    }

    #[tokio::test]
    async fn events_for_untracked_run_are_ignored() {
        let mut app = app_with_agents(1);
        let id = app.agents[0].id.clone();
        // never marked running → the event must be a no-op
        app.apply_agent_event(&id, AgentEvent::Started);
        assert_eq!(app.agents[0].status, AgentStatus::Idle);
    }

    #[tokio::test]
    async fn event_routes_by_id_not_index() {
        let mut app = app_with_agents(3);
        let id = app.agents[2].id.clone();
        app.mark_running_for_test(&id);
        // selection sits on a different row; routing must follow the id
        app.selected = 0;
        app.apply_agent_event(&id, AgentEvent::Started);
        assert_eq!(app.agents[2].status, AgentStatus::Running);
        assert_eq!(app.agents[0].status, AgentStatus::Idle);
    }

    #[tokio::test]
    async fn finished_event_for_deleted_agent_clears_run() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        let id = app.agents[0].id.clone();
        app.mark_running_for_test(&id);
        app.agents.clear();
        app.apply_agent_event(&id, AgentEvent::Finished { tokens_used: 0 });
        assert!(!app.is_running(&id));
    }

    #[test]
    fn run_and_stop_keys_are_noop_without_agents() {
        let mut app = App::new();
        app.handle_key(ch('r'));
        app.handle_key(ch('x'));
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.agents.is_empty());
    }

    // ── Stale-run reconciliation ──────────────────────────────────────

    #[test]
    fn init_resets_stale_running_status_from_disk() {
        let _h = TempHome::new();
        let mut writer = App::new();
        let mut ghost = Agent::new("Ghost", "m", "t");
        ghost.set_status(AgentStatus::Running);
        writer.agents.push(ghost);
        writer.save();

        let mut reader = App::new();
        reader.init();
        let ghost = reader.agents.iter().find(|a| a.name == "Ghost").unwrap();
        // RUNNING never survives a restart — no live run backs it.
        assert_eq!(ghost.status, AgentStatus::Idle);
        assert!(reader.status_msg.contains("reset"));
    }

    #[tokio::test]
    async fn abort_all_runs_clears_handles_and_resets_status() {
        let mut app = app_with_agents(1);
        let id = app.agents[0].id.clone();
        app.mark_running_for_test(&id);
        app.agents[0].set_status(AgentStatus::Running);
        app.agents[0].partial = "buffered".into();

        app.abort_all_runs();
        assert!(!app.is_running(&id));
        assert_eq!(app.agents[0].status, AgentStatus::Idle);
        assert!(app.agents[0].partial.is_empty());
    }

    #[tokio::test]
    async fn quit_aborts_runs_and_persists_idle_state() {
        let _h = TempHome::new();
        let mut app = app_with_agents(1);
        let id = app.agents[0].id.clone();
        app.mark_running_for_test(&id);
        app.agents[0].set_status(AgentStatus::Running);

        app.handle_key(ch('q'));
        assert!(app.should_quit);
        assert!(!app.is_running(&id));
        assert_eq!(app.agents[0].status, AgentStatus::Idle);

        // The persisted file must reflect Idle, not the stale Running.
        let reloaded = crate::storage::load().unwrap();
        assert_eq!(reloaded[0].status, AgentStatus::Idle);
    }
}
