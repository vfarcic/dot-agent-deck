use std::fmt;

use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::state::{AppState, SessionState, SessionStatus, SharedState};

impl fmt::Display for crate::event::AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            crate::event::AgentType::ClaudeCode => write!(f, "ClaudeCode"),
        }
    }
}

pub fn run_tui(state: SharedState) -> std::io::Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();

    loop {
        let snapshot = state.blocking_read().clone();
        terminal.draw(|frame| render_frame(frame, &snapshot))?;

        if crossterm::event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
            }
        }
    }

    ratatui::restore();
    Ok(())
}

fn render_frame(frame: &mut Frame, state: &AppState) {
    let area = frame.area();

    if state.sessions.is_empty() {
        let msg = Paragraph::new("No active sessions. Waiting for connections...")
            .style(Style::default().fg(Color::DarkGray))
            .centered();
        // Center vertically
        let vertical = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(area);
        frame.render_widget(msg, vertical[1]);
        return;
    }

    // Sort sessions by started_at
    let mut sessions: Vec<&SessionState> = state.sessions.values().collect();
    sessions.sort_by_key(|s| s.started_at);

    // Title bar
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " dot-agent-deck ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("— {} session(s)", sessions.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let mut constraints: Vec<Constraint> = vec![Constraint::Length(1)]; // title
    for _ in &sessions {
        constraints.push(Constraint::Length(6));
    }
    constraints.push(Constraint::Min(0)); // filler

    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(title, chunks[0]);

    for (i, session) in sessions.iter().enumerate() {
        render_session_card(frame, chunks[i + 1], session);
    }
}

fn render_session_card(frame: &mut Frame, area: Rect, session: &SessionState) {
    let (status_label, status_style) = status_style(&session.status);

    // Truncate session_id to 11 chars
    let id_display = if session.session_id.len() > 11 {
        &session.session_id[..11]
    } else {
        &session.session_id
    };

    let title_left = format!(" {} · {} ", session.agent_type, id_display);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title_left)
        .title_alignment(ratatui::layout::Alignment::Left)
        .title(Line::from(Span::styled(
            format!(" {} {} ", "●", status_label),
            status_style,
        )).alignment(ratatui::layout::Alignment::Right));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cwd_display = session
        .cwd
        .as_deref()
        .unwrap_or("—");

    let elapsed = format_elapsed(session.last_activity);

    let lines = vec![
        Line::from(vec![
            Span::styled("Dir:  ", Style::default().fg(Color::DarkGray)),
            Span::raw(cwd_display),
        ]),
        Line::from(vec![
            Span::styled(
                if session.active_tool.is_some() {
                    "Tool: "
                } else {
                    "Status: "
                },
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(if let Some(ref tool) = session.active_tool {
                let detail = tool.detail.as_deref().unwrap_or("");
                if detail.is_empty() {
                    tool.name.clone()
                } else {
                    format!("{} — {}", tool.name, detail)
                }
            } else {
                status_label.to_string()
            }),
        ]),
        Line::from(vec![
            Span::styled("Last: ", Style::default().fg(Color::DarkGray)),
            Span::raw(elapsed),
        ]),
    ];

    let content = Paragraph::new(lines);
    frame.render_widget(content, inner);
}

fn status_style(status: &SessionStatus) -> (&str, Style) {
    match status {
        SessionStatus::Working => ("Working", Style::default().fg(Color::Yellow)),
        SessionStatus::WaitingForInput => (
            "Needs Input",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        SessionStatus::Idle => ("Idle", Style::default().fg(Color::Green)),
        SessionStatus::Error => ("Error", Style::default().fg(Color::Red)),
    }
}

fn format_elapsed(last_activity: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(last_activity);
    let total_secs = delta.num_seconds().max(0);

    if total_secs < 60 {
        format!("{}s ago", total_secs)
    } else if total_secs < 3600 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        if secs == 0 {
            format!("{}m ago", mins)
        } else {
            format!("{}m {}s ago", mins, secs)
        }
    } else {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        if mins == 0 {
            format!("{}h ago", hours)
        } else {
            format!("{}h {}m ago", hours, mins)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentType, EventType};
    use chrono::{Duration, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashMap;

    #[test]
    fn test_format_elapsed() {
        let now = Utc::now();
        assert_eq!(format_elapsed(now), "0s ago");
        assert_eq!(format_elapsed(now - Duration::seconds(3)), "3s ago");
        assert_eq!(format_elapsed(now - Duration::seconds(90)), "1m 30s ago");
        assert_eq!(format_elapsed(now - Duration::seconds(60)), "1m ago");
        assert_eq!(format_elapsed(now - Duration::seconds(3900)), "1h 5m ago");
        assert_eq!(format_elapsed(now - Duration::seconds(3600)), "1h ago");
    }

    #[test]
    fn test_status_style() {
        let (label, style) = status_style(&SessionStatus::Working);
        assert_eq!(label, "Working");
        assert_eq!(style.fg, Some(Color::Yellow));

        let (label, style) = status_style(&SessionStatus::WaitingForInput);
        assert_eq!(label, "Needs Input");
        assert_eq!(style.fg, Some(Color::Red));

        let (label, _) = status_style(&SessionStatus::Idle);
        assert_eq!(label, "Idle");

        let (label, _) = status_style(&SessionStatus::Error);
        assert_eq!(label, "Error");
    }

    #[test]
    fn test_render_empty_state() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = AppState::default();
        terminal
            .draw(|frame| render_frame(frame, &state))
            .unwrap();
    }

    #[test]
    fn test_render_with_sessions() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::default();

        // Session 1: working with a tool
        let mut event1 = AgentEvent {
            session_id: "session-abc-123".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/home/user/project".to_string()),
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        };
        state.apply_event(event1.clone());

        event1.event_type = EventType::ToolStart;
        event1.tool_name = Some("Read".to_string());
        event1.tool_detail = Some("src/main.rs".to_string());
        state.apply_event(event1);

        // Session 2: idle
        let event2 = AgentEvent {
            session_id: "session-def-456".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/home/user/other".to_string()),
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        };
        state.apply_event(event2);

        terminal
            .draw(|frame| render_frame(frame, &state))
            .unwrap();
    }
}
