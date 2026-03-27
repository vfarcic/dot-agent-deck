use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::config::{BellConfig, DashboardConfig};
use crate::event::EventType;
use crate::pane::PaneController;
use crate::state::{AppState, SessionState, SessionStatus, SharedState};

impl fmt::Display for crate::event::AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            crate::event::AgentType::ClaudeCode => write!(f, "ClaudeCode"),
        }
    }
}

// ---------------------------------------------------------------------------
// Platform-aware modifier key label
// ---------------------------------------------------------------------------

const MOD_KEY: &str = if cfg!(target_os = "macos") {
    "Opt"
} else {
    "Alt"
};

// ---------------------------------------------------------------------------
// UI state types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum UiMode {
    Normal,
    Filter,
    Help,
    Rename,
    DirPicker,
    NewPaneForm,
}

struct DirPickerState {
    current_dir: PathBuf,
    entries: Vec<PathBuf>,
    selected: usize,
    scroll_offset: usize,
}

impl DirPickerState {
    fn new(start: PathBuf) -> Self {
        let mut state = Self {
            current_dir: start,
            entries: Vec::new(),
            selected: 0,
            scroll_offset: 0,
        };
        state.refresh();
        state
    }

    fn refresh(&mut self) {
        self.entries.clear();
        // Add parent directory entry if not at root
        if self.current_dir.parent().is_some() {
            self.entries.push(PathBuf::from(".."));
        }
        if let Ok(read_dir) = std::fs::read_dir(&self.current_dir) {
            let mut dirs: Vec<PathBuf> = read_dir
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                .filter(|e| {
                    !e.file_name()
                        .to_str()
                        .map(|n| n.starts_with('.'))
                        .unwrap_or(false)
                })
                .map(|e| e.path())
                .collect();
            dirs.sort();
            self.entries.extend(dirs);
        }
        self.selected = 0;
        self.scroll_offset = 0;
    }

    fn enter_selected(&mut self) {
        if let Some(path) = self.entries.get(self.selected) {
            if path == &PathBuf::from("..") {
                self.go_up();
                return;
            }
            self.current_dir = path.clone();
            self.refresh();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.refresh();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Name,
    Command,
}

struct NewPaneFormState {
    dir: PathBuf,
    name: String,
    command: String,
    focused: FormField,
}

struct UiState {
    mode: UiMode,
    selected_index: usize,
    filter_text: String,
    rename_text: String,
    display_names: HashMap<String, String>,
    columns: usize,
    scroll_offset: usize,
    status_message: Option<(String, std::time::Instant)>,
    dir_picker: Option<DirPickerState>,
    new_pane_form: Option<NewPaneFormState>,
    pane_names: HashMap<String, String>,
    /// Maps pane_id → display name; survives session restarts (e.g. /clear).
    pane_display_names: HashMap<String, String>,
    config: DashboardConfig,
    /// Tracks last-seen status per session for bell transition detection.
    last_bell_status: HashMap<String, SessionStatus>,
}

impl UiState {
    fn new(config: DashboardConfig) -> Self {
        Self {
            mode: UiMode::Normal,
            selected_index: 0,
            filter_text: String::new(),
            rename_text: String::new(),
            display_names: HashMap::new(),
            columns: 1,
            scroll_offset: 0,
            status_message: None,
            dir_picker: None,
            new_pane_form: None,
            pane_names: HashMap::new(),
            pane_display_names: HashMap::new(),
            config,
            last_bell_status: HashMap::new(),
        }
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new(DashboardConfig::default())
    }
}

// ---------------------------------------------------------------------------
// Grid navigation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn navigate_grid(current: usize, dir: Direction, columns: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let row = current / columns;
    let col = current % columns;
    let total_rows = total.div_ceil(columns);

    match dir {
        Direction::Up => {
            if row > 0 {
                (row - 1) * columns + col
            } else {
                current
            }
        }
        Direction::Down => {
            let new_idx = (row + 1) * columns + col;
            if row + 1 < total_rows && new_idx < total {
                new_idx
            } else if row + 1 < total_rows {
                // Last row has fewer items; go to last item
                total - 1
            } else {
                current
            }
        }
        Direction::Left => {
            if col > 0 {
                current - 1
            } else {
                current
            }
        }
        Direction::Right => {
            if col + 1 < columns && current + 1 < total {
                current + 1
            } else {
                current
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session filtering
// ---------------------------------------------------------------------------

fn filter_sessions<'a>(state: &'a AppState, ui: &UiState) -> Vec<(&'a String, &'a SessionState)> {
    let mut sessions: Vec<(&String, &SessionState)> = state.sessions.iter().collect();
    sessions.sort_by_key(|(_, s)| s.started_at);

    if ui.filter_text.is_empty() {
        return sessions;
    }

    let query = ui.filter_text.to_lowercase();
    sessions.retain(|(id, s)| {
        let id_match = id.to_lowercase().contains(&query);
        let cwd_match = s
            .cwd
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains(&query);
        let status_str = format!("{:?}", s.status).to_lowercase();
        let status_match = status_str.contains(&query);
        let name_match = ui
            .display_names
            .get(*id)
            .map(|n| n.to_lowercase().contains(&query))
            .unwrap_or(false);
        id_match || cwd_match || status_match || name_match
    });
    sessions
}

// ---------------------------------------------------------------------------
// Bell transition detection
// ---------------------------------------------------------------------------

fn compute_bell_needed(
    sessions: &HashMap<String, SessionState>,
    last_bell_status: &HashMap<String, SessionStatus>,
    bell_config: &BellConfig,
) -> (bool, HashMap<String, SessionStatus>) {
    let mut need_bell = false;
    let mut new_status_map = HashMap::with_capacity(sessions.len());

    for (id, session) in sessions {
        let current = &session.status;
        let changed = last_bell_status.get(id) != Some(current);

        if changed && bell_config.should_bell(current) {
            need_bell = true;
        }

        new_status_map.insert(id.clone(), current.clone());
    }

    (need_bell, new_status_map)
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

struct NewPaneRequest {
    dir: PathBuf,
    name: String,
    command: String,
}

enum KeyResult {
    Continue,
    Quit,
    Focus,
    NewPane(NewPaneRequest),
    ClosePane,
}

/// Detect Alt+1 … Alt+9 and return the digit (1–9).
fn alt_digit(key: KeyEvent) -> Option<u8> {
    if key.modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c @ '1'..='9') = key.code
    {
        return Some(c as u8 - b'0');
    }
    None
}

fn handle_normal_key(key: KeyEvent, ui: &mut UiState, total: usize) -> KeyResult {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyResult::Quit;
    }
    match key.code {
        KeyCode::Char('q') => KeyResult::Quit,
        KeyCode::Char('j') | KeyCode::Down => {
            ui.selected_index =
                navigate_grid(ui.selected_index, Direction::Down, ui.columns, total);
            KeyResult::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            ui.selected_index = navigate_grid(ui.selected_index, Direction::Up, ui.columns, total);
            KeyResult::Continue
        }
        KeyCode::Char('h') | KeyCode::Left => {
            ui.selected_index =
                navigate_grid(ui.selected_index, Direction::Left, ui.columns, total);
            KeyResult::Continue
        }
        KeyCode::Char('l') | KeyCode::Right => {
            ui.selected_index =
                navigate_grid(ui.selected_index, Direction::Right, ui.columns, total);
            KeyResult::Continue
        }
        KeyCode::Char('/') => {
            ui.mode = UiMode::Filter;
            ui.filter_text.clear();
            KeyResult::Continue
        }
        KeyCode::Char('?') => {
            ui.mode = UiMode::Help;
            KeyResult::Continue
        }
        KeyCode::Char('r') if total > 0 => {
            ui.mode = UiMode::Rename;
            ui.rename_text.clear();
            KeyResult::Continue
        }
        KeyCode::Char('n') => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            ui.dir_picker = Some(DirPickerState::new(cwd));
            ui.mode = UiMode::DirPicker;
            KeyResult::Continue
        }
        KeyCode::Char('d') if total > 0 => KeyResult::ClosePane,
        KeyCode::Enter if total > 0 => KeyResult::Focus,
        KeyCode::Esc => {
            if !ui.filter_text.is_empty() {
                ui.filter_text.clear();
            }
            KeyResult::Continue
        }
        _ => KeyResult::Continue,
    }
}

fn handle_filter_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyResult::Quit;
    }
    match key.code {
        KeyCode::Esc => {
            ui.filter_text.clear();
            ui.mode = UiMode::Normal;
        }
        KeyCode::Enter => {
            ui.mode = UiMode::Normal;
        }
        KeyCode::Backspace => {
            ui.filter_text.pop();
        }
        KeyCode::Char(c) => {
            ui.filter_text.push(c);
        }
        _ => {}
    }
    KeyResult::Continue
}

fn handle_help_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    match key.code {
        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
            ui.mode = UiMode::Normal;
        }
        _ => {}
    }
    KeyResult::Continue
}

fn handle_rename_key(
    key: KeyEvent,
    ui: &mut UiState,
    selected_session_id: Option<&str>,
) -> KeyResult {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyResult::Quit;
    }
    match key.code {
        KeyCode::Esc => {
            ui.rename_text.clear();
            ui.mode = UiMode::Normal;
        }
        KeyCode::Enter => {
            if let Some(id) = selected_session_id {
                if ui.rename_text.is_empty() {
                    ui.display_names.remove(id);
                } else {
                    ui.display_names
                        .insert(id.to_string(), ui.rename_text.clone());
                }
            }
            ui.rename_text.clear();
            ui.mode = UiMode::Normal;
        }
        KeyCode::Backspace => {
            ui.rename_text.pop();
        }
        KeyCode::Char(c) => {
            ui.rename_text.push(c);
        }
        _ => {}
    }
    KeyResult::Continue
}

fn handle_dir_picker_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyResult::Quit;
    }
    let picker = match ui.dir_picker.as_mut() {
        Some(p) => p,
        None => {
            ui.mode = UiMode::Normal;
            return KeyResult::Continue;
        }
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            ui.dir_picker = None;
            ui.mode = UiMode::Normal;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if !picker.entries.is_empty() {
                picker.selected = (picker.selected + 1).min(picker.entries.len() - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            picker.selected = picker.selected.saturating_sub(1);
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
            // If no subdirs, select current directory
            if picker.entries.is_empty() {
                transition_to_form(ui);
                return KeyResult::Continue;
            }
            picker.enter_selected();
        }
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
            picker.go_up();
        }
        KeyCode::Char(' ') => {
            transition_to_form(ui);
            return KeyResult::Continue;
        }
        _ => {}
    }
    KeyResult::Continue
}

fn transition_to_form(ui: &mut UiState) {
    let dir = ui
        .dir_picker
        .as_ref()
        .map(|p| p.current_dir.clone())
        .unwrap_or_default();
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let command = ui.config.default_command.clone();
    ui.dir_picker = None;
    ui.new_pane_form = Some(NewPaneFormState {
        dir,
        name,
        command,
        focused: FormField::Name,
    });
    ui.mode = UiMode::NewPaneForm;
}

fn handle_new_pane_form_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyResult::Quit;
    }
    let form = match ui.new_pane_form.as_mut() {
        Some(f) => f,
        None => {
            ui.mode = UiMode::Normal;
            return KeyResult::Continue;
        }
    };
    match key.code {
        KeyCode::Esc => {
            ui.new_pane_form = None;
            ui.mode = UiMode::Normal;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            form.focused = match form.focused {
                FormField::Name => FormField::Command,
                FormField::Command => FormField::Name,
            };
        }
        KeyCode::Enter => match form.focused {
            FormField::Name => {
                form.focused = FormField::Command;
            }
            FormField::Command => {
                let req = NewPaneRequest {
                    dir: form.dir.clone(),
                    name: form.name.clone(),
                    command: form.command.clone(),
                };
                ui.new_pane_form = None;
                ui.mode = UiMode::Normal;
                return KeyResult::NewPane(req);
            }
        },
        KeyCode::Backspace => {
            let field = match form.focused {
                FormField::Name => &mut form.name,
                FormField::Command => &mut form.command,
            };
            field.pop();
        }
        KeyCode::Char(c) => {
            let field = match form.focused {
                FormField::Name => &mut form.name,
                FormField::Command => &mut form.command,
            };
            field.push(c);
        }
        _ => {}
    }
    KeyResult::Continue
}

// ---------------------------------------------------------------------------
// TUI entry point
// ---------------------------------------------------------------------------

pub fn run_tui(
    state: SharedState,
    pane: Box<dyn PaneController>,
    config: DashboardConfig,
) -> std::io::Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let mut tick: u64 = 0;
    let mut ui = UiState::new(config);

    loop {
        // Expire stale status messages
        if let Some((_, created)) = &ui.status_message
            && created.elapsed() > std::time::Duration::from_secs(3)
        {
            ui.status_message = None;
        }

        let snapshot = state.blocking_read().clone();

        // Apply pending pane names to sessions that have appeared
        if !ui.pane_names.is_empty() {
            for (sid, session) in &snapshot.sessions {
                if let Some(ref pane_id) = session.pane_id
                    && let Some(name) = ui.pane_names.remove(pane_id)
                {
                    ui.pane_display_names.insert(pane_id.clone(), name.clone());
                    ui.display_names.insert(sid.clone(), name);
                }
            }
        }

        // Restore display names for sessions whose pane already has a name
        // (handles session restart after /clear)
        for (sid, session) in &snapshot.sessions {
            if let Some(ref pane_id) = session.pane_id
                && !ui.display_names.contains_key(sid)
                && let Some(name) = ui.pane_display_names.get(pane_id).cloned()
            {
                ui.display_names.insert(sid.clone(), name);
            }
        }

        let filtered = filter_sessions(&snapshot, &ui);
        let total = filtered.len();

        // Clamp selection
        if total > 0 {
            ui.selected_index = ui.selected_index.min(total - 1);
        } else {
            ui.selected_index = 0;
        }

        ui.columns = grid_columns(terminal.get_frame().area().width);

        let has_pane_control = pane.is_available();
        terminal.draw(|frame| {
            render_frame(frame, &snapshot, &mut ui, &filtered, tick, has_pane_control);
        })?;
        tick = tick.wrapping_add(1);

        // Bell transition detection
        let (need_bell, new_bell_status) =
            compute_bell_needed(&snapshot.sessions, &ui.last_bell_status, &ui.config.bell);
        ui.last_bell_status = new_bell_status;
        if need_bell {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b"\x07");
            let _ = std::io::stdout().flush();
        }

        if crossterm::event::poll(std::time::Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            // Alt+1..9: jump to card N and focus its pane (works from any mode)
            if let Some(card_num) = alt_digit(key) {
                let idx = (card_num as usize).saturating_sub(1);
                if idx < total {
                    ui.selected_index = idx;
                    ui.mode = UiMode::Normal;
                    if let Some((sid, _)) = filtered.get(idx)
                        && let Some(session) = snapshot.sessions.get(*sid)
                    {
                        if let Some(ref pane_id) = session.pane_id {
                            match pane.focus_pane(pane_id) {
                                Ok(()) => {}
                                Err(e) => {
                                    state.blocking_write().sessions.remove(*sid);
                                    ui.status_message = Some((
                                        format!("Removed stale session: {e}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        } else {
                            ui.status_message = Some((
                                format!("No pane linked to session {sid}"),
                                std::time::Instant::now(),
                            ));
                        }
                    }
                }
                continue;
            }

            let selected_id: Option<String> =
                filtered.get(ui.selected_index).map(|(id, _)| (*id).clone());

            let result = match ui.mode {
                UiMode::Normal => handle_normal_key(key, &mut ui, total),
                UiMode::Filter => handle_filter_key(key, &mut ui),
                UiMode::Help => handle_help_key(key, &mut ui),
                UiMode::Rename => handle_rename_key(key, &mut ui, selected_id.as_deref()),
                UiMode::DirPicker => handle_dir_picker_key(key, &mut ui),
                UiMode::NewPaneForm => handle_new_pane_form_key(key, &mut ui),
            };

            match result {
                KeyResult::Quit => break,
                KeyResult::Focus => {
                    if let Some(ref sid) = selected_id
                        && let Some(session) = snapshot.sessions.get(sid)
                    {
                        if let Some(ref pane_id) = session.pane_id {
                            match pane.focus_pane(pane_id) {
                                Ok(()) => {}
                                Err(e) => {
                                    state.blocking_write().sessions.remove(sid);
                                    ui.status_message = Some((
                                        format!("Removed stale session: {e}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        } else {
                            ui.status_message = Some((
                                format!("No pane linked to session {sid}"),
                                std::time::Instant::now(),
                            ));
                        }
                    }
                }
                KeyResult::NewPane(req) => {
                    if pane.is_available() {
                        let dir_str = req.dir.display().to_string();
                        let cmd = if req.command.is_empty() {
                            None
                        } else {
                            Some(req.command.as_str())
                        };
                        match pane.create_pane(cmd, Some(&dir_str)) {
                            Ok(new_id) => {
                                if !req.name.is_empty() {
                                    let _ = pane.rename_pane(&new_id, &req.name);
                                    ui.pane_display_names
                                        .insert(new_id.clone(), req.name.clone());
                                    ui.pane_names.insert(new_id.clone(), req.name);
                                }
                                ui.status_message = Some((
                                    format!("Created pane {new_id} in {dir_str}"),
                                    std::time::Instant::now(),
                                ));
                            }
                            Err(e) => {
                                ui.status_message = Some((
                                    format!("New pane failed: {e}"),
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                }
                KeyResult::ClosePane => {
                    if let Some(ref sid) = selected_id
                        && let Some(session) = snapshot.sessions.get(sid)
                    {
                        if let Some(ref pane_id) = session.pane_id {
                            match pane.close_pane(pane_id) {
                                Ok(()) => {
                                    state.blocking_write().sessions.remove(sid);
                                    ui.status_message = Some((
                                        format!("Closed pane {pane_id}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    state.blocking_write().sessions.remove(sid);
                                    ui.status_message = Some((
                                        format!("Removed stale session: {e}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        } else {
                            ui.status_message = Some((
                                format!("No pane linked to session {sid}"),
                                std::time::Instant::now(),
                            ));
                        }
                    }
                }
                KeyResult::Continue => {}
            }

            // Keep pane_display_names in sync with display_names
            // so renames persist across session restarts.
            for (sid, session) in &snapshot.sessions {
                if let Some(ref pane_id) = session.pane_id {
                    if let Some(name) = ui.display_names.get(sid) {
                        ui.pane_display_names.insert(pane_id.clone(), name.clone());
                    } else {
                        ui.pane_display_names.remove(pane_id);
                    }
                }
            }
        }
    }

    ratatui::restore();
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_frame(
    frame: &mut Frame,
    state: &AppState,
    ui: &mut UiState,
    filtered: &[(&String, &SessionState)],
    tick: u64,
    has_pane_control: bool,
) {
    let area = frame.area();

    if state.sessions.is_empty() {
        let bar_height = if has_pane_control { 2 } else { 1 };
        let vertical = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(bar_height),
        ])
        .split(area);
        let msg = Paragraph::new("No active sessions. Press n to create a pane.")
            .style(Style::default().fg(Color::Gray))
            .centered();
        frame.render_widget(msg, vertical[1]);
        render_bottom_bar(frame, ui, vertical[3], has_pane_control);

        if ui.mode == UiMode::Help {
            render_help_overlay(frame, has_pane_control);
        }
        if ui.mode == UiMode::DirPicker
            && let Some(ref picker) = ui.dir_picker
        {
            render_dir_picker(frame, picker);
        }
        if ui.mode == UiMode::NewPaneForm
            && let Some(ref form) = ui.new_pane_form
        {
            render_new_pane_form(frame, form);
        }
        return;
    }

    let sessions: Vec<&SessionState> = filtered.iter().map(|(_, s)| *s).collect();
    let session_ids: Vec<&String> = filtered.iter().map(|(id, _)| *id).collect();

    let cols = grid_columns(area.width);
    let card_height = 8;

    // Determine if we need a bottom bar (status/filter/rename)
    let has_bottom_bar = true; // always show status bar
    let bottom_height: u16 = if has_bottom_bar { 1 } else { 0 };

    // Title bar
    let total_sessions = state.sessions.len();
    let showing = sessions.len();
    let title_text = if showing < total_sessions {
        format!("— {}/{} session(s)", showing, total_sessions)
    } else {
        format!("— {} session(s)", total_sessions)
    };
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " dot-agent-deck ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(title_text, Style::default().fg(Color::Gray)),
    ]));

    if sessions.is_empty() {
        // All filtered out
        let vertical = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(bottom_height),
        ])
        .split(area);
        frame.render_widget(title, vertical[0]);

        let msg = Paragraph::new("No sessions match filter.")
            .style(Style::default().fg(Color::Gray))
            .centered();
        let inner = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(vertical[1]);
        frame.render_widget(msg, inner[1]);

        render_bottom_bar(frame, ui, vertical[2], has_pane_control);
        return;
    }

    let all_rows: Vec<&[&SessionState]> = sessions.chunks(cols).collect();
    let all_row_ids: Vec<&[&String]> = session_ids.chunks(cols).collect();
    let total_rows = all_rows.len();

    // Calculate how many rows fit in the available area (title + bottom bar take space)
    let available = area.height.saturating_sub(1 + bottom_height);
    let visible_rows = (available / card_height).max(1) as usize;

    // Adjust scroll offset to keep selected row visible
    let selected_row = ui.selected_index / cols;
    if selected_row < ui.scroll_offset {
        ui.scroll_offset = selected_row;
    } else if selected_row >= ui.scroll_offset + visible_rows {
        ui.scroll_offset = selected_row + 1 - visible_rows;
    }

    let end = (ui.scroll_offset + visible_rows).min(total_rows);
    let rows = &all_rows[ui.scroll_offset..end];
    let row_ids = &all_row_ids[ui.scroll_offset..end];

    let mut constraints: Vec<Constraint> = vec![Constraint::Length(1)]; // title
    for _ in rows {
        constraints.push(Constraint::Length(card_height));
    }
    constraints.push(Constraint::Min(0)); // filler
    constraints.push(Constraint::Length(bottom_height)); // bottom bar

    let row_chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(title, row_chunks[0]);

    for (vi, (row, ids)) in rows.iter().zip(row_ids.iter()).enumerate() {
        let col_constraints: Vec<Constraint> = (0..cols)
            .map(|_| Constraint::Ratio(1, cols as u32))
            .collect();
        let col_chunks = Layout::horizontal(col_constraints).split(row_chunks[vi + 1]);

        for (col_idx, session) in row.iter().enumerate() {
            let flat_index = (ui.scroll_offset + vi) * cols + col_idx;
            let is_selected = flat_index == ui.selected_index;
            let display_name = ids.get(col_idx).and_then(|id| ui.display_names.get(*id));
            let card_number = {
                let n = flat_index + 1;
                if n <= 9 { Some(n as u8) } else { None }
            };
            render_session_card(
                frame,
                col_chunks[col_idx],
                session,
                tick,
                is_selected,
                display_name,
                card_number,
            );
        }
    }

    // Bottom bar
    let bottom_area = row_chunks[row_chunks.len() - 1];
    render_bottom_bar(frame, ui, bottom_area, has_pane_control);

    // Overlays (drawn last, on top)
    if ui.mode == UiMode::Help {
        render_help_overlay(frame, has_pane_control);
    }
    if ui.mode == UiMode::DirPicker
        && let Some(ref picker) = ui.dir_picker
    {
        render_dir_picker(frame, picker);
    }
    if ui.mode == UiMode::NewPaneForm
        && let Some(ref form) = ui.new_pane_form
    {
        render_new_pane_form(frame, form);
    }
}

fn render_bottom_bar(frame: &mut Frame, ui: &UiState, area: Rect, has_pane_control: bool) {
    match ui.mode {
        UiMode::Filter => {
            let line = Line::from(vec![
                Span::styled(
                    "/ ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(&ui.filter_text),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            // Show cursor
            let cursor_x = area.x + 2 + ui.filter_text.len() as u16;
            frame.set_cursor_position(Position::new(cursor_x, area.y));
        }
        UiMode::Rename => {
            let line = Line::from(vec![
                Span::styled(
                    "Rename: ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(&ui.rename_text),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            let cursor_x = area.x + 8 + ui.rename_text.len() as u16;
            frame.set_cursor_position(Position::new(cursor_x, area.y));
        }
        _ => {
            if let Some((ref msg, _)) = ui.status_message {
                let line = Line::styled(msg.as_str(), Style::default().fg(Color::Yellow));
                frame.render_widget(Paragraph::new(line), area);
            } else {
                let hints = if has_pane_control {
                    format!(
                        "?: help  {MOD_KEY}+1-9: jump  {MOD_KEY}+q: quit all  {MOD_KEY}+d: dashboard"
                    )
                } else {
                    format!("?: help  {MOD_KEY}+1-9: jump  q: quit")
                };
                let line = Line::styled(hints, Style::default().fg(Color::Gray));
                frame.render_widget(Paragraph::new(line), area);
            }
        }
    }
}

fn render_help_overlay(frame: &mut Frame, has_pane_control: bool) {
    let area = frame.area();
    let popup_width = 52.min(area.width.saturating_sub(4));
    let base_height: u16 = if has_pane_control { 30 } else { 17 };
    let popup_height = base_height.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let mut help_text = vec![
        Line::styled(
            "  Keybindings",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("  j / Down      Move down"),
        Line::from("  k / Up        Move up"),
        Line::from("  h / Left      Move left"),
        Line::from("  l / Right     Move right"),
        Line::from(format!("  {MOD_KEY}+1-9       Jump to card N")),
        Line::from("  /             Filter sessions"),
        Line::from("  r             Rename session"),
        Line::from("  ?             Toggle this help"),
        Line::from("  Esc           Clear filter"),
        Line::from("  q             Quit"),
    ];

    if has_pane_control {
        help_text.push(Line::from(""));
        help_text.push(Line::styled(
            "  Pane Control",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        help_text.push(Line::from(""));
        help_text.push(Line::from("  Enter         Focus agent pane"));
        help_text.push(Line::from("  n             New pane (dir + name + cmd)"));
        help_text.push(Line::from("  d             Close agent pane"));
        help_text.push(Line::from(""));
        help_text.push(Line::styled(
            "  Zellij (works from any pane)",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        help_text.push(Line::from(""));
        help_text.push(Line::from(format!("  {MOD_KEY}+h         Go to dashboard")));
        help_text.push(Line::from(format!(
            "  {MOD_KEY}+j/k       Navigate stacked panes"
        )));
        help_text.push(Line::from(format!(
            "  {MOD_KEY}+w         Close current pane"
        )));
        help_text.push(Line::from(format!("  {MOD_KEY}+q         Quit all")));
    }

    help_text.push(Line::from(""));
    help_text.push(Line::styled(
        "  Press ? or Esc to close",
        Style::default().fg(Color::Gray),
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let paragraph = Paragraph::new(help_text).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_dir_picker(frame: &mut Frame, picker: &DirPickerState) {
    let area = frame.area();
    let popup_width = 60.min(area.width.saturating_sub(4));
    let popup_height = 20u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    // Reserve lines: title(1) + current_dir(1) + blank(1) + footer(2) = 5
    let max_visible = (popup_height as usize).saturating_sub(5);

    let mut lines = vec![
        Line::styled(
            format!("  {}", picker.current_dir.display()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
    ];

    if picker.entries.is_empty() {
        lines.push(Line::styled(
            "  (no subdirectories)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        // Adjust scroll offset to keep selected visible
        let scroll = if picker.selected >= max_visible {
            picker.selected - max_visible + 1
        } else {
            0
        };

        for (i, entry) in picker
            .entries
            .iter()
            .enumerate()
            .skip(scroll)
            .take(max_visible)
        {
            let name = if entry == &PathBuf::from("..") {
                "..".to_string()
            } else {
                entry
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            };
            let prefix = if i == picker.selected { "> " } else { "  " };
            let style = if i == picker.selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let suffix = if name == ".." { "" } else { "/" };
            lines.push(Line::styled(format!("{prefix}{name}{suffix}"), style));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::styled(
        "  Space: select dir  Enter/l: open  h/BS: up  Esc: cancel",
        Style::default().fg(Color::Gray),
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Select Directory ")
        .border_style(Style::default().fg(Color::Cyan));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_new_pane_form(frame: &mut Frame, form: &NewPaneFormState) {
    let area = frame.area();
    let popup_width = 56.min(area.width.saturating_sub(4));
    let popup_height = 12u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let inner_width = popup_width.saturating_sub(4) as usize;

    let name_style = if form.focused == FormField::Name {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let cmd_style = if form.focused == FormField::Command {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let dir_display = form.dir.display().to_string();
    let lines = vec![
        Line::styled(
            format!("  Dir: {dir_display}"),
            Style::default().fg(Color::Yellow),
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Name:    ", name_style),
            Span::styled(
                format!(
                    "{:<width$}",
                    form.name,
                    width = inner_width.saturating_sub(11)
                ),
                if form.focused == FormField::Name {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Command: ", cmd_style),
            Span::styled(
                format!(
                    "{:<width$}",
                    form.command,
                    width = inner_width.saturating_sub(11)
                ),
                if form.focused == FormField::Command {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
        ]),
        Line::from(""),
        Line::from(""),
        Line::styled(
            "  Tab: switch field  Enter: next/confirm  Esc: cancel",
            Style::default().fg(Color::Gray),
        ),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" New Pane ")
        .border_style(Style::default().fg(Color::Cyan));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);

    // Show cursor in the active field
    let cursor_y = match form.focused {
        FormField::Name => popup_area.y + 3,
        FormField::Command => popup_area.y + 5,
    };
    let field_text = match form.focused {
        FormField::Name => &form.name,
        FormField::Command => &form.command,
    };
    let cursor_x = popup_area.x + 12 + field_text.len() as u16;
    frame.set_cursor_position(Position::new(cursor_x, cursor_y));
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

fn render_session_card(
    frame: &mut Frame,
    area: Rect,
    session: &SessionState,
    tick: u64,
    is_selected: bool,
    display_name: Option<&String>,
    card_number: Option<u8>,
) {
    let (status_label, status_style) = status_style(&session.status);
    let status_color = status_style.fg.unwrap_or(Color::Gray);

    let id_display = if session.session_id.len() > 11 {
        &session.session_id[..11]
    } else {
        &session.session_id
    };

    let num_prefix = match card_number {
        Some(n) => format!("{n} "),
        None => String::new(),
    };
    let title_left = if let Some(name) = display_name {
        format!(" {num_prefix}{} ", name)
    } else {
        format!(" {num_prefix}{} · {} ", session.agent_type, id_display)
    };

    let dot = flash_dot(&session.status, tick);

    let border_style = if is_selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(status_color)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            title_left,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Left)
        .title(
            Line::from(Span::styled(
                format!(" {} {} ", dot, status_label),
                status_style,
            ))
            .alignment(ratatui::layout::Alignment::Right),
        );

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let w = inner.width as usize;
    let wide = w >= 60;

    let cwd_display = session
        .cwd
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_name())
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| "—".into());

    let elapsed = format_elapsed(session.last_activity);

    let mut lines: Vec<Line<'_>> = Vec::new();

    if wide {
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

    if let Some(ref prompt) = session.last_user_prompt {
        let max_prompt = w.saturating_sub(6);
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
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;

    fn default_ui() -> UiState {
        UiState::default()
    }

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
        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| render_frame(frame, &state, &mut ui, &filtered, 0, false))
            .unwrap();
    }

    #[test]
    fn test_render_with_sessions() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::default();

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
            pane_id: None,
        };
        state.apply_event(event1.clone());

        event1.event_type = EventType::ToolStart;
        event1.tool_name = Some("Read".to_string());
        event1.tool_detail = Some("src/main.rs".to_string());
        state.apply_event(event1);

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
            pane_id: None,
        };
        state.apply_event(event2);

        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| render_frame(frame, &state, &mut ui, &filtered, 0, false))
            .unwrap();
    }

    #[test]
    fn test_recent_tool_lines() {
        use crate::state::SessionState;
        use std::collections::VecDeque;

        let mut events = VecDeque::new();
        for (name, detail) in [
            ("Read", "src/main.rs"),
            ("Write", "out.txt"),
            ("Bash", ""),
            ("Grep", "pattern"),
        ] {
            events.push_back(AgentEvent {
                session_id: "s1".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: EventType::ToolStart,
                tool_name: Some(name.to_string()),
                tool_detail: if detail.is_empty() {
                    None
                } else {
                    Some(detail.to_string())
                },
                cwd: None,
                timestamp: Utc::now(),
                user_prompt: None,
                metadata: HashMap::new(),
                pane_id: None,
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
            pane_id: None,
        };

        let lines = recent_tool_lines(&session);
        assert_eq!(lines.len(), 3);
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
            pane_id: None,
        };
        state.apply_event(event.clone());

        event.event_type = EventType::Thinking;
        event.user_prompt = Some("fix the login bug".to_string());
        state.apply_event(event);

        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the login bug")
        );

        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| render_frame(frame, &state, &mut ui, &filtered, 0, false))
            .unwrap();
    }

    #[test]
    fn test_flash_dot() {
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 0),
            "●"
        );
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 1),
            " "
        );
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 2),
            "●"
        );
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
                pane_id: None,
            });
        }

        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| render_frame(frame, &state, &mut ui, &filtered, 0, false))
            .unwrap();
    }

    // ---------------------------------------------------------------------------
    // Navigation tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_navigate_grid_single_column() {
        // 5 items, 1 column
        assert_eq!(navigate_grid(0, Direction::Down, 1, 5), 1);
        assert_eq!(navigate_grid(4, Direction::Down, 1, 5), 4); // at bottom
        assert_eq!(navigate_grid(0, Direction::Up, 1, 5), 0); // at top
        assert_eq!(navigate_grid(3, Direction::Up, 1, 5), 2);
        // Left/Right are no-ops in single column
        assert_eq!(navigate_grid(2, Direction::Left, 1, 5), 2);
        assert_eq!(navigate_grid(2, Direction::Right, 1, 5), 2);
    }

    #[test]
    fn test_navigate_grid_two_columns() {
        // 5 items, 2 columns:
        // [0] [1]
        // [2] [3]
        // [4]
        assert_eq!(navigate_grid(0, Direction::Right, 2, 5), 1);
        assert_eq!(navigate_grid(1, Direction::Left, 2, 5), 0);
        assert_eq!(navigate_grid(0, Direction::Down, 2, 5), 2);
        assert_eq!(navigate_grid(2, Direction::Up, 2, 5), 0);
        // Down from col 1 row 1 to col 1 row 2 — but index 5 doesn't exist, clamp to 4
        assert_eq!(navigate_grid(3, Direction::Down, 2, 5), 4);
        // Right from last item
        assert_eq!(navigate_grid(4, Direction::Right, 2, 5), 4);
        // Left from first col
        assert_eq!(navigate_grid(0, Direction::Left, 2, 5), 0);
    }

    #[test]
    fn test_navigate_grid_three_columns() {
        // 7 items, 3 columns:
        // [0] [1] [2]
        // [3] [4] [5]
        // [6]
        assert_eq!(navigate_grid(1, Direction::Down, 3, 7), 4);
        assert_eq!(navigate_grid(4, Direction::Up, 3, 7), 1);
        assert_eq!(navigate_grid(5, Direction::Down, 3, 7), 6); // col 2 row 2 -> last item
        assert_eq!(navigate_grid(6, Direction::Up, 3, 7), 3);
        assert_eq!(navigate_grid(2, Direction::Right, 3, 7), 2); // at right edge
    }

    #[test]
    fn test_navigate_grid_empty() {
        assert_eq!(navigate_grid(0, Direction::Down, 2, 0), 0);
        assert_eq!(navigate_grid(0, Direction::Up, 2, 0), 0);
    }

    #[test]
    fn test_navigate_grid_single_item() {
        assert_eq!(navigate_grid(0, Direction::Down, 2, 1), 0);
        assert_eq!(navigate_grid(0, Direction::Up, 2, 1), 0);
        assert_eq!(navigate_grid(0, Direction::Left, 2, 1), 0);
        assert_eq!(navigate_grid(0, Direction::Right, 2, 1), 0);
    }

    // ---------------------------------------------------------------------------
    // Filter tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_filter_sessions_no_filter() {
        let mut state = AppState::default();
        for id in ["a", "b", "c"] {
            state.apply_event(AgentEvent {
                session_id: id.to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: EventType::SessionStart,
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: Utc::now(),
                user_prompt: None,
                metadata: HashMap::new(),
                pane_id: None,
            });
        }

        let ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn test_filter_sessions_by_id() {
        let mut state = AppState::default();
        for id in ["alpha", "beta", "gamma"] {
            state.apply_event(AgentEvent {
                session_id: id.to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: EventType::SessionStart,
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: Utc::now(),
                user_prompt: None,
                metadata: HashMap::new(),
                pane_id: None,
            });
        }

        let mut ui = default_ui();
        ui.filter_text = "bet".to_string();
        let filtered = filter_sessions(&state, &ui);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "beta");
    }

    #[test]
    fn test_filter_sessions_by_cwd() {
        let mut state = AppState::default();
        state.apply_event(AgentEvent {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/home/user/myproject".to_string()),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        });
        state.apply_event(AgentEvent {
            session_id: "s2".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/tmp/other".to_string()),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        });

        let mut ui = default_ui();
        ui.filter_text = "myproject".to_string();
        let filtered = filter_sessions(&state, &ui);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "s1");
    }

    #[test]
    fn test_filter_sessions_by_display_name() {
        let mut state = AppState::default();
        state.apply_event(AgentEvent {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        });
        state.apply_event(AgentEvent {
            session_id: "s2".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        });

        let mut ui = default_ui();
        ui.display_names
            .insert("s1".to_string(), "frontend".to_string());
        ui.filter_text = "front".to_string();
        let filtered = filter_sessions(&state, &ui);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "s1");
    }

    #[test]
    fn test_filter_sessions_case_insensitive() {
        let mut state = AppState::default();
        state.apply_event(AgentEvent {
            session_id: "MySession".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::SessionStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        });

        let mut ui = default_ui();
        ui.filter_text = "mysess".to_string();
        let filtered = filter_sessions(&state, &ui);
        assert_eq!(filtered.len(), 1);
    }

    // ---------------------------------------------------------------------------
    // Mode transition tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_mode_transitions() {
        let mut ui = default_ui();
        assert_eq!(ui.mode, UiMode::Normal);

        // Normal -> Filter
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut ui,
            3,
        );
        assert_eq!(ui.mode, UiMode::Filter);

        // Filter -> Normal (Esc)
        handle_filter_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.mode, UiMode::Normal);

        // Normal -> Help
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            &mut ui,
            3,
        );
        assert_eq!(ui.mode, UiMode::Help);

        // Help -> Normal
        handle_help_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.mode, UiMode::Normal);

        // Normal -> Rename
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut ui,
            3,
        );
        assert_eq!(ui.mode, UiMode::Rename);

        // Rename -> Normal (Esc)
        handle_rename_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut ui,
            Some("s1"),
        );
        assert_eq!(ui.mode, UiMode::Normal);
    }

    #[test]
    fn test_rename_commits_on_enter() {
        let mut ui = default_ui();
        ui.mode = UiMode::Rename;
        ui.rename_text = "my-agent".to_string();

        handle_rename_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            Some("session-123"),
        );

        assert_eq!(ui.mode, UiMode::Normal);
        assert_eq!(
            ui.display_names.get("session-123"),
            Some(&"my-agent".to_string())
        );
    }

    #[test]
    fn test_rename_empty_removes_name() {
        let mut ui = default_ui();
        ui.display_names
            .insert("s1".to_string(), "old-name".to_string());
        ui.mode = UiMode::Rename;
        ui.rename_text.clear();

        handle_rename_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            Some("s1"),
        );

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(!ui.display_names.contains_key("s1"));
    }

    #[test]
    fn test_filter_typing() {
        let mut ui = default_ui();
        ui.mode = UiMode::Filter;

        handle_filter_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_filter_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &mut ui,
        );
        assert_eq!(ui.filter_text, "ab");

        handle_filter_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut ui,
        );
        assert_eq!(ui.filter_text, "a");

        // Enter keeps filter
        handle_filter_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.mode, UiMode::Normal);
        assert_eq!(ui.filter_text, "a");
    }

    #[test]
    fn test_filter_esc_clears() {
        let mut ui = default_ui();
        ui.mode = UiMode::Filter;
        ui.filter_text = "hello".to_string();

        handle_filter_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.mode, UiMode::Normal);
        assert!(ui.filter_text.is_empty());
    }

    #[test]
    fn test_normal_esc_clears_filter() {
        let mut ui = default_ui();
        ui.filter_text = "active-filter".to_string();

        handle_normal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui, 5);
        assert!(ui.filter_text.is_empty());
    }

    #[test]
    fn test_rename_not_available_when_empty() {
        let mut ui = default_ui();
        // total = 0, rename should not activate
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut ui,
            0,
        );
        assert_eq!(ui.mode, UiMode::Normal);
    }

    #[test]
    fn test_alt_digit_detects_alt_1_through_9() {
        for c in '1'..='9' {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
            assert_eq!(alt_digit(key), Some(c as u8 - b'0'));
        }
    }

    #[test]
    fn test_alt_digit_ignores_non_alt() {
        let key = KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE);
        assert_eq!(alt_digit(key), None);
    }

    #[test]
    fn test_alt_digit_ignores_non_digit() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(alt_digit(key), None);
    }

    #[test]
    fn test_alt_digit_ignores_zero() {
        let key = KeyEvent::new(KeyCode::Char('0'), KeyModifiers::ALT);
        assert_eq!(alt_digit(key), None);
    }

    // -----------------------------------------------------------------------
    // Bell transition detection tests
    // -----------------------------------------------------------------------

    fn make_session(status: SessionStatus) -> SessionState {
        SessionState {
            session_id: String::new(),
            agent_type: AgentType::ClaudeCode,
            cwd: None,
            status,
            active_tool: None,
            started_at: Utc::now(),
            last_activity: Utc::now(),
            recent_events: std::collections::VecDeque::new(),
            tool_count: 0,
            last_user_prompt: None,
            pane_id: None,
        }
    }

    #[test]
    fn bell_on_transition_to_waiting() {
        let mut sessions = HashMap::new();
        sessions.insert("a".into(), make_session(SessionStatus::WaitingForInput));

        let mut last = HashMap::new();
        last.insert("a".into(), SessionStatus::Working);

        let (need_bell, _) = compute_bell_needed(&sessions, &last, &BellConfig::default());
        assert!(need_bell);
    }

    #[test]
    fn bell_no_repeat_same_status() {
        let mut sessions = HashMap::new();
        sessions.insert("a".into(), make_session(SessionStatus::WaitingForInput));

        let mut last = HashMap::new();
        last.insert("a".into(), SessionStatus::WaitingForInput);

        let (need_bell, _) = compute_bell_needed(&sessions, &last, &BellConfig::default());
        assert!(!need_bell);
    }

    #[test]
    fn bell_respects_config_toggle_off() {
        let mut sessions = HashMap::new();
        sessions.insert("a".into(), make_session(SessionStatus::Idle));

        let mut last = HashMap::new();
        last.insert("a".into(), SessionStatus::Working);

        // Default config has on_idle = false
        let (need_bell, _) = compute_bell_needed(&sessions, &last, &BellConfig::default());
        assert!(!need_bell);
    }

    #[test]
    fn bell_respects_config_toggle_on() {
        let mut sessions = HashMap::new();
        sessions.insert("a".into(), make_session(SessionStatus::Idle));

        let mut last = HashMap::new();
        last.insert("a".into(), SessionStatus::Working);

        let config = BellConfig {
            on_idle: true,
            ..Default::default()
        };
        let (need_bell, _) = compute_bell_needed(&sessions, &last, &config);
        assert!(need_bell);
    }

    #[test]
    fn bell_disabled_globally() {
        let mut sessions = HashMap::new();
        sessions.insert("a".into(), make_session(SessionStatus::WaitingForInput));

        let last = HashMap::new(); // new session

        let config = BellConfig {
            enabled: false,
            ..Default::default()
        };
        let (need_bell, _) = compute_bell_needed(&sessions, &last, &config);
        assert!(!need_bell);
    }

    #[test]
    fn bell_multiple_transitions_single_bool() {
        let mut sessions = HashMap::new();
        sessions.insert("a".into(), make_session(SessionStatus::WaitingForInput));
        sessions.insert("b".into(), make_session(SessionStatus::Error));

        let mut last = HashMap::new();
        last.insert("a".into(), SessionStatus::Working);
        last.insert("b".into(), SessionStatus::Working);

        let (need_bell, _) = compute_bell_needed(&sessions, &last, &BellConfig::default());
        assert!(need_bell);
    }

    #[test]
    fn bell_cleanup_removed_sessions() {
        let sessions = HashMap::new(); // no sessions

        let mut last = HashMap::new();
        last.insert("gone".into(), SessionStatus::Working);

        let (_, new_map) = compute_bell_needed(&sessions, &last, &BellConfig::default());
        assert!(!new_map.contains_key("gone"));
    }

    #[test]
    fn bell_new_session_triggers() {
        let mut sessions = HashMap::new();
        sessions.insert("new".into(), make_session(SessionStatus::WaitingForInput));

        let last = HashMap::new(); // empty — session is brand new

        let (need_bell, new_map) = compute_bell_needed(&sessions, &last, &BellConfig::default());
        assert!(need_bell);
        assert_eq!(new_map.get("new"), Some(&SessionStatus::WaitingForInput));
    }
}
