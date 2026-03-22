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

use crate::event::EventType;
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
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let mut tick: u64 = 0;

    loop {
        let snapshot = state.blocking_read().clone();
        terminal.draw(|frame| render_frame(frame, &snapshot, tick))?;
        tick = tick.wrapping_add(1);

        if crossterm::event::poll(std::time::Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && (key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            break;
        }
    }

    ratatui::restore();
    Ok(())
}

fn render_frame(frame: &mut Frame, state: &AppState, tick: u64) {
    let area = frame.area();

    if state.sessions.is_empty() {
        let msg = Paragraph::new("No active sessions. Waiting for connections...")
            .style(Style::default().fg(Color::Gray))
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
            Style::default().fg(Color::Gray),
        ),
    ]));

    let cols = grid_columns(area.width);
    let card_height = 8;

    // Group sessions into rows of `cols`
    let rows: Vec<&[&SessionState]> = sessions.chunks(cols).collect();

    let mut constraints: Vec<Constraint> = vec![Constraint::Length(1)]; // title
    for _ in &rows {
        constraints.push(Constraint::Length(card_height));
    }
    constraints.push(Constraint::Min(0)); // filler

    let row_chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(title, row_chunks[0]);

    for (row_idx, row) in rows.iter().enumerate() {
        let col_constraints: Vec<Constraint> = (0..cols)
            .map(|_| Constraint::Ratio(1, cols as u32))
            .collect();
        let col_chunks = Layout::horizontal(col_constraints).split(row_chunks[row_idx + 1]);

        for (col_idx, session) in row.iter().enumerate() {
            render_session_card(frame, col_chunks[col_idx], session, tick);
        }
    }
}

fn grid_columns(width: u16) -> usize {
    if width >= 180 {
        3
    } else if width >= 100 {
        2
    } else {
        1
    }
}

fn render_session_card(frame: &mut Frame, area: Rect, session: &SessionState, tick: u64) {
    let (status_label, status_style) = status_style(&session.status);
    let status_color = status_style.fg.unwrap_or(Color::Gray);

    // Truncate session_id to 11 chars
    let id_display = if session.session_id.len() > 11 {
        &session.session_id[..11]
    } else {
        &session.session_id
    };

    let title_left = format!(" {} · {} ", session.agent_type, id_display);

    let dot = flash_dot(&session.status, tick);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(status_color))
        .title(Span::styled(title_left, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
        .title_alignment(ratatui::layout::Alignment::Left)
        .title(Line::from(Span::styled(
            format!(" {} {} ", dot, status_label),
            status_style,
        )).alignment(ratatui::layout::Alignment::Right));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let w = inner.width as usize;
    let wide = w >= 60;

    let cwd_display = session
        .cwd
        .as_deref()
        .unwrap_or("—");

    let elapsed = format_elapsed(session.last_activity);

    let mut lines: Vec<Line<'_>> = Vec::new();

    if wide {
        // Dir + stats on one line
        lines.push(padded_line(
            vec![
                Span::styled("Dir:  ", Style::default().fg(Color::Gray)),
                Span::raw(cwd_display),
            ],
            vec![
                Span::styled("Last: ", Style::default().fg(Color::Gray)),
                Span::raw(format!("{}  ", elapsed)),
                Span::styled("Tools: ", Style::default().fg(Color::Gray)),
                Span::raw(session.tool_count.to_string()),
            ],
            w,
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Dir:  ", Style::default().fg(Color::Gray)),
            Span::raw(cwd_display),
        ]));
    }

    // Prompt line
    if let Some(ref prompt) = session.last_user_prompt {
        let max_prompt = w.saturating_sub(6); // 6 = "Prmt: "
        let display = if prompt.len() > max_prompt {
            format!("{}…", &prompt[..max_prompt])
        } else {
            prompt.clone()
        };
        lines.push(Line::from(vec![
            Span::styled("Prmt: ", Style::default().fg(Color::Gray)),
            Span::raw(display),
        ]));
    }

    // In narrow mode, stats are a separate line
    if !wide {
        lines.push(Line::from(vec![
            Span::styled("Last: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}  ", elapsed)),
            Span::styled("Tools: ", Style::default().fg(Color::Gray)),
            Span::raw(session.tool_count.to_string()),
        ]));
    }

    lines.push(Line::from(""));
    lines.extend(recent_tool_lines(session));

    let content = Paragraph::new(lines);
    frame.render_widget(content, inner);
}

/// Build a single line with left-aligned and right-aligned span groups,
/// padded with spaces to fill `width`.
fn padded_line<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, width: usize) -> Line<'a> {
    let left_len: usize = left.iter().map(|s| s.width()).sum();
    let right_len: usize = right.iter().map(|s| s.width()).sum();
    let gap = width.saturating_sub(left_len + right_len);

    let mut spans = left;
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.extend(right);
    Line::from(spans)
}

fn flash_dot(status: &SessionStatus, tick: u64) -> &'static str {
    if *status == SessionStatus::WaitingForInput && tick % 2 == 1 {
        " "
    } else {
        "●"
    }
}

fn recent_tool_lines(session: &SessionState) -> Vec<Line<'static>> {
    let tool_events: Vec<_> = session
        .recent_events
        .iter()
        .rev()
        .filter(|e| e.event_type == EventType::ToolStart)
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    tool_events
        .into_iter()
        .map(|e| {
            let name = e.tool_name.as_deref().unwrap_or("?");
            let detail = e.tool_detail.as_deref().unwrap_or("");
            let text = if detail.is_empty() {
                format!("  {}", name)
            } else {
                format!("  {} — {}", name, detail)
            };
            Line::styled(text, Style::default().fg(Color::Rgb(140, 140, 140)))
        })
        .collect()
}

fn status_style(status: &SessionStatus) -> (&str, Style) {
    match status {
        SessionStatus::Thinking => ("Thinking", Style::default().fg(Color::Cyan)),
        SessionStatus::Working => ("Working", Style::default().fg(Color::Yellow)),
        SessionStatus::Compacting => ("Compacting", Style::default().fg(Color::Blue)),
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
        let (label, style) = status_style(&SessionStatus::Thinking);
        assert_eq!(label, "Thinking");
        assert_eq!(style.fg, Some(Color::Cyan));

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
            .draw(|frame| render_frame(frame, &state, 0))
            .unwrap();
    }

    #[test]
    fn test_render_with_sessions() {
        let backend = TestBackend::new(80, 30);
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
            user_prompt: None,
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
            user_prompt: None,
            metadata: HashMap::new(),
        };
        state.apply_event(event2);

        terminal
            .draw(|frame| render_frame(frame, &state, 0))
            .unwrap();
    }

    #[test]
    fn test_recent_tool_lines() {
        use crate::state::SessionState;
        use std::collections::VecDeque;

        let mut events = VecDeque::new();
        for (name, detail) in [("Read", "src/main.rs"), ("Write", "out.txt"), ("Bash", ""), ("Grep", "pattern")] {
            events.push_back(AgentEvent {
                session_id: "s1".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: EventType::ToolStart,
                tool_name: Some(name.to_string()),
                tool_detail: if detail.is_empty() { None } else { Some(detail.to_string()) },
                cwd: None,
                timestamp: Utc::now(),
                user_prompt: None,
                metadata: HashMap::new(),
            });
        }

        let session = SessionState {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            cwd: None,
            status: crate::state::SessionStatus::Idle,
            active_tool: None,
            started_at: Utc::now(),
            last_activity: Utc::now(),
            recent_events: events,
            tool_count: 0,
            last_user_prompt: None,
        };

        let lines = recent_tool_lines(&session);
        assert_eq!(lines.len(), 3);
        // Should be the last 3 ToolStart events in chronological order
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text[0], "  Write — out.txt");
        assert_eq!(text[1], "  Bash");
        assert_eq!(text[2], "  Grep — pattern");
    }

    #[test]
    fn test_prompt_display_in_card() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::default();
        let mut event = AgentEvent {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/tmp".to_string()),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
        };
        state.apply_event(event.clone());

        // Send a thinking event with a prompt
        event.event_type = EventType::Thinking;
        event.user_prompt = Some("fix the login bug".to_string());
        state.apply_event(event);

        // Should render without panic and session should have prompt
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the login bug")
        );

        terminal
            .draw(|frame| render_frame(frame, &state, 0))
            .unwrap();
    }

    #[test]
    fn test_flash_dot() {
        assert_eq!(flash_dot(&crate::state::SessionStatus::WaitingForInput, 0), "●");
        assert_eq!(flash_dot(&crate::state::SessionStatus::WaitingForInput, 1), " ");
        assert_eq!(flash_dot(&crate::state::SessionStatus::WaitingForInput, 2), "●");
        assert_eq!(flash_dot(&crate::state::SessionStatus::Working, 0), "●");
        assert_eq!(flash_dot(&crate::state::SessionStatus::Working, 1), "●");
        assert_eq!(flash_dot(&crate::state::SessionStatus::Idle, 1), "●");
    }

    #[test]
    fn test_grid_columns() {
        assert_eq!(grid_columns(79), 1);
        assert_eq!(grid_columns(99), 1);
        assert_eq!(grid_columns(100), 2);
        assert_eq!(grid_columns(150), 2);
        assert_eq!(grid_columns(179), 2);
        assert_eq!(grid_columns(180), 3);
        assert_eq!(grid_columns(250), 3);
    }

    #[test]
    fn test_render_wide_grid_layout() {
        // 120 wide = 2 columns, 2 sessions should render in one row
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::default();
        for id in ["s1", "s2", "s3"] {
            state.apply_event(AgentEvent {
                session_id: id.to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: EventType::SessionStart,
                tool_name: None,
                tool_detail: None,
                cwd: Some("/tmp".to_string()),
                timestamp: Utc::now(),
                user_prompt: None,
                metadata: HashMap::new(),
            });
        }

        // 3 sessions at 2 cols = 2 rows, needs 1 (title) + 10 + 10 = 21, we have 20 — still renders
        terminal
            .draw(|frame| render_frame(frame, &state, 0))
            .unwrap();
    }
}
