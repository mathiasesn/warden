use crate::agent::{AgentStatus, LogLevel};
use crate::app::{App, AppMode, Focus};
use ratatui::{prelude::*, widgets::*};

// ─── Color palette ────────────────────────────────────────────────────────────

const C_ACCENT: Color = Color::Cyan;
const C_SUCCESS: Color = Color::Green;
const C_WARN: Color = Color::Yellow;
const C_ERROR: Color = Color::Red;
const C_DONE: Color = Color::Magenta;
const C_DIM: Color = Color::DarkGray;
const C_TEXT: Color = Color::White;
const C_BG_SEL: Color = Color::Rgb(40, 44, 52);

fn status_color(s: &AgentStatus) -> Color {
    match s {
        AgentStatus::Running => C_SUCCESS,
        AgentStatus::Idle => C_WARN,
        AgentStatus::Error => C_ERROR,
        AgentStatus::Completed => C_DONE,
    }
}

fn log_level_color(l: &LogLevel) -> Color {
    match l {
        LogLevel::Info => C_ACCENT,
        LogLevel::Warning => C_WARN,
        LogLevel::Error => C_ERROR,
        LogLevel::Debug => C_DIM,
    }
}

/// Braille spinner frames for actively-running agents.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The last `max` characters of `s` (counted by char, not byte), so streamed
/// output keeps its newest tokens on screen instead of running off the edge.
fn tail_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        s.chars().skip(count - max).collect()
    }
}

// ─── Root draw ────────────────────────────────────────────────────────────────

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Outer layout: content + key hint bar
    let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let body_area = layout[0];
    let bar_area = layout[1];

    // Body: left panel + right panel
    let panels = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(body_area);

    draw_agent_list(frame, app, panels[0]);
    draw_log_viewer(frame, app, panels[1]);
    draw_key_bar(frame, app, bar_area);

    // Modals (rendered on top)
    match &app.mode {
        AppMode::AddAgent => draw_modal_add_agent(frame, app, area),
        AppMode::ConfirmDelete => draw_modal_confirm_delete(frame, app, area),
        AppMode::ChangeStatus => draw_modal_change_status(frame, area),
        AppMode::AddLog => draw_modal_add_log(frame, app, area),
        AppMode::Normal => {}
    }
}

// ─── Agent list ───────────────────────────────────────────────────────────────

fn draw_agent_list(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::AgentList;
    let border_color = if focused { C_ACCENT } else { C_DIM };

    let block = Block::default()
        .title(Span::styled(
            " ◈ Agents ",
            Style::default().fg(C_ACCENT).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("Name"),
        Cell::from("Model"),
        Cell::from("Status"),
        Cell::from("Task"),
    ])
    .style(Style::default().fg(C_WARN).bold())
    .bottom_margin(1);

    let rows: Vec<Row> = app
        .agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let is_sel = i == app.selected;
            let sel_marker = if is_sel && focused { "▶" } else { " " };

            // A spinner replaces the static symbol while a run is live.
            let symbol = if app.is_running(&agent.id) {
                SPINNER[app.frame % SPINNER.len()]
            } else {
                agent.status.symbol()
            };
            let status_cell = Cell::from(Span::styled(
                format!("{} {}", symbol, agent.status.label()),
                Style::default().fg(status_color(&agent.status)).bold(),
            ));

            let task_preview: String = agent.task.chars().take(22).collect();
            let task_preview = if agent.task.chars().count() > 22 {
                format!("{task_preview}…")
            } else {
                task_preview
            };

            let row_style = if is_sel {
                Style::default().bg(C_BG_SEL).bold()
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(sel_marker).style(Style::default().fg(C_ACCENT)),
                Cell::from(agent.name.as_str()),
                Cell::from(agent.model.as_str()).style(Style::default().fg(C_DIM)),
                status_cell,
                Cell::from(task_preview).style(Style::default().fg(Color::Gray)),
            ])
            .style(row_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(28),
            Constraint::Percentage(22),
            Constraint::Length(13),
            Constraint::Min(0),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);

    // Empty state
    if app.agents.is_empty() {
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 3,
        });
        frame.render_widget(
            Paragraph::new("No agents yet.\nPress 'a' to add one.")
                .style(Style::default().fg(C_DIM))
                .alignment(Alignment::Center),
            inner,
        );
    }
}

// ─── Log viewer ───────────────────────────────────────────────────────────────

fn draw_log_viewer(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::LogViewer;
    let border_color = if focused { C_ACCENT } else { C_DIM };

    let title = if let Some(a) = app.selected_agent() {
        format!(" ◈ Logs — {} ", a.name)
    } else {
        " ◈ Logs ".to_string()
    };

    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(C_ACCENT).bold()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(agent) = app.selected_agent() else {
        return;
    };
    let running = app.is_running(&agent.id);

    if agent.logs.is_empty() && agent.partial.is_empty() {
        frame.render_widget(
            Paragraph::new("No log entries yet.")
                .style(Style::default().fg(C_DIM))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    // Committed log entries, one line each.
    let mut lines: Vec<Line> = agent
        .logs
        .iter()
        .map(|entry| {
            let ts = entry.timestamp.format("%H:%M:%S").to_string();
            let lvl = entry.level.label();
            let col = log_level_color(&entry.level);

            Line::from(vec![
                Span::styled(ts, Style::default().fg(C_DIM)),
                Span::raw("  "),
                Span::styled(lvl, Style::default().fg(col).bold()),
                Span::raw("  "),
                Span::styled(entry.message.as_str(), Style::default().fg(C_TEXT)),
            ])
        })
        .collect();

    // In-flight streamed output for a running agent: a spinner, the tail of the
    // text so far, and a cursor block.
    if running && !agent.partial.is_empty() {
        let spin = SPINNER[app.frame % SPINNER.len()];
        let budget = (inner.width as usize).saturating_sub(4);
        let tail = tail_chars(&agent.partial, budget);
        lines.push(Line::from(vec![
            Span::styled(spin, Style::default().fg(C_ACCENT)),
            Span::raw(" "),
            Span::styled(tail, Style::default().fg(C_ACCENT).italic()),
            Span::styled("▌", Style::default().fg(C_ACCENT)),
        ]));
    }

    let total = lines.len();
    let height = (inner.height as usize).max(1);

    // While a run streams and the user isn't manually scrolling the log pane,
    // follow the tail; otherwise honour their scroll position.
    let top = if running && !focused {
        total.saturating_sub(height)
    } else {
        app.log_scroll.min(total.saturating_sub(1))
    };

    let visible: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);

    // Scroll position badge — shows the bottom-most visible row.
    if total > height {
        let shown = (top + height).min(total);
        let badge = format!(" {}/{} ↕ ", shown, total);
        // Width in terminal columns, not bytes — `↕` is multi-byte but one cell.
        let bw = badge.chars().count() as u16;
        let badge_area = Rect {
            x: area.right().saturating_sub(bw + 1),
            y: area.bottom().saturating_sub(1),
            width: bw,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(badge).style(Style::default().fg(C_DIM)),
            badge_area,
        );
    }
}

// ─── Key hint bar ─────────────────────────────────────────────────────────────

fn draw_key_bar(frame: &mut Frame, app: &App, area: Rect) {
    let (msg_color, msg) = if app.status_msg.is_empty() {
        (C_DIM, "Ready".to_string())
    } else {
        (C_SUCCESS, app.status_msg.clone())
    };

    let hints = match app.focus {
        Focus::AgentList => {
            "  [a]dd  [d]el  [s]tatus  [l]og  [r]un  [x]stop  [S]ave  [Tab]switch  [q]uit"
        }
        Focus::LogViewer => "  [j/k]scroll  [g/G]top/bot  [Tab]switch  [q]quit",
    };

    let line = Line::from(vec![
        Span::styled(format!("  {msg}"), Style::default().fg(msg_color)),
        Span::styled(hints, Style::default().fg(C_DIM)),
    ]);

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Rgb(20, 20, 24))),
        area,
    );
}

// ─── Modal: Add Agent ─────────────────────────────────────────────────────────

fn draw_modal_add_agent(frame: &mut Frame, app: &App, area: Rect) {
    let modal = centered_rect(58, 16, area);
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .title(Span::styled(
            " ✦ Add New Agent ",
            Style::default().fg(C_SUCCESS).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_SUCCESS));

    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let rows = Layout::vertical([
        Constraint::Length(2), // hint
        Constraint::Length(3), // name
        Constraint::Length(3), // model
        Constraint::Length(3), // task
        Constraint::Length(2), // confirm hint
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new("  Tab/Enter → next field   Esc → cancel").style(Style::default().fg(C_DIM)),
        rows[0],
    );

    let fields = [
        ("Name *", app.form.name.as_str(), 0u8),
        (
            "Model  (leave blank = unknown)",
            app.form.model.as_str(),
            1u8,
        ),
        ("Task *", app.form.task.as_str(), 2u8),
    ];

    for (i, (label, value, idx)) in fields.iter().enumerate() {
        let active = app.form.field == *idx;
        let bc = if active { C_ACCENT } else { C_DIM };
        let display = if active {
            format!("{value}▌")
        } else {
            value.to_string()
        };
        frame.render_widget(
            Paragraph::new(display).block(
                Block::default()
                    .title(format!(" {label} "))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(bc)),
            ),
            rows[i + 1],
        );
    }

    let confirm_hint = if app.form.is_valid() {
        Span::styled(
            "  ↵ Enter on 'Task' to confirm",
            Style::default().fg(C_SUCCESS),
        )
    } else {
        Span::styled("  Name and Task are required", Style::default().fg(C_WARN))
    };
    frame.render_widget(Paragraph::new(confirm_hint), rows[4]);
}

// ─── Modal: Confirm Delete ────────────────────────────────────────────────────

fn draw_modal_confirm_delete(frame: &mut Frame, app: &App, area: Rect) {
    let modal = centered_rect(48, 7, area);
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .title(Span::styled(
            " ✦ Confirm Delete ",
            Style::default().fg(C_ERROR).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_ERROR));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let name = app.selected_agent().map(|a| a.name.as_str()).unwrap_or("?");
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Remove "),
            Span::styled(name, Style::default().fg(C_WARN).bold()),
            Span::raw("?"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  [y] Yes   [n / Esc] No",
            Style::default().fg(C_DIM),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

// ─── Modal: Change Status ─────────────────────────────────────────────────────

fn draw_modal_change_status(frame: &mut Frame, area: Rect) {
    let modal = centered_rect(36, 10, area);
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .title(Span::styled(
            " ✦ Set Status ",
            Style::default().fg(C_WARN).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_WARN));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let options: Vec<(&str, &str, Color)> = vec![
        ("[1]", "◉ RUNNING", C_SUCCESS),
        ("[2]", "◌ IDLE", C_WARN),
        ("[3]", "✗ ERROR", C_ERROR),
        ("[4]", "✓ COMPLETED", C_DONE),
    ];

    let mut lines: Vec<Line> = options
        .iter()
        .map(|(key, label, color)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(*key, Style::default().fg(C_DIM)),
                Span::raw("  "),
                Span::styled(*label, Style::default().fg(*color).bold()),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [Esc] Cancel",
        Style::default().fg(C_DIM),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

// ─── Modal: Add Log ───────────────────────────────────────────────────────────

fn draw_modal_add_log(frame: &mut Frame, app: &App, area: Rect) {
    let modal = centered_rect(58, 11, area);
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .title(Span::styled(
            " ✦ Add Log Entry ",
            Style::default().fg(Color::Magenta).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let rows = Layout::vertical([
        Constraint::Length(1), // level picker
        Constraint::Length(1), // spacer
        Constraint::Length(3), // message input
        Constraint::Length(1), // spacer
        Constraint::Length(1), // hint
    ])
    .split(inner);

    // Level selector
    let levels = ["INFO ", "WARN ", "ERROR", "DEBUG"];
    let l_colors = [C_ACCENT, C_WARN, C_ERROR, C_DIM];
    let mut level_spans = vec![Span::raw("  Level (Tab): ")];
    for (i, (lbl, &col)) in levels.iter().zip(l_colors.iter()).enumerate() {
        if i as u8 == app.log_level {
            level_spans.push(Span::styled(
                format!("[{lbl}]"),
                Style::default().fg(col).bold().underlined(),
            ));
        } else {
            level_spans.push(Span::styled(format!(" {lbl} "), Style::default().fg(C_DIM)));
        }
        level_spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(level_spans)), rows[0]);

    // Message input
    let display = format!("{}▌", app.log_input);
    frame.render_widget(
        Paragraph::new(display).block(
            Block::default()
                .title(" Message ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(C_ACCENT)),
        ),
        rows[2],
    );

    frame.render_widget(
        Paragraph::new(Span::styled(
            "  [Enter] Add   [Tab] Cycle level   [Esc] Cancel",
            Style::default().fg(C_DIM),
        )),
        rows[4],
    );
}

// ─── Helper ───────────────────────────────────────────────────────────────────

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let vert_pad = r.height.saturating_sub(height) / 2;
    let vert = Layout::vertical([
        Constraint::Length(vert_pad),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vert[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_app() -> App {
        let mut app = App::new();
        let mut a = Agent::new(
            "Tester",
            "gpt-4o",
            "a fairly long task description that gets truncated",
        );
        a.add_log(LogLevel::Warning, "careful now");
        a.set_status(AgentStatus::Running);
        app.agents.push(a);
        app.agents
            .push(Agent::new("Second", "claude", "second task"));
        app
    }

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn status_color_covers_all_variants() {
        assert_eq!(status_color(&AgentStatus::Running), C_SUCCESS);
        assert_eq!(status_color(&AgentStatus::Idle), C_WARN);
        assert_eq!(status_color(&AgentStatus::Error), C_ERROR);
        assert_eq!(status_color(&AgentStatus::Completed), C_DONE);
    }

    #[test]
    fn log_level_color_covers_all_variants() {
        assert_eq!(log_level_color(&LogLevel::Info), C_ACCENT);
        assert_eq!(log_level_color(&LogLevel::Warning), C_WARN);
        assert_eq!(log_level_color(&LogLevel::Error), C_ERROR);
        assert_eq!(log_level_color(&LogLevel::Debug), C_DIM);
    }

    #[test]
    fn centered_rect_is_centered_and_within_bounds() {
        let area = Rect::new(0, 0, 80, 24);
        let r = centered_rect(58, 16, area);
        assert_eq!(r.height, 16);
        assert_eq!(r.y, 4); // (24 - 16) / 2
        assert!(r.x > 0);
        assert!(r.x + r.width <= area.width);
        assert!(r.y + r.height <= area.height);
    }

    #[test]
    fn centered_rect_handles_height_larger_than_area() {
        let area = Rect::new(0, 0, 80, 10);
        let r = centered_rect(58, 16, area);
        // vert_pad saturates to 0; the result must still fit inside the area.
        assert!(r.y + r.height <= area.height);
    }

    #[test]
    fn draw_normal_renders_panel_titles() {
        let text = render(&sample_app());
        assert!(text.contains("Agents"));
        assert!(text.contains("Logs"));
    }

    #[test]
    fn draw_empty_shows_empty_state() {
        let text = render(&App::new());
        assert!(text.contains("No agents yet"));
    }

    #[test]
    fn draw_renders_every_modal_without_panicking() {
        for mode in [
            AppMode::AddAgent,
            AppMode::ConfirmDelete,
            AppMode::ChangeStatus,
            AppMode::AddLog,
        ] {
            let mut app = sample_app();
            app.mode = mode;
            let _ = render(&app);
        }
    }

    #[test]
    fn draw_add_agent_modal_shows_its_title() {
        let mut app = sample_app();
        app.mode = AppMode::AddAgent;
        assert!(render(&app).contains("Add New Agent"));
    }

    #[test]
    fn tail_chars_keeps_the_end_and_passes_short_strings_through() {
        assert_eq!(tail_chars("hello", 10), "hello");
        assert_eq!(tail_chars("hello world", 5), "world");
        assert_eq!(tail_chars("hi", 0), "");
        // counts by char, not byte — multi-byte chars stay intact
        assert_eq!(tail_chars("aé⠿z", 2), "⠿z");
    }

    #[tokio::test]
    async fn draw_renders_live_partial_for_running_agent() {
        let mut app = sample_app();
        let id = app.agents[0].id.clone();
        app.mark_running_for_test(&id);
        app.agents[0].partial = "streaming tokens here".into();
        app.selected = 0;
        let text = render(&app);
        assert!(text.contains("streaming tokens here"));
        // the live cursor is drawn after the streamed text
        assert!(text.contains('▌'));
    }

    #[tokio::test]
    async fn idle_agent_shows_no_live_partial() {
        // partial without a tracked run must not render (it's stale state).
        let mut app = sample_app();
        app.agents[0].partial = "should not show".into();
        app.selected = 0;
        assert!(!render(&app).contains("should not show"));
    }
}
