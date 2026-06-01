mod agent;
mod app;
mod event;
mod runner;
mod storage;
mod ui;

use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

use app::App;
use event::Event;

/// Idle redraw cadence. Input and (later) agent events drive redraws directly;
/// this only governs time-based animation while nothing else is happening.
const TICK_RATE: Duration = Duration::from_millis(100);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Terminal setup ───────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // ── App ─────────────────────────────────────────────────────────
    let mut app = App::new();
    app.init();

    // Use the real backend when a key is configured; otherwise keep the mock.
    let runner_label = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.trim().is_empty() => {
            app.set_backend(Box::new(runner::AnthropicBackend::new(key)));
            "Anthropic"
        }
        _ => "mock",
    };
    app.status_msg = format!("{}  ·  runner: {runner_label}", app.status_msg);

    // ── Event sources ────────────────────────────────────────────────
    // Input and ticks arrive on one channel so the loop can multiplex them
    // (and, soon, agent-execution events) without ever blocking on the keyboard.
    let (tx, rx) = mpsc::unbounded_channel();
    event::spawn_input(tx.clone());
    event::spawn_ticker(tx.clone(), TICK_RATE);
    app.set_event_sender(tx);

    let result = run(&mut terminal, &mut app, rx).await;

    // ── Restore terminal ────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }
    Ok(())
}

async fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut rx: mpsc::UnboundedReceiver<Event>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        match rx.recv().await {
            Some(Event::Input(key)) => app.handle_key(key),
            Some(Event::Tick) => app.on_tick(),
            Some(Event::Agent { id, kind }) => app.apply_agent_event(&id, kind),
            // All event sources dropped — nothing left to drive the UI.
            None => return Ok(()),
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
