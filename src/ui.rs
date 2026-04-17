use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Tabs},
};

use crate::ascii_art::{AsciiArtResult, generate_ascii_art};
use crate::config;
use crate::config::{BellConfig, DashboardConfig, IdleArtConfig};
use crate::embedded_pane::EmbeddedPaneController;
use crate::event::{AgentType, EventType};
use crate::pane::{PaneController, PaneError};
use crate::project_config::{ModeConfig, OrchestrationConfig, load_project_config};
use crate::state::{AppState, DashboardStats, SessionState, SessionStatus, SharedState};
use crate::tab::{Tab, TabId, TabManager};
use crate::terminal_widget::TerminalWidget;
use crate::theme::ColorPalette;

impl fmt::Display for crate::event::AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            crate::event::AgentType::ClaudeCode => write!(f, "ClaudeCode"),
            crate::event::AgentType::OpenCode => write!(f, "OpenCode"),
            crate::event::AgentType::None => write!(f, "No agent"),
        }
    }
}

// ---------------------------------------------------------------------------
// Platform-aware modifier key label
// ---------------------------------------------------------------------------

const MOD_KEY: &str = "Ctrl";

// ---------------------------------------------------------------------------
// Card density (adaptive layout)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardDensity {
    Compact,  // 6 rows: 1 prompt, 1 tool
    Normal,   // 8 rows: 1 prompt, 3 tools
    Spacious, // 10 rows: 3 prompts, 3 tools
}

impl CardDensity {
    /// Card height in rows. When `wide` is false an extra stats line is rendered,
    /// so each non-compact mode needs one more row.
    fn card_height(self, wide: bool) -> u16 {
        let extra = if wide { 0 } else { 1 };
        match self {
            CardDensity::Compact => 7 + extra,
            CardDensity::Normal => 9 + extra,
            CardDensity::Spacious => 11 + extra,
        }
    }

    fn max_tools(self) -> usize {
        match self {
            CardDensity::Compact => 1,
            _ => 3,
        }
    }

    fn max_prompts(self) -> usize {
        match self {
            CardDensity::Spacious => 3,
            _ => 1,
        }
    }
}

fn choose_density(
    total_cards: usize,
    cols: usize,
    available_height: u16,
    wide: bool,
) -> CardDensity {
    let total_card_rows = total_cards.div_ceil(cols);
    for density in [
        CardDensity::Spacious,
        CardDensity::Normal,
        CardDensity::Compact,
    ] {
        let needed = total_card_rows as u16 * density.card_height(wide);
        if needed <= available_height {
            return density;
        }
    }
    CardDensity::Compact
}

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
    PaneInput,
    StarPrompt,
    ConfigGenPrompt,
    QuitConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneLayout {
    Stacked,
    Tiled,
}

/// Describes which panes to render and how to lay them out, based on the active tab.
enum ActiveTabView {
    /// Dashboard tab: show all panes except those managed by mode tabs.
    Dashboard { exclude_pane_ids: Vec<String> },
    /// Mode tab: agent pane on left (50%), side panes stacked on right (50%).
    Mode {
        mode_name: String,
        agent_pane_id: String,
        side_pane_ids: Vec<String>,
        /// Which side pane has visual focus (`None` = agent pane).
        focused_side_pane_index: Option<usize>,
    },
    /// Orchestration tab: same layout as dashboard, scoped to role panes only.
    Orchestration { role_pane_ids: Vec<String> },
}

/// Lightweight snapshot of tab state for rendering, decoupled from TabManager.
struct TabBarInfo {
    show: bool,
    labels: Vec<String>,
    active_index: usize,
}

struct DirPickerState {
    current_dir: PathBuf,
    entries: Vec<PathBuf>,
    selected: usize,
    scroll_offset: usize,
    filter_text: String,
    filtering: bool,
    filtered_indices: Vec<usize>,
}

impl DirPickerState {
    fn new(start: PathBuf) -> Self {
        let mut state = Self {
            current_dir: start,
            entries: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            filter_text: String::new(),
            filtering: false,
            filtered_indices: Vec::new(),
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
        self.filter_text.clear();
        self.filtering = false;
        self.refilter();
    }

    fn enter_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        if entry == Path::new("..") {
            self.go_up();
            return;
        }
        self.current_dir = entry;
        self.refresh();
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.refresh();
        }
    }

    fn refilter(&mut self) {
        let query = self.filter_text.to_lowercase();
        self.filtered_indices.clear();
        for (idx, entry) in self.entries.iter().enumerate() {
            if entry == Path::new("..") {
                self.filtered_indices.push(idx);
                continue;
            }
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let name_lower = name.to_lowercase();
            if query.is_empty() || name_lower.contains(&query) {
                self.filtered_indices.push(idx);
            }
        }
        self.selected = 0;
        self.scroll_offset = 0;
    }

    fn clear_filter(&mut self) {
        if self.filter_text.is_empty() && !self.filtering {
            return;
        }
        self.filter_text.clear();
        self.filtering = false;
        self.refilter();
    }

    fn select_next(&mut self) {
        let total = self.filtered_indices.len();
        if total == 0 {
            return;
        }
        self.selected = (self.selected + 1) % total;
    }

    fn select_previous(&mut self) {
        let total = self.filtered_indices.len();
        if total == 0 {
            return;
        }
        if self.selected == 0 {
            self.selected = total - 1;
        } else {
            self.selected -= 1;
        }
    }

    fn ensure_visible(&mut self, max_visible: usize) {
        if self.filtered_indices.is_empty() {
            self.scroll_offset = 0;
            self.selected = 0;
            return;
        }
        if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len() - 1;
        }
        let window = max_visible.max(1);
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + window {
            self.scroll_offset = self.selected + 1 - window;
        }
        let max_scroll = self.filtered_indices.len().saturating_sub(window);
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }
    }

    fn selected_entry(&self) -> Option<&PathBuf> {
        let idx = *self.filtered_indices.get(self.selected)?;
        self.entries.get(idx)
    }

    fn has_subdirs(&self) -> bool {
        self.entries.iter().any(|entry| entry != Path::new(".."))
    }
}

// ---------------------------------------------------------------------------
// Unified new-pane form (mode selection + name/command in one modal)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Mode,
    Name,
    Command,
}

struct NewPaneFormState {
    dir: PathBuf,
    name: String,
    command: String,
    // Mode/orchestration selection fields
    modes: Vec<ModeConfig>,
    orchestrations: Vec<OrchestrationConfig>,
    selection_index: usize, // 0 = "No mode", 1..M = modes, M+1..M+O = orchestrations
    has_mode_field: bool,
    focused: FormField,
}

// ---------------------------------------------------------------------------
// Idle ASCII art state machine (per session)
// ---------------------------------------------------------------------------

enum IdleArtPhase {
    /// Session is idle but hasn't hit the timeout yet.
    Waiting,
    /// LLM generation spawned; poll receiver for result.
    Generating(std::sync::mpsc::Receiver<Option<AsciiArtResult>>),
    /// Generation succeeded; frames are cached.
    HasArt(AsciiArtResult),
    /// Generation failed; retry after cooldown.
    Failed(std::time::Instant),
}

struct IdleArtEntry {
    phase: IdleArtPhase,
    /// `last_activity` when this idle stretch began.
    idle_since: DateTime<Utc>,
    /// True if the user navigated to this card after art appeared (dismisses art).
    dismissed: bool,
}

impl NewPaneFormState {
    fn new(
        dir: PathBuf,
        name: String,
        command: String,
        modes: Vec<ModeConfig>,
        orchestrations: Vec<OrchestrationConfig>,
    ) -> Self {
        let has_mode_field = !modes.is_empty() || !orchestrations.is_empty();
        Self {
            dir,
            name,
            command,
            modes,
            orchestrations,
            selection_index: 0,
            has_mode_field,
            focused: if has_mode_field {
                FormField::Mode
            } else {
                FormField::Name
            },
        }
    }

    fn mode_option_count(&self) -> usize {
        1 + self.modes.len() + self.orchestrations.len()
    }

    fn select_next_mode(&mut self) {
        if self.selection_index + 1 < self.mode_option_count() {
            self.selection_index += 1;
        }
    }

    fn select_previous_mode(&mut self) {
        self.selection_index = self.selection_index.saturating_sub(1);
    }

    fn selected_mode(&self) -> Option<&ModeConfig> {
        if self.selection_index == 0 || self.selection_index > self.modes.len() {
            None
        } else {
            self.modes.get(self.selection_index - 1)
        }
    }

    fn selected_orchestration(&self) -> Option<&OrchestrationConfig> {
        let orch_start = 1 + self.modes.len();
        if self.selection_index >= orch_start {
            self.orchestrations.get(self.selection_index - orch_start)
        } else {
            None
        }
    }

    fn mode_display_name(&self) -> String {
        if self.selection_index == 0 {
            "No mode".to_string()
        } else if self.selection_index <= self.modes.len() {
            self.modes[self.selection_index - 1].name.clone()
        } else {
            let orch_idx = self.selection_index - 1 - self.modes.len();
            let name = &self.orchestrations[orch_idx].name;
            if name.is_empty() {
                "Orchestration".to_string()
            } else {
                format!("Orch: {name}")
            }
        }
    }

    fn next_field(&self) -> FormField {
        match self.focused {
            FormField::Mode => FormField::Name,
            FormField::Name => FormField::Command,
            FormField::Command => {
                if self.has_mode_field {
                    FormField::Mode
                } else {
                    FormField::Name
                }
            }
        }
    }

    fn prev_field(&self) -> FormField {
        match self.focused {
            FormField::Mode => FormField::Command,
            FormField::Name => {
                if self.has_mode_field {
                    FormField::Mode
                } else {
                    FormField::Command
                }
            }
            FormField::Command => FormField::Name,
        }
    }
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
    /// Maps pane_id → launch metadata for auto-save/restore.
    pane_metadata: HashMap<String, config::SavedPane>,
    config: DashboardConfig,
    palette: ColorPalette,
    /// Tracks last-seen status per session for bell transition detection.
    last_bell_status: HashMap<String, SessionStatus>,
    /// Populated by the background version-check task when a newer release is available.
    update_available: Option<String>,
    /// Layout mode for embedded terminal panes (stacked or tiled).
    pane_layout: PaneLayout,
    /// Warnings collected during session save/restore, flushed after terminal restore.
    session_warnings: Vec<String>,
    /// Mouse text selection state for copy support.
    selection: Option<TextSelection>,
    /// Screen rect of the focused pane (set during render, used for mouse mapping).
    focused_pane_rect: Option<Rect>,
    /// Screen rects of side panes in mode tabs (set during render, used for scroll hit-testing).
    side_pane_rects: Vec<(String, Rect)>,
    /// Screen rect of the agent pane in mode tabs (set during render, used for click-to-focus).
    agent_pane_rect: Option<Rect>,
    /// Tracks last click time and position for double/triple-click detection.
    last_click: Option<(std::time::Instant, u16, u16, u8)>, // (time, col, row, click_count)
    /// Star-prompt state for the "star the repo" reminder dialog.
    star_prompt_state: config::StarPromptState,
    /// Per-session idle ASCII art cache. Key = session_id.
    idle_art_cache: HashMap<String, IdleArtEntry>,
    /// Config generation state — tracks directories where user chose "Never".
    config_gen_state: config::ConfigGenState,
    /// Pane ID + cwd for the pending config-gen modal prompt.
    config_gen_target: Option<(String, String)>,
    /// Selected option index in the config-gen modal (0=Yes, 1=No, 2=Never).
    config_gen_selected: usize,
    /// Selected option in quit confirm modal (0=Quit, 1=Cancel).
    quit_confirm_selected: usize,
    /// Orchestration tab IDs whose start-role prompt has already been injected.
    orchestration_prompted: HashSet<TabId>,
}

/// Tracks an in-progress or completed mouse text selection within a pane.
#[derive(Debug, Clone)]
struct TextSelection {
    /// Pane-relative start column (0-based, within inner area).
    start_col: u16,
    /// Pane-relative start row.
    start_row: u16,
    /// Pane-relative end column.
    end_col: u16,
    /// Pane-relative end row.
    end_row: u16,
    /// The focused pane's Rect at selection start (screen coordinates).
    pane_rect: Rect,
}

impl UiState {
    fn new(config: DashboardConfig, palette: ColorPalette) -> Self {
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
            pane_metadata: HashMap::new(),
            config,
            palette,
            last_bell_status: HashMap::new(),
            update_available: None,
            pane_layout: PaneLayout::Stacked,
            session_warnings: Vec::new(),
            selection: None,
            focused_pane_rect: None,
            side_pane_rects: Vec::new(),
            agent_pane_rect: None,
            last_click: None,
            star_prompt_state: config::StarPromptState::default(),
            idle_art_cache: HashMap::new(),
            config_gen_state: config::ConfigGenState::load(),
            config_gen_target: None,
            config_gen_selected: 0,
            quit_confirm_selected: 0,
            orchestration_prompted: HashSet::new(),
        }
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new(DashboardConfig::default(), ColorPalette::dark())
    }
}

// ---------------------------------------------------------------------------
// Grid navigation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Session filtering
// ---------------------------------------------------------------------------

/// Resize dashboard panes to match the dashboard layout after a tab switch.
/// Resize PTYs for a mode tab's agent + side panes to 50% width.
fn resize_mode_tab_panes(pane: &dyn PaneController, tab_manager: &TabManager, area: Rect) {
    let (agent_pane_id, side_pane_ids) = match tab_manager.active_tab() {
        Tab::Mode {
            agent_pane_id,
            mode_manager,
            ..
        } => (agent_pane_id.clone(), mode_manager.managed_pane_ids()),
        _ => return,
    };
    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
        let half_width = (area.width / 2).saturating_sub(2);
        let agent_rows = area.height.saturating_sub(3);
        if agent_rows > 0 && half_width > 0 {
            let _ = embedded.resize_pane_pty(&agent_pane_id, agent_rows, half_width);
        }
        let side_count = side_pane_ids.len().max(1) as u16;
        let side_rows = (area.height / side_count).saturating_sub(2);
        if side_rows > 0 && half_width > 0 {
            for id in &side_pane_ids {
                let _ = embedded.resize_pane_pty(id, side_rows, half_width);
            }
        }
    }
}

fn resize_dashboard_panes(
    pane: &dyn PaneController,
    ui: &UiState,
    tab_manager: &TabManager,
    area: Rect,
) {
    let orch_pane_ids = match tab_manager.active_tab() {
        Tab::Dashboard => None,
        Tab::Orchestration { role_pane_ids, .. } => Some(role_pane_ids.clone()),
        _ => return,
    };
    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
        let all = embedded.pane_ids();
        let pane_ids: Vec<String> = if let Some(ref include) = orch_pane_ids {
            all.into_iter().filter(|id| include.contains(id)).collect()
        } else {
            let exclude = tab_manager.all_managed_pane_ids();
            all.into_iter().filter(|id| !exclude.contains(id)).collect()
        };
        if pane_ids.is_empty() {
            return;
        }
        let right_width = (area.width * 67 / 100).saturating_sub(2);
        let pane_count = pane_ids.len() as u16;
        // Account for UI chrome: tab bar (1 row when shown), info bar (1 row),
        // and hints bar (1 row) so panes don't push the status line off screen.
        let chrome_rows: u16 = if tab_manager.show_tab_bar() { 3 } else { 2 };
        let usable_height = area.height.saturating_sub(chrome_rows);
        for pane_id in &pane_ids {
            let is_focused = embedded.focused_pane_id().as_deref() == Some(pane_id.as_str());
            let rows = match ui.pane_layout {
                PaneLayout::Tiled => (usable_height / pane_count).saturating_sub(2),
                PaneLayout::Stacked => {
                    if is_focused
                        || (embedded.focused_pane_id().is_none() && pane_id == &pane_ids[0])
                    {
                        usable_height.saturating_sub(2 + pane_count.saturating_sub(1))
                    } else {
                        0
                    }
                }
            };
            if rows > 0 {
                let _ = embedded.resize_pane_pty(pane_id, rows, right_width);
            }
        }
    }
}

fn filter_sessions<'a>(state: &'a AppState, ui: &UiState) -> Vec<(&'a String, &'a SessionState)> {
    let mut sessions: Vec<(&String, &SessionState)> = state.sessions.iter().collect();
    sessions.sort_by(|(_, a), (_, b)| {
        // Sort by pane ID (numeric creation order) when available,
        // falling back to started_at for sessions without a pane.
        match (&a.pane_id, &b.pane_id) {
            (Some(pa), Some(pb)) => {
                let na = pa.parse::<u64>().unwrap_or(u64::MAX);
                let nb = pb.parse::<u64>().unwrap_or(u64::MAX);
                na.cmp(&nb)
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.started_at.cmp(&b.started_at),
        }
    });

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
// Orchestrator prompt construction
// ---------------------------------------------------------------------------

/// Build the orchestrator context file content.
/// Includes the role's own prompt_template, the available-agents list, and
/// delegation protocol instructions.
fn build_orchestrator_context(config: &OrchestrationConfig) -> String {
    let mut content = String::new();

    // 1. Orchestrator's own prompt_template.
    if let Some(start_role) = config.roles.iter().find(|r| r.start)
        && let Some(ref tpl) = start_role.prompt_template
    {
        content.push_str(tpl);
        content.push_str("\n\n");
    }

    // 2. Available agents list.
    content.push_str("## Available agents\n\n");
    for role in &config.roles {
        if role.start {
            continue;
        }
        let desc = role.description.as_deref().unwrap_or("(no description)");
        content.push_str(&format!("- **{}**: {}\n", role.name, desc));
    }

    // 3. Delegation protocol.
    content.push_str("\n## Delegation protocol\n\n");
    content.push_str(
        "To delegate work to an agent, run:\n\
         ```bash\n\
         dot-agent-deck work-done --delegate <role-name> --task \"Task description with context, file paths, and constraints.\"\n\
         ```\n\n\
         To delegate to multiple agents in parallel:\n\
         ```bash\n\
         dot-agent-deck work-done --delegate <role1> --delegate <role2> --task \"Task description.\"\n\
         ```\n\n\
         When all work is complete and you are satisfied with the results:\n\
         ```bash\n\
         dot-agent-deck work-done --done --task \"Final summary of what was accomplished.\"\n\
         ```\n",
    );

    // 4. Wait for user instructions.
    content.push_str(
        "\n## Important\n\n\
         Do NOT take any action yet. Read and acknowledge your role and available agents, \
         then wait for the user to provide instructions on what to work on.\n",
    );

    content
}

/// Write the orchestrator context file and return the one-line prompt to inject.
/// The file is written to `.dot-agent-deck/orchestrator-context.md` inside `cwd`.
/// Returns `None` if the file cannot be written.
fn prepare_orchestrator_prompt(config: &OrchestrationConfig, cwd: &str) -> Option<String> {
    let dir = std::path::Path::new(cwd).join(".dot-agent-deck");
    std::fs::create_dir_all(&dir).ok()?;
    let file_path = dir.join("orchestrator-context.md");
    let content = build_orchestrator_context(config);
    std::fs::write(&file_path, &content).ok()?;
    Some("Read .dot-agent-deck/orchestrator-context.md for your role, available agents, and delegation protocol. Acknowledge your role and wait for instructions.".to_string())
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

#[derive(Debug)]
struct NewPaneRequest {
    dir: PathBuf,
    name: String,
    command: String,
    mode_config: Option<ModeConfig>,
    orchestration_config: Option<OrchestrationConfig>,
}

#[derive(Debug)]
enum KeyResult {
    Continue,
    Quit,
    Focus,
    NewPane(NewPaneRequest),
    SendConfigGenPrompt { pane_id: String, cwd: String },
    RequestConfigGen,
    ForwardToPane(Vec<u8>),
}

/// Convert a crossterm `KeyEvent` into the byte sequence expected by a terminal PTY.
fn keyevent_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    // Alt modifier: wrap the base key bytes with an ESC prefix.
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);

    // Ctrl+letter → control codes 0x01–0x1a (Alt adds ESC prefix)
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
    {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            let ctrl = vec![c as u8 - b'a' + 1];
            return Some(if has_alt {
                [vec![0x1b], ctrl].concat()
            } else {
                ctrl
            });
        }
    }

    // Base key bytes (without Alt). Alt prefix is added at the end.
    let base: Option<Vec<u8>> = match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        _ => None,
    };

    // Prepend ESC for Alt-modified keys (e.g., Alt+Backspace → \x1b\x7f).
    match (has_alt, base) {
        (true, Some(b)) => Some([vec![0x1b], b].concat()),
        (false, b) => b,
        (true, None) => None,
    }
}

/// Compute the row offset between widget-relative coordinates and vt100 screen
/// coordinates. The widget shows the bottom `inner_h` rows of the screen.
fn screen_row_offset(screen: &vt100::Screen, pane_rect: Rect) -> u16 {
    let inner_h = pane_rect.height.saturating_sub(2);
    let screen_rows = screen.size().0;
    screen_rows.saturating_sub(inner_h)
}

/// Extract text from a vt100 screen for the given selection region.
/// Selection coordinates are widget-relative; `row_offset` maps them to screen rows.
fn extract_selection_text(screen: &vt100::Screen, sel: &TextSelection, row_offset: u16) -> String {
    let (sr, sc, er, ec) = if (sel.start_row, sel.start_col) <= (sel.end_row, sel.end_col) {
        (sel.start_row, sel.start_col, sel.end_row, sel.end_col)
    } else {
        (sel.end_row, sel.end_col, sel.start_row, sel.start_col)
    };
    let mut result = String::new();
    let (screen_rows, screen_cols) = screen.size();
    for widget_row in sr..=er {
        let screen_row = widget_row + row_offset;
        if screen_row >= screen_rows {
            break;
        }
        let col_start = if widget_row == sr { sc } else { 0 };
        let col_end = if widget_row == er {
            ec
        } else {
            screen_cols.saturating_sub(1)
        };
        for col in col_start..=col_end.min(screen_cols.saturating_sub(1)) {
            if let Some(cell) = screen.cell(screen_row, col) {
                let ch = cell.contents();
                if ch.is_empty() {
                    result.push(' ');
                } else {
                    result.push_str(ch);
                }
            }
        }
        // Trim trailing spaces per line and add newline between lines.
        if widget_row < er {
            let trimmed = result.trim_end_matches(' ');
            let trimmed_len = trimmed.len();
            result.truncate(trimmed_len);
            result.push('\n');
        }
    }
    let trimmed = result.trim_end_matches(' ');
    trimmed.to_string()
}

/// Copy text to the system clipboard using the OSC 52 escape sequence.
/// Writes directly to `/dev/tty` to bypass ratatui's buffered terminal output.
fn copy_to_clipboard_osc52(text: &str) {
    use std::io::Write;
    let encoded = base64_encode(text.as_bytes());
    // Use ST (\x1b\\) terminator — more widely supported than BEL (\x07) in raw mode.
    let seq = format!("\x1b]52;c;{encoded}\x1b\\");
    // Write to /dev/tty directly so the escape sequence reaches the outer terminal
    // even when ratatui has captured stdout.
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(seq.as_bytes());
        let _ = tty.flush();
    }
}

/// Minimal base64 encoder (no external dependency needed).
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Find the word boundaries around (row, col) in a vt100 screen.
/// `row` is widget-relative; `row_offset` maps it to screen coordinates.
/// Returns (start_col, end_col) for the word at the given position.
fn word_bounds_at(screen: &vt100::Screen, row: u16, col: u16, row_offset: u16) -> (u16, u16) {
    let (_rows, cols) = screen.size();
    let screen_row = row + row_offset;
    let is_word_char = |c: u16| -> bool {
        screen
            .cell(screen_row, c)
            .map(|cell| {
                let ch = cell.contents();
                !ch.is_empty()
                    && ch
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-')
            })
            .unwrap_or(false)
    };
    let mut start = col;
    while start > 0 && is_word_char(start - 1) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols && is_word_char(end + 1) {
        end += 1;
    }
    (start, end)
}

fn handle_pane_input_key(key: KeyEvent) -> KeyResult {
    if let Some(bytes) = keyevent_to_bytes(&key) {
        KeyResult::ForwardToPane(bytes)
    } else {
        KeyResult::Continue
    }
}

fn handle_quit_confirm_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    match key.code {
        // Ctrl+C again: actually quit (legacy shortcut)
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyResult::Quit,
        KeyCode::Up | KeyCode::Char('k') => {
            ui.quit_confirm_selected = 0;
            KeyResult::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            ui.quit_confirm_selected = 1;
            KeyResult::Continue
        }
        KeyCode::Enter => {
            if ui.quit_confirm_selected == 0 {
                KeyResult::Quit
            } else {
                ui.mode = UiMode::Normal;
                KeyResult::Continue
            }
        }
        KeyCode::Esc => {
            ui.mode = UiMode::Normal;
            KeyResult::Continue
        }
        _ => KeyResult::Continue,
    }
}

fn handle_star_prompt_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    match key.code {
        KeyCode::Char('s') => {
            let msg = if open::that("https://github.com/vfarcic/dot-agent-deck").is_ok() {
                "Thanks for starring! ⭐".to_string()
            } else {
                "Visit github.com/vfarcic/dot-agent-deck to star ⭐".to_string()
            };
            ui.star_prompt_state.dismiss_permanently();
            ui.mode = UiMode::Normal;
            ui.status_message = Some((msg, std::time::Instant::now()));
            KeyResult::Continue
        }
        KeyCode::Char('l') | KeyCode::Esc => {
            ui.star_prompt_state.snooze();
            ui.mode = UiMode::Normal;
            KeyResult::Continue
        }
        KeyCode::Char('d') => {
            ui.star_prompt_state.dismiss_permanently();
            ui.mode = UiMode::Normal;
            KeyResult::Continue
        }
        _ => KeyResult::Continue,
    }
}

fn handle_config_gen_prompt_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            ui.config_gen_selected = ui.config_gen_selected.saturating_sub(1);
            KeyResult::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if ui.config_gen_selected < 2 {
                ui.config_gen_selected += 1;
            }
            KeyResult::Continue
        }
        KeyCode::Enter => match ui.config_gen_selected {
            0 => {
                // Yes — send prompt and focus pane.
                ui.mode = UiMode::Normal;
                if let Some((pane_id, cwd)) = ui.config_gen_target.take() {
                    return KeyResult::SendConfigGenPrompt { pane_id, cwd };
                }
                KeyResult::Continue
            }
            1 => {
                // No — dismiss for now, hint stays on card.
                ui.config_gen_target = None;
                ui.mode = UiMode::Normal;
                KeyResult::Continue
            }
            _ => {
                // Never — suppress hint permanently for this directory.
                if let Some((_, ref cwd)) = ui.config_gen_target {
                    ui.config_gen_state.suppress_dir(cwd);
                }
                ui.config_gen_target = None;
                ui.mode = UiMode::Normal;
                ui.status_message = Some((
                    "Config prompt suppressed for this directory.".to_string(),
                    std::time::Instant::now(),
                ));
                KeyResult::Continue
            }
        },
        KeyCode::Esc => {
            ui.config_gen_target = None;
            ui.mode = UiMode::Normal;
            KeyResult::Continue
        }
        _ => KeyResult::Continue,
    }
}

fn truncate_with_ellipsis(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = input.chars().count();
    if char_count <= max_chars {
        return input.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = input.chars().take(keep).collect();
    out.push('…');
    out
}

/// Select deck at `idx` and focus its pane. Returns `true` if idx was valid.
fn focus_deck(
    idx: usize,
    ui: &mut UiState,
    filtered: &[(&String, &SessionState)],
    snapshot: &AppState,
    state: &SharedState,
    pane: &dyn PaneController,
) -> bool {
    if idx >= filtered.len() {
        return false;
    }
    ui.selected_index = idx;
    if let Some((sid, _)) = filtered.get(idx)
        && let Some(session) = snapshot.sessions.get(*sid)
    {
        if let Some(ref pane_id) = session.pane_id {
            match pane.focus_pane(pane_id) {
                Ok(()) => {
                    ui.mode = UiMode::PaneInput;
                    ui.status_message = Some((
                        "PaneInput mode — type to interact, Ctrl+d for dashboard".to_string(),
                        std::time::Instant::now(),
                    ));
                    // Recompute PTY dimensions after focus change so stacked
                    // panes get the correct expanded/collapsed sizes.
                    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
                        let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
                        let right_width = (term_w * 67 / 100).saturating_sub(2);
                        // Account for UI chrome (tab bar + info bar + hints bar).
                        // Use 3 as safe default; tab bar is shown whenever tabs exist.
                        let usable_h = term_h.saturating_sub(3);
                        let pane_ids = embedded.pane_ids();
                        let pane_count = pane_ids.len() as u16;
                        for pid in &pane_ids {
                            let is_focused = pid == pane_id;
                            let rows = match ui.pane_layout {
                                PaneLayout::Tiled => (usable_h / pane_count).saturating_sub(2),
                                PaneLayout::Stacked => {
                                    if is_focused {
                                        usable_h.saturating_sub(2 + pane_count.saturating_sub(1))
                                    } else {
                                        0
                                    }
                                }
                            };
                            if rows > 0 {
                                let _ = embedded.resize_pane_pty(pid, rows, right_width);
                            }
                        }
                    }
                }
                Err(PaneError::CommandFailed(ref msg)) => {
                    state.blocking_write().sessions.remove(*sid);
                    ui.status_message = Some((
                        format!("Removed stale session: {msg}"),
                        std::time::Instant::now(),
                    ));
                }
                Err(e) => {
                    ui.status_message =
                        Some((format!("Pane focus failed: {e}"), std::time::Instant::now()));
                }
            }
        } else {
            ui.status_message = Some((
                format!("No pane linked to session {sid}"),
                std::time::Instant::now(),
            ));
        }
    }
    true
}

fn handle_normal_key(key: KeyEvent, ui: &mut UiState, total: usize) -> KeyResult {
    // Ctrl+C from dashboard: show quit confirmation
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return KeyResult::Continue;
    }
    match key.code {
        // Dashboard card navigation (linear cycling)
        KeyCode::Char('j') | KeyCode::Down => {
            if total > 0 {
                ui.selected_index = (ui.selected_index + 1) % total;
            }
            KeyResult::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if total > 0 {
                ui.selected_index = (ui.selected_index + total - 1) % total;
            }
            KeyResult::Continue
        }
        // Left/Right/h/l handled in main loop for tab switching
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
        KeyCode::Enter if total > 0 => KeyResult::Focus,
        KeyCode::Char('g') if total > 0 => KeyResult::RequestConfigGen,
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
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return KeyResult::Continue;
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
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return KeyResult::Continue;
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
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return KeyResult::Continue;
    }
    let picker = match ui.dir_picker.as_mut() {
        Some(p) => p,
        None => {
            ui.mode = UiMode::Normal;
            return KeyResult::Continue;
        }
    };

    if picker.filtering {
        match key.code {
            KeyCode::Esc => {
                picker.clear_filter();
            }
            KeyCode::Enter => {
                picker.filtering = false;
            }
            KeyCode::Backspace => {
                picker.filter_text.pop();
                if picker.filter_text.is_empty() {
                    picker.filtering = false;
                }
                picker.refilter();
            }
            KeyCode::Down => {
                picker.select_next();
            }
            KeyCode::Up => {
                picker.select_previous();
            }
            KeyCode::Char(c) => match c {
                'q' | 'Q' => {
                    ui.dir_picker = None;
                    ui.mode = UiMode::Normal;
                }
                _ => {
                    picker.filter_text.push(c);
                    picker.refilter();
                }
            },
            _ => {}
        }
        return KeyResult::Continue;
    }

    match key.code {
        KeyCode::Esc => {
            if !picker.filter_text.is_empty() {
                picker.clear_filter();
            } else {
                ui.dir_picker = None;
                ui.mode = UiMode::Normal;
            }
        }
        KeyCode::Char('q') => {
            ui.dir_picker = None;
            ui.mode = UiMode::Normal;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            picker.select_next();
        }
        KeyCode::Char('k') | KeyCode::Up => {
            picker.select_previous();
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
            // If no subdirs, select current directory
            if !picker.has_subdirs() {
                transition_after_dir_pick(ui);
                return KeyResult::Continue;
            }
            if picker.filtered_indices.is_empty() {
                return KeyResult::Continue;
            }
            picker.enter_selected();
        }
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
            picker.go_up();
        }
        KeyCode::Char('/') => {
            picker.filtering = true;
        }
        KeyCode::Char(' ') => {
            transition_after_dir_pick(ui);
            return KeyResult::Continue;
        }
        _ => {}
    }
    KeyResult::Continue
}

/// Check for `.dot-agent-deck.toml` in the selected directory.
/// Open the unified new-pane form, with mode field when modes are available.
fn transition_after_dir_pick(ui: &mut UiState) {
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

    let (modes, orchestrations) = match load_project_config(&dir) {
        Ok(Some(config)) => (config.modes, config.orchestrations),
        _ => (vec![], vec![]),
    };

    ui.dir_picker = None;
    ui.new_pane_form = Some(NewPaneFormState::new(
        dir,
        name,
        command,
        modes,
        orchestrations,
    ));
    ui.mode = UiMode::NewPaneForm;
}

fn handle_new_pane_form_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return KeyResult::Continue;
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
        KeyCode::Tab => {
            form.focused = form.next_field();
        }
        KeyCode::BackTab => {
            form.focused = form.prev_field();
        }
        // Left/Right cycle mode when Mode field is focused
        KeyCode::Left | KeyCode::Char('h') if form.focused == FormField::Mode => {
            form.select_previous_mode();
        }
        KeyCode::Right | KeyCode::Char('l') if form.focused == FormField::Mode => {
            form.select_next_mode();
        }
        KeyCode::Enter => match form.focused {
            FormField::Mode => {
                form.focused = FormField::Name;
            }
            FormField::Name => {
                form.focused = FormField::Command;
            }
            FormField::Command => {
                let req = NewPaneRequest {
                    dir: form.dir.clone(),
                    name: form.name.clone(),
                    command: form.command.clone(),
                    mode_config: form.selected_mode().cloned(),
                    orchestration_config: form.selected_orchestration().cloned(),
                };
                ui.new_pane_form = None;
                ui.mode = UiMode::Normal;
                return KeyResult::NewPane(req);
            }
        },
        KeyCode::Backspace if form.focused != FormField::Mode => {
            let field = match form.focused {
                FormField::Name => &mut form.name,
                FormField::Command => &mut form.command,
                FormField::Mode => unreachable!(),
            };
            field.pop();
        }
        KeyCode::Char(c) if form.focused != FormField::Mode => {
            let field = match form.focused {
                FormField::Name => &mut form.name,
                FormField::Command => &mut form.command,
                FormField::Mode => unreachable!(),
            };
            field.push(c);
        }
        _ => {}
    }
    KeyResult::Continue
}

// ---------------------------------------------------------------------------
// Reactive pane routing — extract new Bash commands from session events
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// TUI entry point
// ---------------------------------------------------------------------------

pub fn run_tui(
    state: SharedState,
    pane: Arc<dyn PaneController>,
    config: DashboardConfig,
    palette: ColorPalette,
    continue_session: bool,
) -> std::io::Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
        );
        ratatui::restore();
        original_hook(info);
    }));

    // Enable mouse capture and bracketed paste so events reach our event loop.
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
    )?;

    let mut terminal = ratatui::init();
    let mut tick: u64 = 0;
    let mut ui = UiState::new(config, palette);
    let mut tab_manager = TabManager::new(Arc::clone(&pane));

    let mut star_state = config::StarPromptState::load();
    let should_show_star = star_state.increment_and_check();
    ui.star_prompt_state = star_state;
    if should_show_star {
        ui.mode = UiMode::StarPrompt;
    }

    if continue_session {
        // Ensure the terminal has up-to-date dimensions before we resize
        // any PTYs — without this, get_frame().area() may return stale or
        // default values because no draw() call has happened yet.
        let _ = terminal.autoresize();

        let saved = config::SavedSession::load();
        // Collect deferred mode pane restores — we need the terminal ready
        // before we can resize PTYs, so mode tabs are opened after the loop.
        let mut deferred_mode_panes: Vec<(config::SavedPane, ModeConfig)> = Vec::new();
        for saved_pane in &saved.panes {
            let dir = std::path::Path::new(&saved_pane.dir);
            if !dir.is_dir() {
                ui.session_warnings.push(format!(
                    "Warning: skipping pane '{}' — directory {} not found",
                    saved_pane.name, saved_pane.dir
                ));
                continue;
            }
            // If the pane belonged to a mode tab, defer it so we can open a
            // full mode tab (with side panes) instead of a plain dashboard pane.
            if let Some(ref mode_name) = saved_pane.mode {
                match load_project_config(dir) {
                    Ok(Some(cfg)) => {
                        if let Some(mode_cfg) =
                            cfg.modes.iter().find(|m| m.name == *mode_name).cloned()
                        {
                            deferred_mode_panes.push((saved_pane.clone(), mode_cfg));
                            continue;
                        }
                        ui.session_warnings.push(format!(
                            "Warning: mode '{}' not found in {}, restoring as plain pane",
                            mode_name, saved_pane.dir
                        ));
                    }
                    Ok(None) => {
                        ui.session_warnings.push(format!(
                            "Warning: no project config in {}, restoring as plain pane",
                            saved_pane.dir
                        ));
                    }
                    Err(e) => {
                        ui.session_warnings.push(format!(
                            "Warning: failed to load project config from {}: {e}",
                            saved_pane.dir
                        ));
                    }
                }
            }
            let cmd = if saved_pane.command.is_empty() {
                None
            } else {
                Some(saved_pane.command.as_str())
            };
            match pane.create_pane(cmd, Some(&saved_pane.dir)) {
                Ok(new_id) => {
                    state.blocking_write().register_pane(new_id.clone());
                    if !saved_pane.name.is_empty() {
                        if let Err(e) = pane.rename_pane(&new_id, &saved_pane.name) {
                            ui.session_warnings.push(format!(
                                "Warning: failed to rename pane '{}': {e}",
                                saved_pane.name
                            ));
                        }
                        ui.pane_display_names
                            .insert(new_id.clone(), saved_pane.name.clone());
                        ui.pane_names
                            .insert(new_id.clone(), saved_pane.name.clone());
                    }
                    ui.pane_metadata.insert(new_id, saved_pane.clone());
                }
                Err(e) => {
                    ui.session_warnings.push(format!(
                        "Warning: failed to restore pane '{}': {e}",
                        saved_pane.name
                    ));
                }
            }
        }
        // Restore mode tabs — create agent pane (empty shell), open mode tab,
        // resize PTYs, then send init + agent commands at the right size.
        for (saved_pane, mode_config) in deferred_mode_panes {
            match pane.create_pane(None, Some(&saved_pane.dir)) {
                Ok(new_id) => {
                    state.blocking_write().register_pane(new_id.clone());
                    if !saved_pane.name.is_empty() {
                        let _ = pane.rename_pane(&new_id, &saved_pane.name);
                        ui.pane_display_names
                            .insert(new_id.clone(), saved_pane.name.clone());
                        ui.pane_names
                            .insert(new_id.clone(), saved_pane.name.clone());
                    }
                    ui.pane_metadata.insert(new_id.clone(), saved_pane.clone());
                    match tab_manager.open_mode_tab(&mode_config, &saved_pane.dir, new_id.clone()) {
                        Ok((_tab_idx, side_ids)) => {
                            for id in &side_ids {
                                state.blocking_write().register_pane(id.clone());
                            }
                            if let Some(embedded) =
                                pane.as_any().downcast_ref::<EmbeddedPaneController>()
                            {
                                let frame_area = terminal.get_frame().area();
                                let half_width = (frame_area.width / 2).saturating_sub(2);
                                let agent_rows = frame_area.height.saturating_sub(3);
                                if agent_rows > 0 && half_width > 0 {
                                    let _ =
                                        embedded.resize_pane_pty(&new_id, agent_rows, half_width);
                                }
                                let side_count = side_ids.len().max(1) as u16;
                                let side_rows = (frame_area.height / side_count).saturating_sub(2);
                                if side_rows > 0 && half_width > 0 {
                                    for id in &side_ids {
                                        let _ = embedded.resize_pane_pty(id, side_rows, half_width);
                                    }
                                }
                            }
                            let _ = tab_manager.start_mode_commands();
                            if let Some(ref init_cmd) = mode_config.init_command {
                                let _ = pane.write_to_pane(&new_id, init_cmd);
                            }
                            if !saved_pane.command.is_empty() {
                                let _ = pane.write_to_pane(&new_id, &saved_pane.command);
                            }
                        }
                        Err(e) => {
                            let _ = pane.close_pane(&new_id);
                            ui.session_warnings.push(format!(
                                "Warning: failed to restore mode '{}': {e}",
                                mode_config.name
                            ));
                        }
                    }
                }
                Err(e) => {
                    ui.session_warnings.push(format!(
                        "Warning: failed to restore mode pane '{}': {e}",
                        saved_pane.name
                    ));
                }
            }
        }
        // Always start on the dashboard so the user gets an overview first.
        tab_manager.switch_to(0);
    }

    'outer: loop {
        // Expire stale status messages
        if let Some((_, created)) = &ui.status_message
            && created.elapsed() > std::time::Duration::from_secs(3)
        {
            ui.status_message = None;
        }

        let snapshot = state.blocking_read().clone();

        // Route new Bash commands through mode tabs for reactive panes.
        let pane_changes = tab_manager.route_reactive_commands(&snapshot.sessions);
        for (old_id, new_id) in &pane_changes {
            let mut st = state.blocking_write();
            st.unregister_pane(old_id);
            st.register_pane(new_id.clone());
            drop(st);
            // Resize the new pane PTY to match the current side pane dimensions.
            if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
                let frame_area = terminal.get_frame().area();
                let half_width = (frame_area.width / 2).saturating_sub(2);
                let side_pane_count = embedded.pane_ids().len().saturating_sub(1) as u16; // exclude agent
                let pane_rows = if side_pane_count > 0 {
                    (frame_area.height / side_pane_count).saturating_sub(2)
                } else {
                    frame_area.height.saturating_sub(3)
                };
                if pane_rows > 0 && half_width > 0 {
                    let _ = embedded.resize_pane_pty(new_id, pane_rows, half_width);
                }
            }
        }

        // Clamp focused side pane index after reactive pane pool changes.
        if !pane_changes.is_empty()
            && let Tab::Mode {
                focused_side_pane_index,
                mode_manager,
                ..
            } = tab_manager.active_tab_mut()
            && let Some(idx) = *focused_side_pane_index
        {
            let count = mode_manager.managed_pane_ids().len();
            if count == 0 {
                *focused_side_pane_index = None;
            } else if idx >= count {
                *focused_side_pane_index = Some(count - 1);
            }
        }

        // Pick up version-check result once
        if ui.update_available.is_none() {
            ui.update_available = snapshot.update_available.clone();
        }

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

        let all_filtered = filter_sessions(&snapshot, &ui);
        // Scope sessions to only those visible in the active tab.
        let filtered: Vec<(&String, &SessionState)> = match tab_manager.active_tab() {
            Tab::Dashboard => {
                let exclude = tab_manager.all_managed_pane_ids();
                all_filtered
                    .into_iter()
                    .filter(|(_, s)| s.pane_id.as_ref().is_none_or(|pid| !exclude.contains(pid)))
                    .collect()
            }
            Tab::Orchestration { role_pane_ids, .. } => all_filtered
                .into_iter()
                .filter(|(_, s)| {
                    s.pane_id
                        .as_ref()
                        .is_some_and(|pid| role_pane_ids.contains(pid))
                })
                .collect(),
            _ => all_filtered,
        };
        let total = filtered.len();

        // Clamp selection
        if total > 0 {
            ui.selected_index = ui.selected_index.min(total - 1);
        } else {
            ui.selected_index = 0;
        }

        // Sync dashboard card selection with focused pane (handles async pane creation).
        if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>()
            && let Some(focused_pane_id) = embedded.focused_pane_id()
        {
            for (i, (_, session)) in filtered.iter().enumerate() {
                if session.pane_id.as_deref() == Some(focused_pane_id.as_str()) {
                    ui.selected_index = i;
                    break;
                }
            }
        }

        let term_width = terminal.get_frame().area().width;
        let has_embedded_panes = pane
            .as_any()
            .downcast_ref::<EmbeddedPaneController>()
            .map(|e| !e.pane_ids().is_empty())
            .unwrap_or(false);
        let dashboard_width = if has_embedded_panes {
            term_width * 33 / 100
        } else {
            term_width
        };
        ui.columns = grid_columns(dashboard_width);

        let has_pane_control = pane.is_available();
        let pane_layout = ui.pane_layout;
        let tab_view = match tab_manager.active_tab() {
            Tab::Dashboard => ActiveTabView::Dashboard {
                exclude_pane_ids: tab_manager.all_managed_pane_ids(),
            },
            Tab::Mode {
                name,
                agent_pane_id,
                mode_manager,
                focused_side_pane_index,
                ..
            } => ActiveTabView::Mode {
                mode_name: name.clone(),
                agent_pane_id: agent_pane_id.clone(),
                side_pane_ids: mode_manager.managed_pane_ids(),
                focused_side_pane_index: *focused_side_pane_index,
            },
            Tab::Orchestration { role_pane_ids, .. } => ActiveTabView::Orchestration {
                role_pane_ids: role_pane_ids.clone(),
            },
        };
        let tab_bar_labels: Vec<String> = tab_manager
            .tabs()
            .iter()
            .map(|tab| match tab {
                Tab::Dashboard => "Dashboard".to_string(),
                Tab::Mode {
                    name,
                    agent_pane_id,
                    ..
                } => ui
                    .pane_metadata
                    .get(agent_pane_id)
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| name.clone()),
                Tab::Orchestration { name, .. } => name.clone(),
            })
            .collect();
        let tab_bar_info = TabBarInfo {
            show: tab_manager.show_tab_bar(),
            labels: tab_bar_labels,
            active_index: tab_manager.active_index(),
        };
        terminal.draw(|frame| {
            render_frame(
                frame,
                &snapshot,
                &mut ui,
                &filtered,
                tick,
                has_pane_control,
                &*pane,
                pane_layout,
                &tab_view,
                &tab_bar_info,
            );
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

        // Inject orchestrator prompt when start role agent becomes ready.
        // Claude Code fires SessionStart immediately on launch (via hook),
        // transitioning the placeholder from AgentType::None to a real type.
        for tab in tab_manager.tabs_mut() {
            if let Tab::Orchestration {
                id,
                role_pane_ids,
                start_role_index,
                orchestrator_prompt,
                ..
            } = tab
                && orchestrator_prompt.is_some()
                && !ui.orchestration_prompted.contains(id)
            {
                let start_pane_id = &role_pane_ids[*start_role_index];
                // Agent is ready when its session has a real agent type (not placeholder).
                let ready = snapshot.sessions.values().any(|s| {
                    s.pane_id.as_deref() == Some(start_pane_id) && s.agent_type != AgentType::None
                });
                if ready {
                    if let Some(prompt) = orchestrator_prompt.take() {
                        let _ = pane.write_to_pane(start_pane_id, &prompt);
                    }
                    ui.orchestration_prompted.insert(*id);
                }
            }
        }

        // Drain all pending events before re-rendering. This avoids a full
        // render cycle between each keystroke, which eliminates perceived typing
        // latency in PaneInput mode.
        if !crossterm::event::poll(std::time::Duration::from_millis(16))? {
            continue;
        }

        // Process events in a tight loop until the queue is empty.
        loop {
            let ev = event::read()?;

            // Handle terminal resize: update PTY dimensions for all embedded panes.
            if let Event::Resize(w, h) = ev {
                if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
                    let pane_ids = embedded.pane_ids();
                    if !pane_ids.is_empty() {
                        // Width depends on active tab layout:
                        // - Dashboard: right panel is 67% of width
                        // - Mode tab: both agent and side panes are 50% of width
                        let is_mode_tab = tab_manager.active_mode_name().is_some();
                        let pane_width = if is_mode_tab {
                            (w * 50 / 100).saturating_sub(2)
                        } else {
                            (w * 67 / 100).saturating_sub(2)
                        };
                        let pane_count = pane_ids.len() as u16;
                        for pane_id in &pane_ids {
                            let is_focused =
                                embedded.focused_pane_id().as_deref() == Some(pane_id.as_str());
                            let rows = match ui.pane_layout {
                                PaneLayout::Tiled => (h / pane_count).saturating_sub(2),
                                PaneLayout::Stacked => {
                                    if is_focused
                                        || (embedded.focused_pane_id().is_none()
                                            && pane_id == &pane_ids[0])
                                    {
                                        h.saturating_sub(2 + pane_count.saturating_sub(1))
                                    } else {
                                        0 // collapsed panes don't need resize
                                    }
                                }
                            };
                            if rows > 0 {
                                let _ = embedded.resize_pane_pty(pane_id, rows, pane_width);
                            }
                        }
                    }
                }
                break; // re-render after resize
            }

            // Handle mouse events: scroll, text selection, and copy.
            if let Event::Mouse(mouse) = ev {
                // Side pane scroll: works in any UI mode by hit-testing rects
                let mut side_scrolled = false;
                if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
                    let side_rects = ui.side_pane_rects.clone();
                    let scroll_delta = match mouse.kind {
                        crossterm::event::MouseEventKind::ScrollUp => Some(3_isize),
                        crossterm::event::MouseEventKind::ScrollDown => Some(-3_isize),
                        _ => None,
                    };
                    if let Some(delta) = scroll_delta {
                        for (side_id, rect) in &side_rects {
                            if mouse.column >= rect.x
                                && mouse.column < rect.x + rect.width
                                && mouse.row >= rect.y
                                && mouse.row < rect.y + rect.height
                            {
                                embedded.scroll_pane(side_id, delta);
                                side_scrolled = true;
                                break;
                            }
                        }
                    }
                }

                // Click-to-focus for mode tab panes (Normal mode only).
                if ui.mode == UiMode::Normal
                    && matches!(
                        mouse.kind,
                        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
                    )
                {
                    let mut clicked_focus = false;
                    // Check side panes.
                    for (idx, (side_id, rect)) in ui.side_pane_rects.iter().enumerate() {
                        if mouse.column >= rect.x
                            && mouse.column < rect.x + rect.width
                            && mouse.row >= rect.y
                            && mouse.row < rect.y + rect.height
                        {
                            let side_id = side_id.clone();
                            if let Tab::Mode {
                                focused_side_pane_index,
                                ..
                            } = tab_manager.active_tab_mut()
                            {
                                *focused_side_pane_index = Some(idx);
                                clicked_focus = true;
                            }
                            let _ = pane.focus_pane(&side_id);
                            break;
                        }
                    }
                    // Check agent pane area.
                    if !clicked_focus
                        && let Some(rect) = ui.agent_pane_rect
                        && mouse.column >= rect.x
                        && mouse.column < rect.x + rect.width
                        && mouse.row >= rect.y
                        && mouse.row < rect.y + rect.height
                        && let Tab::Mode {
                            focused_side_pane_index,
                            agent_pane_id,
                            ..
                        } = tab_manager.active_tab_mut()
                    {
                        *focused_side_pane_index = None;
                        let _ = pane.focus_pane(agent_pane_id);
                    }
                }

                if !side_scrolled
                    && let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>()
                    && let Some(pane_id) = embedded.focused_pane_id()
                {
                    match mouse.kind {
                        crossterm::event::MouseEventKind::ScrollUp
                            if ui.mode == UiMode::PaneInput =>
                        {
                            embedded.scroll_pane(&pane_id, 3);
                        }
                        crossterm::event::MouseEventKind::ScrollDown
                            if ui.mode == UiMode::PaneInput =>
                        {
                            embedded.scroll_pane(&pane_id, -3);
                        }
                        crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        ) => {
                            if let Some(rect) = ui.focused_pane_rect {
                                let inner_x = rect.x + 1;
                                let inner_y = rect.y + 1;
                                let inner_w = rect.width.saturating_sub(2);
                                let inner_h = rect.height.saturating_sub(2);
                                if mouse.column >= inner_x
                                    && mouse.column < inner_x + inner_w
                                    && mouse.row >= inner_y
                                    && mouse.row < inner_y + inner_h
                                {
                                    let col = mouse.column - inner_x;
                                    let row = mouse.row - inner_y;

                                    // Detect multi-click (double/triple).
                                    // Require same row and nearby column (within 3 cells)
                                    // to handle slight mouse movement between clicks.
                                    let now = std::time::Instant::now();
                                    let click_count = if let Some((t, lc, lr, cnt)) = ui.last_click
                                    {
                                        if now.duration_since(t).as_millis() < 400
                                            && lr == row
                                            && col.abs_diff(lc) <= 3
                                        {
                                            (cnt + 1).min(3)
                                        } else {
                                            1
                                        }
                                    } else {
                                        1
                                    };
                                    ui.last_click = Some((now, col, row, click_count));

                                    match click_count {
                                        2 => {
                                            // Double-click: select word.
                                            if let Some(screen_arc) = embedded.get_screen(&pane_id)
                                                && let Ok(parser) = screen_arc.lock()
                                            {
                                                let offset =
                                                    screen_row_offset(parser.screen(), rect);
                                                let (wstart, wend) = word_bounds_at(
                                                    parser.screen(),
                                                    row,
                                                    col,
                                                    offset,
                                                );
                                                ui.selection = Some(TextSelection {
                                                    start_col: wstart,
                                                    start_row: row,
                                                    end_col: wend,
                                                    end_row: row,
                                                    pane_rect: rect,
                                                });
                                            }
                                        }
                                        3 => {
                                            // Triple-click: select paragraph (contiguous
                                            // non-blank lines around the clicked row).
                                            if let Some(screen_arc) = embedded.get_screen(&pane_id)
                                                && let Ok(parser) = screen_arc.lock()
                                            {
                                                let offset =
                                                    screen_row_offset(parser.screen(), rect);
                                                let screen = parser.screen();
                                                let screen_rows = screen.size().0;
                                                let is_blank_row = |wr: u16| -> bool {
                                                    let sr = wr + offset;
                                                    if sr >= screen_rows {
                                                        return true;
                                                    }
                                                    let cols = screen.size().1;
                                                    (0..cols).all(|c| {
                                                        screen
                                                            .cell(sr, c)
                                                            .map(|cell| {
                                                                let ch = cell.contents();
                                                                ch.is_empty()
                                                                    || ch.trim().is_empty()
                                                            })
                                                            .unwrap_or(true)
                                                    })
                                                };
                                                let mut start_r = row;
                                                while start_r > 0 && !is_blank_row(start_r - 1) {
                                                    start_r -= 1;
                                                }
                                                let mut end_r = row;
                                                while end_r + 1 < inner_h
                                                    && !is_blank_row(end_r + 1)
                                                {
                                                    end_r += 1;
                                                }
                                                ui.selection = Some(TextSelection {
                                                    start_col: 0,
                                                    start_row: start_r,
                                                    end_col: inner_w.saturating_sub(1),
                                                    end_row: end_r,
                                                    pane_rect: rect,
                                                });
                                            }
                                        }
                                        _ => {
                                            // Single click: start drag selection.
                                            ui.selection = Some(TextSelection {
                                                start_col: col,
                                                start_row: row,
                                                end_col: col,
                                                end_row: row,
                                                pane_rect: rect,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        crossterm::event::MouseEventKind::Drag(
                            crossterm::event::MouseButton::Left,
                        ) => {
                            // Extend selection.
                            if let Some(ref mut sel) = ui.selection {
                                let inner_x = sel.pane_rect.x + 1;
                                let inner_y = sel.pane_rect.y + 1;
                                let inner_w = sel.pane_rect.width.saturating_sub(2);
                                let inner_h = sel.pane_rect.height.saturating_sub(2);
                                sel.end_col = mouse
                                    .column
                                    .saturating_sub(inner_x)
                                    .min(inner_w.saturating_sub(1));
                                sel.end_row = mouse
                                    .row
                                    .saturating_sub(inner_y)
                                    .min(inner_h.saturating_sub(1));
                            }
                        }
                        crossterm::event::MouseEventKind::Up(
                            crossterm::event::MouseButton::Left,
                        ) => {
                            let was_multiclick = ui
                                .last_click
                                .map(|(_, _, _, cnt)| cnt >= 2)
                                .unwrap_or(false);
                            // Only copy when the selection is a real drag or multi-click,
                            // not a plain single click.
                            if let Some(ref sel) = ui.selection
                                && (was_multiclick
                                    || sel.start_col != sel.end_col
                                    || sel.start_row != sel.end_row)
                                && let Some(screen_arc) = embedded.get_screen(&pane_id)
                                && let Ok(parser) = screen_arc.lock()
                            {
                                let offset = screen_row_offset(parser.screen(), sel.pane_rect);
                                let text = extract_selection_text(parser.screen(), sel, offset);
                                if !text.is_empty() {
                                    copy_to_clipboard_osc52(&text);
                                    ui.status_message = Some((
                                        "Copied to clipboard".to_string(),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                            // Keep selection visible after multi-click; clear on single-click.
                            if !was_multiclick {
                                ui.selection = None;
                            }
                        }
                        _ => {}
                    }
                }
                if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
                continue;
            }

            // Handle paste: wrap in bracketed paste sequences if the child app enabled it.
            if let Event::Paste(text) = ev {
                if ui.mode == UiMode::PaneInput
                    && let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>()
                    && let Some(pane_id) = embedded.focused_pane_id()
                {
                    embedded.reset_scrollback(&pane_id);
                    let use_bracketed = embedded
                        .get_screen(&pane_id)
                        .and_then(|s| s.lock().ok().map(|p| p.screen().bracketed_paste()))
                        .unwrap_or(false);
                    let mut payload = Vec::new();
                    if use_bracketed {
                        payload.extend_from_slice(b"\x1b[200~");
                    }
                    payload.extend_from_slice(text.as_bytes());
                    if use_bracketed {
                        payload.extend_from_slice(b"\x1b[201~");
                    }
                    let _ = embedded.write_raw_bytes(&pane_id, &payload);
                }
                if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
                continue;
            }

            let Event::Key(key) = ev else {
                if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
                continue;
            };

            // Only handle key-press events (ignore release/repeat on platforms that send them).
            if key.kind != crossterm::event::KeyEventKind::Press {
                if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
                continue;
            }

            // 1..9 in Normal mode: jump to card N and focus its pane
            let mut shortcut_handled = false;
            if ui.mode == UiMode::Normal
                && let KeyCode::Char(c @ '1'..='9') = key.code
                && key.modifiers == KeyModifiers::NONE
            {
                let idx = (c as usize) - ('1' as usize);
                // Dismiss idle art on the target card
                if let Some((sid, _)) = filtered.get(idx)
                    && let Some(entry) = ui.idle_art_cache.get_mut(*sid)
                {
                    entry.dismissed = true;
                }
                focus_deck(idx, &mut ui, &filtered, &snapshot, &state, &*pane);
                shortcut_handled = true;
            }

            // ---------------------------------------------------------------
            // Global Ctrl+key shortcuts (work from any mode / future pane focus)
            // ---------------------------------------------------------------
            if !shortcut_handled && key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    // Ctrl+d: enter Normal (command) mode, stay on current tab
                    KeyCode::Char('d') => {
                        // Re-suppress the prompt in reactive panes when
                        // leaving PaneInput so automated output stays clean.
                        if ui.mode == UiMode::PaneInput
                            && let Some(embedded) =
                                pane.as_any().downcast_ref::<EmbeddedPaneController>()
                            && let Some(focused_id) = embedded.focused_pane_id()
                            && let Tab::Mode { mode_manager, .. } = tab_manager.active_tab_mut()
                            && mode_manager.is_reactive_pane(&focused_id)
                        {
                            let _ = pane.write_to_pane(
                                &focused_id,
                                "export PS1= PS2= PROMPT= && printf '\\x1b[3J\\x1b[2J\\x1b[H'",
                            );
                        }
                        ui.mode = UiMode::Normal;
                        shortcut_handled = true;
                    }
                    // Ctrl+t: toggle layout
                    KeyCode::Char('t') => {
                        ui.pane_layout = match ui.pane_layout {
                            PaneLayout::Stacked => PaneLayout::Tiled,
                            PaneLayout::Tiled => PaneLayout::Stacked,
                        };
                        let mode_name = match ui.pane_layout {
                            PaneLayout::Stacked => "stacked",
                            PaneLayout::Tiled => "tiled",
                        };
                        // Resize PTYs to match new layout dimensions.
                        // Mode tabs use 50% width; dashboard uses 67%.
                        if let Some(embedded) =
                            pane.as_any().downcast_ref::<EmbeddedPaneController>()
                        {
                            let frame_area = terminal.get_frame().area();
                            let is_mode_tab = tab_manager.active_mode_name().is_some();
                            let pane_width = if is_mode_tab {
                                (frame_area.width / 2).saturating_sub(2)
                            } else {
                                (frame_area.width * 67 / 100).saturating_sub(2)
                            };
                            let pane_ids = embedded.pane_ids();
                            let pane_count = pane_ids.len() as u16;
                            if pane_count > 0 {
                                for pane_id in &pane_ids {
                                    let rows = match ui.pane_layout {
                                        PaneLayout::Tiled => {
                                            (frame_area.height / pane_count).saturating_sub(2)
                                        }
                                        PaneLayout::Stacked => {
                                            let is_focused = embedded.focused_pane_id().as_deref()
                                                == Some(pane_id.as_str());
                                            if is_focused
                                                || (embedded.focused_pane_id().is_none()
                                                    && pane_id == &pane_ids[0])
                                            {
                                                frame_area.height.saturating_sub(
                                                    2 + pane_count.saturating_sub(1),
                                                )
                                            } else {
                                                0
                                            }
                                        }
                                    };
                                    if rows > 0 {
                                        let _ = embedded.resize_pane_pty(pane_id, rows, pane_width);
                                    }
                                }
                            }
                        }
                        ui.status_message =
                            Some((format!("Layout: {mode_name}"), std::time::Instant::now()));
                        shortcut_handled = true;
                    }
                    // Ctrl+n: new pane (open directory picker)
                    KeyCode::Char('n') => {
                        ui.mode = UiMode::DirPicker;
                        ui.dir_picker = Some(DirPickerState::new(
                            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
                        ));
                        shortcut_handled = true;
                    }
                    // Ctrl+w: close selected pane (or entire mode tab if it's the agent pane)
                    KeyCode::Char('w') => {
                        if let Some(sid) =
                            filtered.get(ui.selected_index).map(|(id, _)| (*id).clone())
                            && let Some(session) = snapshot.sessions.get(&sid)
                            && let Some(ref pane_id) = session.pane_id
                        {
                            let closed_pane_id = pane_id.clone();
                            // Check if this pane belongs to a mode or orchestration tab.
                            let mode_tab_idx = tab_manager
                                .tab_index_for_agent_pane(pane_id)
                                .or_else(|| tab_manager.tab_index_for_pane(pane_id));
                            if let Some(tab_idx) = mode_tab_idx {
                                // Close the entire tab (agent + side panes, or all role panes).
                                if let Ok(side_ids) = tab_manager.close_tab(tab_idx) {
                                    let mut st = state.blocking_write();
                                    for id in &side_ids {
                                        st.unregister_pane(id);
                                        st.pane_role_map.remove(id);
                                        st.pane_cwd_map.remove(id);
                                    }
                                    st.sessions.remove(&sid);
                                    st.unregister_pane(&closed_pane_id);
                                    st.pane_role_map.remove(&closed_pane_id);
                                    st.pane_cwd_map.remove(&closed_pane_id);
                                    drop(st);
                                }
                                let _ = pane.close_pane(&closed_pane_id);
                                ui.pane_metadata.remove(&closed_pane_id);
                                let area = terminal.get_frame().area();
                                resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                            } else {
                                // Plain dashboard pane — close just this one.
                                let _ = pane.close_pane(pane_id);
                                let mut st = state.blocking_write();
                                st.sessions.remove(&sid);
                                st.unregister_pane(&closed_pane_id);
                                drop(st);
                                ui.pane_metadata.remove(&closed_pane_id);
                            }
                            if ui.mode == UiMode::PaneInput {
                                ui.mode = UiMode::Normal;
                            }
                            // Clamp selected_index so it doesn't point past
                            // the now-shorter card list.
                            if ui.selected_index > 0 {
                                ui.selected_index = ui.selected_index.saturating_sub(1);
                            }
                            ui.status_message = Some((
                                format!("Closed pane {closed_pane_id}"),
                                std::time::Instant::now(),
                            ));
                        }
                        shortcut_handled = true;
                    }
                    // Ctrl+PageDown: next tab
                    KeyCode::PageDown => {
                        if tab_manager.show_tab_bar() {
                            let prev_idx = tab_manager.active_index();
                            tab_manager.switch_to(prev_idx + 1);
                            if prev_idx != tab_manager.active_index() {
                                let area = terminal.get_frame().area();
                                resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                                resize_mode_tab_panes(&*pane, &tab_manager, area);
                            }
                        }
                        shortcut_handled = true;
                    }
                    // Ctrl+PageUp: previous tab
                    KeyCode::PageUp => {
                        if tab_manager.show_tab_bar() {
                            let prev_idx = tab_manager.active_index();
                            if prev_idx > 0 {
                                tab_manager.switch_to(prev_idx - 1);
                                if prev_idx != tab_manager.active_index() {
                                    let area = terminal.get_frame().area();
                                    resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                                    resize_mode_tab_panes(&*pane, &tab_manager, area);
                                }
                            }
                        }
                        shortcut_handled = true;
                    }
                    _ => {}
                }
            }

            // Tab / Shift+Tab / Left / Right / h / l: cycle tabs in Normal mode
            if !shortcut_handled && ui.mode == UiMode::Normal {
                match key.code {
                    KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                        let count = tab_manager.tab_count();
                        if count > 0 {
                            let prev_idx = tab_manager.active_index();
                            let next = (prev_idx + 1) % count;
                            tab_manager.switch_to(next);
                            if prev_idx != tab_manager.active_index() {
                                let area = terminal.get_frame().area();
                                resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                                resize_mode_tab_panes(&*pane, &tab_manager, area);
                            }
                        }
                        shortcut_handled = true;
                    }
                    KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                        let count = tab_manager.tab_count();
                        if count > 0 {
                            let prev_idx = tab_manager.active_index();
                            let prev = (prev_idx + count - 1) % count;
                            tab_manager.switch_to(prev);
                            if prev_idx != tab_manager.active_index() {
                                let area = terminal.get_frame().area();
                                resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                                resize_mode_tab_panes(&*pane, &tab_manager, area);
                            }
                        }
                        shortcut_handled = true;
                    }
                    _ => {}
                }
            }

            let selected_id: Option<String> =
                filtered.get(ui.selected_index).map(|(id, _)| (*id).clone());

            // On a mode tab in Normal mode, j/k navigate side panes, Enter focuses, Esc resets.
            if !shortcut_handled
                && ui.mode == UiMode::Normal
                && let Tab::Mode {
                    focused_side_pane_index,
                    mode_manager,
                    agent_pane_id,
                    ..
                } = tab_manager.active_tab_mut()
            {
                let side_ids = mode_manager.managed_pane_ids();
                let side_count = side_ids.len();
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        *focused_side_pane_index = match *focused_side_pane_index {
                            None => {
                                if side_count > 0 {
                                    Some(0)
                                } else {
                                    None
                                }
                            }
                            Some(i) if i + 1 < side_count => Some(i + 1),
                            Some(_) => None, // wrap back to agent pane
                        };
                        // Sync embedded controller focus so the visual highlight
                        // matches (prevents stale cyan border on a previous pane).
                        let focus_id = match *focused_side_pane_index {
                            None => agent_pane_id.clone(),
                            Some(i) => side_ids
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| agent_pane_id.clone()),
                        };
                        let _ = pane.focus_pane(&focus_id);
                        shortcut_handled = true;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        *focused_side_pane_index = match *focused_side_pane_index {
                            None => {
                                if side_count > 0 {
                                    Some(side_count - 1)
                                } else {
                                    None
                                }
                            }
                            Some(0) => None,
                            Some(i) => Some(i - 1),
                        };
                        let focus_id = match *focused_side_pane_index {
                            None => agent_pane_id.clone(),
                            Some(i) => side_ids
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| agent_pane_id.clone()),
                        };
                        let _ = pane.focus_pane(&focus_id);
                        shortcut_handled = true;
                    }
                    KeyCode::Enter => {
                        let target_pane_id = match *focused_side_pane_index {
                            None => agent_pane_id.clone(),
                            Some(i) => side_ids
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| agent_pane_id.clone()),
                        };
                        let is_reactive = mode_manager.is_reactive_pane(&target_pane_id);
                        if pane.focus_pane(&target_pane_id).is_ok() {
                            ui.mode = UiMode::PaneInput;
                            // Restore a minimal prompt so the user can
                            // interact with the shell in this reactive pane.
                            if is_reactive {
                                let _ = pane.write_to_pane(
                                    &target_pane_id,
                                    "export PS1='$ ' PS2='> ' PROMPT='$ '",
                                );
                            }
                            ui.status_message = Some((
                                "PaneInput mode — type to interact, Ctrl+d for dashboard"
                                    .to_string(),
                                std::time::Instant::now(),
                            ));
                        }
                        shortcut_handled = true;
                    }
                    KeyCode::Esc => {
                        *focused_side_pane_index = None;
                        let _ = pane.focus_pane(agent_pane_id);
                        shortcut_handled = true;
                    }
                    _ => {}
                }
            }

            // Mode-specific key handling (skip if a global shortcut was handled).
            let result = if shortcut_handled {
                KeyResult::Continue
            } else {
                match ui.mode {
                    UiMode::Normal => handle_normal_key(key, &mut ui, total),
                    UiMode::Filter => handle_filter_key(key, &mut ui),
                    UiMode::Help => handle_help_key(key, &mut ui),
                    UiMode::Rename => handle_rename_key(key, &mut ui, selected_id.as_deref()),
                    UiMode::DirPicker => handle_dir_picker_key(key, &mut ui),
                    UiMode::NewPaneForm => handle_new_pane_form_key(key, &mut ui),
                    UiMode::PaneInput => handle_pane_input_key(key),
                    UiMode::StarPrompt => handle_star_prompt_key(key, &mut ui),
                    UiMode::ConfigGenPrompt => handle_config_gen_prompt_key(key, &mut ui),
                    UiMode::QuitConfirm => handle_quit_confirm_key(key, &mut ui),
                }
            };

            match result {
                KeyResult::Quit => break 'outer,
                KeyResult::Focus => {
                    // Dismiss idle art on the focused card
                    if let Some(ref sid) = selected_id
                        && let Some(entry) = ui.idle_art_cache.get_mut(sid)
                    {
                        entry.dismissed = true;
                    }
                    if let Some(ref sid) = selected_id
                        && let Some(session) = snapshot.sessions.get(sid)
                    {
                        if let Some(ref pane_id) = session.pane_id {
                            if let Some(tab_idx) = tab_manager.tab_index_for_pane(pane_id) {
                                tab_manager.switch_to(tab_idx);
                                let area = terminal.get_frame().area();
                                resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                                resize_mode_tab_panes(&*pane, &tab_manager, area);
                            }
                            match pane.focus_pane(pane_id) {
                                Ok(()) => {
                                    ui.mode = UiMode::PaneInput;
                                    // Reset dismissed flags so art reappears when
                                    // the user returns to the dashboard.
                                    for entry in ui.idle_art_cache.values_mut() {
                                        entry.dismissed = false;
                                    }
                                    ui.status_message = Some((
                                        "PaneInput mode — type to interact, Ctrl+d for dashboard"
                                            .to_string(),
                                        std::time::Instant::now(),
                                    ));
                                }
                                Err(PaneError::CommandFailed(ref msg)) => {
                                    state.blocking_write().sessions.remove(sid);
                                    ui.status_message = Some((
                                        format!("Removed stale session: {msg}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    ui.status_message = Some((
                                        format!("Pane focus failed: {e}"),
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
                KeyResult::RequestConfigGen => {
                    if let Some(ref sid) = selected_id
                        && let Some(session) = snapshot.sessions.get(sid)
                        && let Some(ref pane_id) = session.pane_id
                        && let Some(ref cwd) = session.cwd
                    {
                        ui.config_gen_target = Some((pane_id.clone(), cwd.clone()));
                        ui.config_gen_selected = 0;
                        ui.mode = UiMode::ConfigGenPrompt;
                    } else {
                        ui.status_message = Some((
                            "No active agent session to send prompt to.".to_string(),
                            std::time::Instant::now(),
                        ));
                    }
                }
                KeyResult::NewPane(req) => {
                    if pane.is_available() {
                        let dir_str = req.dir.display().to_string();

                        // Orchestration path — manage own panes, no agent pane.
                        if let Some(orch_config) = req.orchestration_config {
                            let prompt = prepare_orchestrator_prompt(&orch_config, &dir_str);
                            match tab_manager.open_orchestration_tab(&orch_config, &dir_str, prompt)
                            {
                                Ok((_tab_idx, role_pane_ids)) => {
                                    {
                                        let mut st = state.blocking_write();
                                        for id in &role_pane_ids {
                                            st.register_pane(id.clone());
                                            st.insert_placeholder_session(
                                                id.clone(),
                                                Some(dir_str.clone()),
                                            );
                                        }
                                        // Register pane-to-role and pane-to-cwd mappings for work-done resolution.
                                        for (i, role) in orch_config.roles.iter().enumerate() {
                                            st.pane_role_map.insert(
                                                role_pane_ids[i].clone(),
                                                role.name.clone(),
                                            );
                                            st.pane_cwd_map
                                                .insert(role_pane_ids[i].clone(), dir_str.clone());
                                        }
                                    }
                                    // Register display names for role panes.
                                    for (i, role) in orch_config.roles.iter().enumerate() {
                                        ui.pane_display_names
                                            .insert(role_pane_ids[i].clone(), role.name.clone());
                                        ui.pane_names
                                            .insert(role_pane_ids[i].clone(), role.name.clone());
                                    }

                                    // Focus the start role's pane.
                                    let start_idx =
                                        orch_config.roles.iter().position(|r| r.start).unwrap_or(0);
                                    let _ = pane.focus_pane(&role_pane_ids[start_idx]);
                                    ui.mode = UiMode::PaneInput;

                                    // Resize role panes to dashboard layout.
                                    let area = terminal.get_frame().area();
                                    resize_dashboard_panes(&*pane, &ui, &tab_manager, area);

                                    // Start role commands after resize.
                                    for (i, role) in orch_config.roles.iter().enumerate() {
                                        let _ =
                                            pane.write_to_pane(&role_pane_ids[i], &role.command);
                                    }

                                    ui.status_message = Some((
                                        format!("Activated orchestration: {}", orch_config.name),
                                        std::time::Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    ui.status_message = Some((
                                        format!("Orchestration failed: {e}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        } else {
                            // For mode tabs, create the agent pane as an empty
                            // shell so the PTY can be resized to the correct
                            // dimensions before the command starts.  This avoids
                            // the process seeing the default 80×24 size.
                            let is_mode = req.mode_config.is_some();
                            let cmd = if req.command.is_empty() || is_mode {
                                None
                            } else {
                                Some(req.command.as_str())
                            };
                            match pane.create_pane(cmd, Some(&dir_str)) {
                                Ok(new_id) => {
                                    // Register so only events from our panes are accepted,
                                    // and create a placeholder session for an immediate dashboard card.
                                    {
                                        let mut st = state.blocking_write();
                                        st.register_pane(new_id.clone());
                                        st.insert_placeholder_session(
                                            new_id.clone(),
                                            Some(dir_str.clone()),
                                        );
                                    }
                                    if !req.name.is_empty() {
                                        let _ = pane.rename_pane(&new_id, &req.name);
                                        ui.pane_display_names
                                            .insert(new_id.clone(), req.name.clone());
                                        ui.pane_names.insert(new_id.clone(), req.name.clone());
                                    }
                                    let mode_name_for_save =
                                        req.mode_config.as_ref().map(|m| m.name.clone());
                                    ui.pane_metadata.insert(
                                        new_id.clone(),
                                        config::SavedPane {
                                            dir: dir_str.clone(),
                                            name: req.name.clone(),
                                            command: req.command,
                                            mode: mode_name_for_save,
                                        },
                                    );

                                    if let Some(mode_config) = req.mode_config {
                                        // Mode selected — open a mode tab.
                                        let mode_name = mode_config.name.clone();
                                        match tab_manager.open_mode_tab(
                                            &mode_config,
                                            &dir_str,
                                            new_id.clone(),
                                        ) {
                                            Ok((_tab_idx, side_ids)) => {
                                                for id in &side_ids {
                                                    state
                                                        .blocking_write()
                                                        .register_pane(id.clone());
                                                }
                                                let _ = pane.focus_pane(&new_id);
                                                ui.mode = UiMode::PaneInput;
                                                if let Some(embedded) =
                                                    pane.as_any()
                                                        .downcast_ref::<EmbeddedPaneController>()
                                                {
                                                    let frame_area = terminal.get_frame().area();
                                                    let half_width =
                                                        (frame_area.width / 2).saturating_sub(2);
                                                    let agent_rows =
                                                        frame_area.height.saturating_sub(3);
                                                    if agent_rows > 0 && half_width > 0 {
                                                        let _ = embedded.resize_pane_pty(
                                                            &new_id, agent_rows, half_width,
                                                        );
                                                    }
                                                    let side_count = side_ids.len().max(1) as u16;
                                                    let side_rows = (frame_area.height
                                                        / side_count)
                                                        .saturating_sub(2);
                                                    if side_rows > 0 && half_width > 0 {
                                                        for id in &side_ids {
                                                            let _ = embedded.resize_pane_pty(
                                                                id, side_rows, half_width,
                                                            );
                                                        }
                                                    }
                                                }
                                                // Start commands now that panes are correctly sized
                                                let _ = tab_manager.start_mode_commands();
                                                // Send the agent pane command after resize
                                                // so it starts at the correct PTY dimensions.
                                                if let Some(ref init_cmd) = mode_config.init_command
                                                {
                                                    let _ = pane.write_to_pane(&new_id, init_cmd);
                                                }
                                                if let Some(saved) = ui.pane_metadata.get(&new_id) {
                                                    let agent_cmd = saved.command.clone();
                                                    if !agent_cmd.is_empty() {
                                                        let _ =
                                                            pane.write_to_pane(&new_id, &agent_cmd);
                                                    }
                                                }
                                                ui.status_message = Some((
                                                    format!("Activated mode: {mode_name}"),
                                                    std::time::Instant::now(),
                                                ));
                                            }
                                            Err(e) => {
                                                let _ = pane.close_pane(&new_id);
                                                ui.status_message = Some((
                                                    format!("Mode activation failed: {e}"),
                                                    std::time::Instant::now(),
                                                ));
                                            }
                                        }
                                    } else {
                                        // No mode — regular dashboard pane.
                                        let _ = pane.focus_pane(&new_id);
                                        ui.mode = UiMode::PaneInput;
                                        ui.selected_index = filtered.len();
                                        if let Some(embedded) =
                                            pane.as_any().downcast_ref::<EmbeddedPaneController>()
                                        {
                                            let frame_area = terminal.get_frame().area();
                                            let right_width =
                                                (frame_area.width * 67 / 100).saturating_sub(2);
                                            let pane_count = embedded.pane_ids().len() as u16;
                                            let rows = match ui.pane_layout {
                                                PaneLayout::Tiled => (frame_area.height
                                                    / pane_count.max(1))
                                                .saturating_sub(2),
                                                PaneLayout::Stacked => {
                                                    frame_area.height.saturating_sub(
                                                        2 + pane_count.saturating_sub(1),
                                                    )
                                                }
                                            };
                                            if rows > 0 && right_width > 0 {
                                                let _ = embedded.resize_pane_pty(
                                                    &new_id,
                                                    rows,
                                                    right_width,
                                                );
                                            }
                                        }
                                        ui.status_message = Some((
                                            format!("Created pane {new_id} in {dir_str}"),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                                Err(e) => {
                                    ui.status_message = Some((
                                        format!("New pane failed: {e}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        } // close else (non-orchestration path)
                    }
                }
                KeyResult::SendConfigGenPrompt { pane_id, cwd } => {
                    let prompt = crate::config_gen::config_gen_prompt(&cwd);
                    match pane.write_to_pane(&pane_id, &prompt) {
                        Ok(()) => {
                            // Focus the pane so user can press Enter to execute.
                            if let Some(tab_idx) = tab_manager.tab_index_for_pane(&pane_id) {
                                tab_manager.switch_to(tab_idx);
                                let area = terminal.get_frame().area();
                                resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                                resize_mode_tab_panes(&*pane, &tab_manager, area);
                            }
                            let _ = pane.focus_pane(&pane_id);
                            ui.mode = UiMode::PaneInput;
                            ui.status_message = Some((
                                "Config prompt sent — press Enter to execute.".to_string(),
                                std::time::Instant::now(),
                            ));
                        }
                        Err(e) => {
                            ui.status_message = Some((
                                format!("Failed to send config prompt: {e}"),
                                std::time::Instant::now(),
                            ));
                        }
                    }
                }
                KeyResult::ForwardToPane(bytes) => {
                    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>()
                        && let Some(pane_id) = embedded.focused_pane_id()
                    {
                        // Reset scrollback to live output on any keystroke.
                        embedded.reset_scrollback(&pane_id);
                        if let Err(e) = embedded.write_raw_bytes(&pane_id, &bytes) {
                            ui.status_message =
                                Some((format!("PTY write failed: {e}"), std::time::Instant::now()));
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

            // In PaneInput mode, drain remaining events to reduce typing latency.
            // In all other modes, break immediately to re-render so UI state
            // transitions (mode changes, focus, dialogs) take effect before the
            // next event is processed.
            if ui.mode != UiMode::PaneInput
                || !crossterm::event::poll(std::time::Duration::from_millis(0))?
            {
                break;
            }
        } // end inner event-drain loop
    }

    // Tear down all mode tabs (clean up their panes).
    for i in (1..tab_manager.tab_count()).rev() {
        if let Ok(ids) = tab_manager.close_tab(i) {
            for id in ids {
                state.blocking_write().unregister_pane(&id);
            }
        }
    }

    // Auto-save current pane session for --continue restore.
    // Reconcile pane_metadata with the authoritative live pane registry so that
    // externally-closed panes are pruned and renames are captured.
    {
        let live_panes = state.blocking_read().managed_pane_ids.clone();
        ui.pane_metadata.retain(|id, _| live_panes.contains(id));
        for (id, meta) in ui.pane_metadata.iter_mut() {
            if let Some(name) = ui.pane_display_names.get(id) {
                meta.name = name.clone();
            }
        }
        let session = config::SavedSession {
            panes: {
                let mut ids: Vec<&String> = ui.pane_metadata.keys().collect();
                ids.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
                ids.into_iter()
                    .filter_map(|id| ui.pane_metadata.get(id).cloned())
                    .collect()
            },
        };
        if session.panes.is_empty() {
            if let Err(e) = config::SavedSession::clear() {
                ui.session_warnings
                    .push(format!("Warning: failed to clear session: {e}"));
            }
        } else if let Err(e) = session.save() {
            ui.session_warnings
                .push(format!("Warning: failed to save session: {e}"));
        }
    }

    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
    );
    ratatui::restore();

    // Flush accumulated session warnings now that the terminal is restored.
    for warning in &ui.session_warnings {
        eprintln!("{warning}");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_frame(
    frame: &mut Frame,
    state: &AppState,
    ui: &mut UiState,
    filtered: &[(&String, &SessionState)],
    tick: u64,
    has_pane_control: bool,
    pane_controller: &dyn PaneController,
    pane_layout: PaneLayout,
    tab_view: &ActiveTabView,
    tab_bar: &TabBarInfo,
) {
    let area = frame.area();
    let palette = ui.palette;
    ui.side_pane_rects.clear();
    ui.agent_pane_rect = None;

    let active_mode_name = match tab_view {
        ActiveTabView::Dashboard { .. } | ActiveTabView::Orchestration { .. } => None,
        ActiveTabView::Mode { mode_name, .. } => Some(mode_name.as_str()),
    };

    // Fill entire frame with terminal background so nothing falls through
    // to the alternate screen default (which may be black on light themes).
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.terminal_bg)),
        area,
    );

    // Layout: optional tab bar at top, main content, hints bar at bottom.
    let (main_area, hints_area) = if tab_bar.show {
        let chunks = Layout::vertical([
            Constraint::Length(1), // tab bar
            Constraint::Fill(1),   // main content
            Constraint::Length(1), // hints bar
        ])
        .split(area);

        // Render the tab bar.
        let titles: Vec<Line> = tab_bar
            .labels
            .iter()
            .map(|l| Line::from(format!(" {l} ")))
            .collect();
        // Fill tab bar row with distinct background.
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.tab_bar_bg)),
            chunks[0],
        );
        // Active tab: inverted colors for high contrast.
        let tabs_widget = Tabs::new(titles)
            .select(tab_bar.active_index)
            .style(Style::default().fg(palette.text_muted))
            .highlight_style(
                Style::default()
                    .fg(palette.terminal_bg)
                    .bg(palette.text_secondary)
                    .add_modifier(Modifier::BOLD),
            )
            .divider("│");
        frame.render_widget(tabs_widget, chunks[0]);

        (chunks[1], chunks[2])
    } else {
        let chunks = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).split(area);
        (chunks[0], chunks[1])
    };

    // Determine if we have embedded terminal panes to show on the right.
    let embedded = pane_controller
        .as_any()
        .downcast_ref::<EmbeddedPaneController>();

    // ── Mode tab rendering ─────────────────────────────────────────────
    if let ActiveTabView::Mode {
        agent_pane_id,
        side_pane_ids,
        focused_side_pane_index,
        ..
    } = tab_view
    {
        render_mode_tab(
            frame,
            ui,
            embedded,
            agent_pane_id,
            side_pane_ids,
            main_area,
            hints_area,
            has_pane_control,
            active_mode_name,
            tab_bar.show,
            *focused_side_pane_index,
        );
        return;
    }

    // ── Dashboard / Orchestration tab rendering ──────────────────────
    let all_pane_ids = embedded.map(|e| e.pane_ids()).unwrap_or_default();
    let pane_ids: Vec<String> = match tab_view {
        ActiveTabView::Dashboard { exclude_pane_ids } => all_pane_ids
            .into_iter()
            .filter(|id| !exclude_pane_ids.contains(id))
            .collect(),
        ActiveTabView::Orchestration { role_pane_ids } => all_pane_ids
            .into_iter()
            .filter(|id| role_pane_ids.contains(id))
            .collect(),
        _ => unreachable!(),
    };
    let has_terminal_panes = !pane_ids.is_empty();

    let (dashboard_area, panes_area) = if has_terminal_panes {
        let chunks = Layout::horizontal([Constraint::Percentage(33), Constraint::Percentage(67)])
            .split(main_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (main_area, None)
    };

    if state.sessions.is_empty() {
        let vertical = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(dashboard_area);
        let msg = Paragraph::new(format!(
            "No active sessions. Press {MOD_KEY}+n to create a pane."
        ))
        .style(Style::default().fg(palette.text_secondary))
        .centered();
        frame.render_widget(msg, vertical[1]);
        render_bottom_bar(frame, ui, hints_area, has_pane_control, tab_bar.show, false);

        if let Some(right) = panes_area {
            ui.focused_pane_rect = render_terminal_panes(
                frame,
                embedded,
                right,
                &pane_ids,
                pane_layout,
                &ui.pane_display_names,
                &ui.selection,
                palette,
                None,
            );
        }

        render_overlays(frame, ui, active_mode_name, palette);
        return;
    }

    let sessions: Vec<&SessionState> = filtered.iter().map(|(_, s)| *s).collect();
    let session_ids: Vec<&String> = filtered.iter().map(|(id, _)| *id).collect();

    let cols = grid_columns(dashboard_area.width);

    // Choose card density based on available vertical space
    // wide = true when each column has inner width >= 60 (card border takes 2 chars)
    let col_width = dashboard_area.width / cols.max(1) as u16;
    let wide = col_width.saturating_sub(2) >= 60;
    // 1 row for title + 1 row for stats bar at bottom of dashboard
    let available_for_density = dashboard_area.height.saturating_sub(2);
    let density = choose_density(sessions.len(), cols, available_for_density, wide);
    let card_height = density.card_height(wide);

    // Update idle art state machine
    update_idle_art(
        &mut ui.idle_art_cache,
        &ui.config.idle_art,
        &state.sessions,
        density,
    );

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
        Span::styled(title_text, Style::default().fg(palette.text_secondary)),
    ]));

    if sessions.is_empty() {
        // All filtered out
        let vertical = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .split(dashboard_area);
        frame.render_widget(title, vertical[0]);

        let msg = Paragraph::new("No sessions match filter.")
            .style(Style::default().fg(palette.text_secondary))
            .centered();
        let inner = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(vertical[1]);
        frame.render_widget(msg, inner[1]);

        render_stats_bar(
            frame,
            &state.aggregate_stats(),
            vertical[2],
            active_mode_name,
            palette,
        );
        render_bottom_bar(frame, ui, hints_area, has_pane_control, tab_bar.show, false);
        // Still render live terminal panes even when filter matches zero sessions.
        if let Some(right) = panes_area {
            ui.focused_pane_rect = render_terminal_panes(
                frame,
                embedded,
                right,
                &pane_ids,
                pane_layout,
                &ui.pane_display_names,
                &ui.selection,
                palette,
                None,
            );
        }
        render_overlays(frame, ui, active_mode_name, palette);
        return;
    }

    let all_rows: Vec<&[&SessionState]> = sessions.chunks(cols).collect();
    let all_row_ids: Vec<&[&String]> = session_ids.chunks(cols).collect();
    let total_rows = all_rows.len();

    // Calculate how many rows fit in the available area
    let visible_rows = (available_for_density / card_height).max(1) as usize;

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
    constraints.push(Constraint::Length(1)); // stats bar

    let row_chunks = Layout::vertical(constraints).split(dashboard_area);

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
            let idle_art = ids.get(col_idx).and_then(|id| ui.idle_art_cache.get(*id));
            render_session_card(
                frame,
                col_chunks[col_idx],
                session,
                tick,
                is_selected,
                display_name,
                card_number,
                density,
                palette,
                idle_art,
            );
        }
    }

    // Stats bar at bottom of dashboard area
    let stats_area = row_chunks[row_chunks.len() - 1];
    render_stats_bar(
        frame,
        &state.aggregate_stats(),
        stats_area,
        active_mode_name,
        palette,
    );

    // Full-width hints bar
    render_bottom_bar(frame, ui, hints_area, has_pane_control, tab_bar.show, false);

    // Render terminal panes on the right side
    if let Some(right) = panes_area {
        ui.focused_pane_rect = render_terminal_panes(
            frame,
            embedded,
            right,
            &pane_ids,
            pane_layout,
            &ui.pane_display_names,
            &ui.selection,
            palette,
            None,
        );
    }

    render_overlays(frame, ui, active_mode_name, palette);
}

fn render_overlays(
    frame: &mut Frame,
    ui: &mut UiState,
    active_mode_name: Option<&str>,
    palette: ColorPalette,
) {
    if ui.mode == UiMode::Help {
        render_help_overlay(frame, active_mode_name, palette);
    }
    if ui.mode == UiMode::DirPicker
        && let Some(picker) = ui.dir_picker.as_mut()
    {
        render_dir_picker(frame, picker, palette);
    }
    if ui.mode == UiMode::NewPaneForm
        && let Some(ref form) = ui.new_pane_form
    {
        render_new_pane_form(frame, form, palette);
    }
    if ui.mode == UiMode::StarPrompt {
        render_star_prompt(frame, palette);
    }
    if ui.mode == UiMode::ConfigGenPrompt {
        render_config_gen_prompt(frame, ui.config_gen_selected, palette);
    }
    if ui.mode == UiMode::QuitConfirm {
        render_quit_confirm(frame, ui.quit_confirm_selected, palette);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_terminal_panes(
    frame: &mut Frame,
    embedded: Option<&EmbeddedPaneController>,
    area: Rect,
    pane_ids: &[String],
    layout: PaneLayout,
    display_names: &HashMap<String, String>,
    selection: &Option<TextSelection>,
    palette: ColorPalette,
    visual_focus_id: Option<&str>,
) -> Option<Rect> {
    let ctrl = embedded?;
    if pane_ids.is_empty() {
        return None;
    }
    let focused_id = visual_focus_id
        .map(|s| s.to_string())
        .or_else(|| ctrl.focused_pane_id());

    // Get pane info for display names
    let pane_infos = ctrl.list_panes().unwrap_or_default();
    let pane_name = |id: &str| -> String {
        if let Some(name) = display_names.get(id) {
            return name.clone();
        }
        if let Some(info) = pane_infos.iter().find(|p| p.pane_id == id)
            && !info.title.is_empty()
        {
            return info.title.clone();
        }
        format!("pane {id}")
    };

    // Track the focused pane's rect and screen for hardware cursor positioning.
    let mut focused_pane_rect: Option<Rect> = None;
    let mut focused_screen: Option<std::sync::Arc<std::sync::Mutex<vt100::Parser>>> = None;

    match layout {
        PaneLayout::Tiled => {
            let constraints: Vec<Constraint> = pane_ids
                .iter()
                .map(|_| Constraint::Ratio(1, pane_ids.len() as u32))
                .collect();
            let chunks = Layout::vertical(constraints).split(area);
            for (i, pane_id) in pane_ids.iter().enumerate() {
                if let Some(screen) = ctrl.get_screen(pane_id) {
                    let focused = focused_id.as_deref() == Some(pane_id.as_str());
                    let title = pane_name(pane_id);
                    let widget = TerminalWidget::new(Arc::clone(&screen), title, focused, palette);
                    if focused {
                        focused_pane_rect = Some(chunks[i]);
                        focused_screen = Some(screen);
                    }
                    frame.render_widget(widget, chunks[i]);
                }
            }
        }
        PaneLayout::Stacked => {
            // Focused pane gets remaining space; unfocused get a single collapsed title row.
            let title_bar_height: u16 = 1;
            let mut constraints: Vec<Constraint> = Vec::new();
            let mut focused_idx: Option<usize> = None;

            for (i, pane_id) in pane_ids.iter().enumerate() {
                let is_focused = focused_id.as_deref() == Some(pane_id.as_str());
                if is_focused {
                    constraints.push(Constraint::Fill(1));
                    focused_idx = Some(i);
                } else {
                    constraints.push(Constraint::Length(title_bar_height));
                }
            }

            // If no pane is focused, expand the first one.
            if focused_idx.is_none() && !pane_ids.is_empty() {
                constraints[0] = Constraint::Fill(1);
                focused_idx = Some(0);
            }

            let chunks = Layout::vertical(constraints).split(area);
            for (i, pane_id) in pane_ids.iter().enumerate() {
                let is_expanded = focused_idx == Some(i);
                let title = pane_name(pane_id);
                if is_expanded {
                    if let Some(screen) = ctrl.get_screen(pane_id) {
                        let is_focused = focused_id.as_deref() == Some(pane_id.as_str());
                        let widget =
                            TerminalWidget::new(Arc::clone(&screen), title, is_focused, palette);
                        if is_focused {
                            focused_pane_rect = Some(chunks[i]);
                            focused_screen = Some(screen);
                        }
                        frame.render_widget(widget, chunks[i]);
                    }
                } else {
                    // Collapsed: show a titled border block.
                    let block = Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(palette.text_secondary))
                        .title(format!(" {title} "));
                    frame.render_widget(block, chunks[i]);
                }
            }
        }
    }

    // Set hardware cursor in the focused pane for real blinking.
    if let Some(rect) = focused_pane_rect
        && let Some(screen_arc) = focused_screen
        && let Ok(parser) = screen_arc.lock()
    {
        let screen = parser.screen();
        if !screen.hide_cursor() && screen.scrollback() == 0 {
            let (crow, ccol) = screen.cursor_position();
            // Inner area: 1-cell border on each side.
            let inner_x = rect.x + 1;
            let inner_y = rect.y + 1;
            let inner_w = rect.width.saturating_sub(2);
            let inner_h = rect.height.saturating_sub(2);
            if ccol < inner_w && crow < inner_h {
                frame.set_cursor_position(Position::new(inner_x + ccol, inner_y + crow));
            }
        }
    }

    // Render selection highlight over the focused pane.
    if let Some(sel) = selection
        && let Some(rect) = focused_pane_rect
    {
        let inner_x = rect.x + 1;
        let inner_y = rect.y + 1;
        let inner_w = rect.width.saturating_sub(2);
        let inner_h = rect.height.saturating_sub(2);
        // Normalize so start <= end.
        let (sr, sc, er, ec) = if (sel.start_row, sel.start_col) <= (sel.end_row, sel.end_col) {
            (sel.start_row, sel.start_col, sel.end_row, sel.end_col)
        } else {
            (sel.end_row, sel.end_col, sel.start_row, sel.start_col)
        };
        let buf = frame.buffer_mut();
        for row in sr..=er.min(inner_h.saturating_sub(1)) {
            let col_start = if row == sr { sc } else { 0 };
            let col_end = if row == er {
                ec
            } else {
                inner_w.saturating_sub(1)
            };
            for col in col_start..=col_end.min(inner_w.saturating_sub(1)) {
                let x = inner_x + col;
                let y = inner_y + row;
                if let Some(cell) = buf.cell_mut((x, y)) {
                    // Invert colors for selection highlight.
                    cell.set_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }
        }
    }

    focused_pane_rect
}

/// Render a mode tab: agent pane on left 50%, side panes stacked on right 50%.
#[allow(clippy::too_many_arguments)]
fn render_mode_tab(
    frame: &mut Frame,
    ui: &mut UiState,
    embedded: Option<&EmbeddedPaneController>,
    agent_pane_id: &str,
    side_pane_ids: &[String],
    main_area: Rect,
    hints_area: Rect,
    has_pane_control: bool,
    active_mode_name: Option<&str>,
    show_tab_bar: bool,
    focused_side_pane_index: Option<usize>,
) {
    let palette = ui.palette;

    // Determine which pane ID should appear visually focused.
    let agent_visual_focus: Option<&str> = if focused_side_pane_index.is_none() {
        Some(agent_pane_id)
    } else {
        None
    };
    let side_visual_focus: Option<String> =
        focused_side_pane_index.and_then(|i| side_pane_ids.get(i).cloned());

    // 50/50 horizontal split
    let chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_area);
    let agent_area = chunks[0];
    let side_area = chunks[1];

    // Track agent pane rect for click-to-focus.
    ui.agent_pane_rect = Some(agent_area);

    // Left side: single agent pane
    if !agent_pane_id.is_empty() {
        let agent_ids = vec![agent_pane_id.to_string()];
        let rect = render_terminal_panes(
            frame,
            embedded,
            agent_area,
            &agent_ids,
            PaneLayout::Stacked,
            &ui.pane_display_names,
            &ui.selection,
            palette,
            agent_visual_focus,
        );
        if rect.is_some() {
            ui.focused_pane_rect = rect;
        }
    }

    // Right side: side panes stacked (all visible simultaneously)
    if !side_pane_ids.is_empty() {
        let rect = render_terminal_panes(
            frame,
            embedded,
            side_area,
            side_pane_ids,
            PaneLayout::Tiled,
            &ui.pane_display_names,
            &ui.selection,
            palette,
            side_visual_focus.as_deref(),
        );
        // Use side pane rect when a side pane is visually focused, or as fallback.
        if side_visual_focus.is_some() || ui.focused_pane_rect.is_none() {
            ui.focused_pane_rect = rect;
        }

        // Track side pane rects for scroll hit-testing
        let count = side_pane_ids.len() as u32;
        let constraints: Vec<Constraint> = side_pane_ids
            .iter()
            .map(|_| Constraint::Ratio(1, count))
            .collect();
        let chunks = Layout::vertical(constraints).split(side_area);
        for (i, pane_id) in side_pane_ids.iter().enumerate() {
            ui.side_pane_rects.push((pane_id.clone(), chunks[i]));
        }
    }

    // Full-width hints bar
    render_bottom_bar(frame, ui, hints_area, has_pane_control, show_tab_bar, true);

    render_overlays(frame, ui, active_mode_name, palette);
}

fn render_stats_bar(
    frame: &mut Frame,
    stats: &DashboardStats,
    area: Rect,
    active_mode_name: Option<&str>,
    palette: ColorPalette,
) {
    let mut spans: Vec<Span> = Vec::new();

    // Always show active count
    spans.push(Span::styled(
        format!(" {} active", stats.active),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    let segments: &[(usize, &str, Color)] = &[
        (stats.working, "working", Color::Green),
        (stats.thinking, "thinking", Color::Blue),
        (stats.compacting, "compacting", Color::Magenta),
        (stats.waiting, "waiting", Color::Yellow),
        (stats.errors, "error", Color::Red),
        (stats.idle, "idle", palette.text_secondary),
    ];

    for &(count, label, color) in segments {
        if count > 0 {
            spans.push(Span::styled(
                "  \u{2502}  ",
                Style::default().fg(palette.text_muted),
            ));
            spans.push(Span::styled(
                format!("{count} {label}"),
                Style::default().fg(color),
            ));
        }
    }

    // Always show total tools
    spans.push(Span::styled(
        "  \u{2502}  ",
        Style::default().fg(palette.text_muted),
    ));
    spans.push(Span::styled(
        format!("{} tools", stats.total_tools),
        Style::default().fg(palette.text_secondary),
    ));

    if let Some(name) = active_mode_name {
        spans.push(Span::styled(
            "  \u{2502}  ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!("mode: {name}"),
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_bottom_bar(
    frame: &mut Frame,
    ui: &UiState,
    area: Rect,
    has_pane_control: bool,
    show_tab_bar: bool,
    is_mode_tab: bool,
) {
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
                let tab_hint = if show_tab_bar {
                    format!("  {MOD_KEY}+PgUp/PgDn: tabs")
                } else {
                    String::new()
                };
                let hints = if is_mode_tab {
                    format!(
                        "j/k: navigate panes  Enter: interact  Esc: agent pane  {MOD_KEY}+w: close tab  ?: help  {MOD_KEY}+c: quit{tab_hint}"
                    )
                } else if has_pane_control {
                    format!(
                        "{MOD_KEY}+n: new  {MOD_KEY}+w: close  {MOD_KEY}+t: layout  {MOD_KEY}+d: dashboard (1-9 ? /)  {MOD_KEY}+c: quit{tab_hint}"
                    )
                } else {
                    format!("?: help  1-9: jump  {MOD_KEY}+c: quit{tab_hint}")
                };
                let mut spans = vec![Span::styled(
                    hints,
                    Style::default().fg(ui.palette.text_secondary),
                )];
                if let Some(ref latest) = ui.update_available {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        format!(
                            " Update available: v{latest} (current: v{}) ",
                            env!("DAD_VERSION")
                        ),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                let line = Line::from(spans);
                frame.render_widget(Paragraph::new(line), area);
            }
        }
    }
}

fn render_quit_confirm(frame: &mut Frame, selected: usize, palette: ColorPalette) {
    let area = frame.area();
    let popup_width = 44.min(area.width.saturating_sub(4));
    let popup_height = 9u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let options = [
        ("Quit", "exit the application"),
        ("Cancel", "return to dashboard"),
    ];

    let mut text = vec![
        Line::from(""),
        Line::styled(
            "  Are you sure you want to quit?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
    ];

    for (i, (label, desc)) in options.iter().enumerate() {
        let cursor = if i == selected { ">" } else { " " };
        let style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        text.push(Line::styled(
            format!("  {cursor} {label:<7} \u{2014} {desc}"),
            style,
        ));
    }

    text.push(Line::from(""));
    text.push(Line::styled(
        "  Up/Down: navigate  Enter: confirm  Esc: cancel",
        Style::default().fg(Color::DarkGray),
    ));

    let block = Block::default()
        .title(" Quit ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(palette.terminal_bg));
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_star_prompt(frame: &mut Frame, palette: ColorPalette) {
    let area = frame.area();
    let popup_width = 50u16.min(area.width.saturating_sub(4));
    let popup_height = 10u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let text = vec![
        Line::from(""),
        Line::styled(
            "  If you find dot-agent-deck useful,",
            Style::default().fg(Color::White),
        ),
        Line::styled(
            "  please consider starring the repo!",
            Style::default().fg(Color::White),
        ),
        Line::from(""),
        Line::styled(
            "  github.com/vfarcic/dot-agent-deck",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        ),
        Line::from(""),
        Line::styled(
            "  s Star  l Later  d Don't ask again",
            Style::default().fg(Color::Gray),
        ),
    ];

    let block = Block::default()
        .title(" ⭐ Enjoying dot-agent-deck? ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(palette.terminal_bg));

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_config_gen_prompt(frame: &mut Frame, selected: usize, palette: ColorPalette) {
    let area = frame.area();
    let popup_width = 60u16.min(area.width.saturating_sub(4));
    let popup_height = 14u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let options = [
        ("Yes", "generate config for this project"),
        ("No", "skip for now"),
        ("Never", "never ask for this directory"),
    ];

    let mut text = vec![
        Line::from(""),
        Line::styled(
            "  No workspace modes config found for this",
            Style::default().fg(Color::White),
        ),
        Line::styled(
            "  project. Want to instruct your agent to",
            Style::default().fg(Color::White),
        ),
        Line::styled(
            "  analyze the project and create one?",
            Style::default().fg(Color::White),
        ),
        Line::from(""),
    ];

    for (i, (label, desc)) in options.iter().enumerate() {
        let cursor = if i == selected { ">" } else { " " };
        let style = if i == selected {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        text.push(Line::styled(
            format!("  {cursor} {label:<6} \u{2014} {desc}"),
            style,
        ));
    }

    text.push(Line::from(""));
    text.push(Line::styled(
        "  Disable: dot-agent-deck config set auto_config_prompt false",
        Style::default().fg(Color::DarkGray),
    ));
    text.push(Line::from(""));
    text.push(Line::styled(
        "  Up/Down: navigate  Enter: confirm  Esc: cancel",
        Style::default().fg(Color::DarkGray),
    ));

    let block = Block::default()
        .title(" Generate .dot-agent-deck.toml ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(palette.terminal_bg));

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_help_overlay(frame: &mut Frame, active_mode_name: Option<&str>, palette: ColorPalette) {
    let cyan = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let left: Vec<Line> = vec![
        Line::styled("  Global (works from any pane)", cyan),
        Line::from(""),
        Line::from(format!("  {MOD_KEY}+d           Command mode (dashboard)")),
        Line::from(format!("  {MOD_KEY}+n           Create new pane")),
        Line::from(format!("  {MOD_KEY}+w           Close selected pane")),
        Line::from(format!(
            "  {MOD_KEY}+t           Toggle layout (stacked/tiled)"
        )),
        Line::from(format!("  {MOD_KEY}+c           Quit")),
        Line::from(""),
        Line::styled("  Tab Navigation", cyan),
        Line::from(""),
        Line::from("  Tab / Right / l       Next tab"),
        Line::from("  Shift+Tab / Left / h  Prev tab"),
        Line::from(format!("  {MOD_KEY}+PgDn            Next tab")),
        Line::from(format!("  {MOD_KEY}+PgUp            Prev tab")),
        Line::from(""),
        Line::styled("  Dashboard (command mode)", cyan),
        Line::from(""),
        Line::from("  1-9             Jump to pane N"),
        Line::from("  j/k / Up/Down   Navigate cards"),
        Line::from("  Enter           Focus selected pane"),
        Line::from("  /               Filter sessions"),
        Line::from("  Esc             Clear filter"),
        Line::from("  r               Rename session"),
        Line::from("  g               Generate .dot-agent-deck.toml"),
        Line::from("  y / n           Approve / deny permission"),
        Line::from("  ?               Toggle this help"),
    ];

    let mut right: Vec<Line> = vec![
        Line::styled("  Mode Tab (in-tab navigation)", cyan),
        Line::from(""),
        Line::from("  j / Down        Focus next pane"),
        Line::from("  k / Up          Focus previous pane"),
        Line::from("  Enter           Enter PaneInput on selected"),
        Line::from("  Esc             Deselect side pane"),
        Line::from("  Mouse click     Focus pane"),
        Line::from(format!("  {MOD_KEY}+d            Return to Normal mode")),
        Line::from(""),
        Line::styled("  New Agent Form", cyan),
        Line::from(""),
        Line::from("  Tab             Switch field"),
        Line::from("  \u{25c0}/\u{25b6}             Cycle mode"),
        Line::from("  Enter           Next field / confirm"),
        Line::from("  Esc             Cancel"),
        Line::from(""),
        Line::styled("  Directory Picker", cyan),
        Line::from(""),
        Line::from("  j / Down        Select next"),
        Line::from("  k / Up          Select previous"),
        Line::from("  l / Right / Enter   Enter directory"),
        Line::from("  h / Left / Bksp     Go up one level"),
        Line::from("  Space           Confirm current directory"),
        Line::from("  /               Filter (case-insensitive)"),
        Line::from("  Esc             Clear filter (twice = close)"),
        Line::from("  q               Cancel"),
        Line::from(""),
        Line::styled("  Session", cyan),
        Line::from(""),
        Line::from("  Panes auto-saved on exit."),
        Line::from("  Restore: dot-agent-deck --continue"),
    ];

    if let Some(name) = active_mode_name {
        right.push(Line::from(""));
        right.push(Line::styled(format!("  Active mode: {name}"), cyan));
    }

    let area = frame.area();
    let column_width: u16 = 50;
    let popup_width = (column_width * 2 + 3).min(area.width.saturating_sub(2));
    let content_height = left.len().max(right.len()) as u16;
    let popup_height = (content_height + 4).min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(palette.terminal_bg));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Reserve last 2 rows for footer
    let footer_height: u16 = 2;
    let columns_height = inner.height.saturating_sub(footer_height);
    let columns_area = Rect::new(inner.x, inner.y, inner.width, columns_height);
    let footer_area = Rect::new(
        inner.x,
        inner.y + columns_height,
        inner.width,
        footer_height.min(inner.height),
    );

    let halves = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(columns_area);
    frame.render_widget(Paragraph::new(left), halves[0]);
    frame.render_widget(Paragraph::new(right), halves[1]);

    let footer = Paragraph::new(vec![
        Line::from(""),
        Line::styled(
            "  Press ? or Esc to close",
            Style::default().fg(palette.text_secondary),
        ),
    ]);
    frame.render_widget(footer, footer_area);
}

fn render_dir_picker(frame: &mut Frame, picker: &mut DirPickerState, palette: ColorPalette) {
    let area = frame.area();
    let popup_width = 60.min(area.width.saturating_sub(4));
    let popup_height = 20u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    // Reserve lines so controls remain visible regardless of list length.
    let show_filter_row = picker.filtering || !picker.filter_text.is_empty();
    let mut reserved_lines = 5; // current dir + blank + blank + two footer lines
    if show_filter_row {
        reserved_lines += 1;
    }
    let inner_height = popup_area.height.saturating_sub(2) as usize;
    let max_visible = inner_height.saturating_sub(reserved_lines);
    let visible_rows = max_visible.max(1);
    picker.ensure_visible(visible_rows);

    let mut lines = vec![Line::styled(
        format!("  {}", picker.current_dir.display()),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )];

    if show_filter_row {
        let mut spans = vec![Span::styled(
            "  / ",
            Style::default().fg(palette.text_secondary),
        )];
        spans.push(Span::styled(
            picker.filter_text.clone(),
            Style::default().fg(palette.text_primary),
        ));
        if picker.filtering {
            spans.push(Span::styled("█", Style::default().fg(Color::Cyan)));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));

    if picker.filtered_indices.is_empty() {
        let message = if picker.entries.is_empty() {
            "  (no subdirectories)"
        } else {
            "  (no matching directories)"
        };
        lines.push(Line::styled(
            message,
            Style::default().fg(palette.text_muted),
        ));
    } else {
        for (i, entry_idx) in picker
            .filtered_indices
            .iter()
            .enumerate()
            .skip(picker.scroll_offset)
            .take(visible_rows)
        {
            let entry = &picker.entries[*entry_idx];
            let is_parent = entry == Path::new("..");
            let name = if is_parent {
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
            let suffix = if is_parent { "" } else { "/" };
            lines.push(Line::styled(format!("{prefix}{name}{suffix}"), style));
        }
    }

    lines.push(Line::from(""));
    let nav_footer = if picker.filtering {
        "  ↑↓: move between matches  Backspace: delete  q: cancel"
    } else {
        "  j/k or ↑↓: navigate  l/Enter: open  Space: select  h/Backspace: up"
    };
    lines.push(Line::styled(
        nav_footer,
        Style::default().fg(palette.text_secondary),
    ));
    let mode_footer = if picker.filtering {
        "  Typing: add characters  Enter: accept filter  Esc: clear"
    } else if !picker.filter_text.is_empty() {
        "  /: edit filter  Esc: clear filter  q: cancel"
    } else {
        "  /: filter directories  Esc or q: cancel"
    };
    lines.push(Line::styled(
        mode_footer,
        Style::default().fg(palette.text_secondary),
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Select Directory ")
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(palette.terminal_bg));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_new_pane_form(frame: &mut Frame, form: &NewPaneFormState, palette: ColorPalette) {
    let area = frame.area();
    let popup_width = 56.min(area.width.saturating_sub(4));
    // The mode field (when modes exist) or the tip line (when they don't)
    // each need 2 extra rows.  Always reserve them.
    let mode_extra: u16 = 2;
    let popup_height = (12 + mode_extra).min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let inner_width = popup_width.saturating_sub(4) as usize;

    let focused_label = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let unfocused_label = Style::default().fg(palette.text_secondary);

    let mode_style = if form.focused == FormField::Mode {
        focused_label
    } else {
        unfocused_label
    };
    let name_style = if form.focused == FormField::Name {
        focused_label
    } else {
        unfocused_label
    };
    let cmd_style = if form.focused == FormField::Command {
        focused_label
    } else {
        unfocused_label
    };

    let dir_display = form.dir.display().to_string();
    let mut lines = vec![
        Line::styled(
            format!("  Dir: {dir_display}"),
            Style::default().fg(Color::Yellow),
        ),
        Line::from(""),
    ];

    // Mode field (only when modes are available)
    if !form.has_mode_field {
        // No .dot-agent-deck.toml or no modes — show a contextual hint.
        lines.push(Line::styled(
            "  Tip: press g on dashboard to create modes",
            Style::default()
                .fg(palette.hint_accent)
                .add_modifier(Modifier::ITALIC),
        ));
        lines.push(Line::from(""));
    }
    if form.has_mode_field {
        let mode_name = form.mode_display_name();
        let mode_value = if form.focused == FormField::Mode {
            format!("\u{25c0} {mode_name} \u{25b6}")
        } else {
            mode_name.to_string()
        };
        let mode_value_style = if form.focused == FormField::Mode {
            Style::default().fg(palette.text_primary)
        } else {
            unfocused_label
        };
        lines.push(Line::from(vec![
            Span::styled("  Mode:    ", mode_style),
            Span::styled(mode_value, mode_value_style),
        ]));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled("  Name:    ", name_style),
        Span::styled(
            format!(
                "{:<width$}",
                form.name,
                width = inner_width.saturating_sub(11)
            ),
            if form.focused == FormField::Name {
                Style::default().fg(palette.text_primary)
            } else {
                unfocused_label
            },
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Command: ", cmd_style),
        Span::styled(
            format!(
                "{:<width$}",
                form.command,
                width = inner_width.saturating_sub(11)
            ),
            if form.focused == FormField::Command {
                Style::default().fg(palette.text_primary)
            } else {
                unfocused_label
            },
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    let footer = if form.has_mode_field {
        "  Tab: switch  \u{25c0}\u{25b6}: mode  Enter: next  Esc: cancel"
    } else {
        "  Tab: switch field  Enter: next/confirm  Esc: cancel"
    };
    lines.push(Line::styled(
        footer,
        Style::default().fg(palette.text_secondary),
    ));

    let title = match form.selected_mode() {
        Some(cfg) => format!(" New Agent \u{2014} {} mode ", cfg.name),
        None => " New Agent ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(palette.terminal_bg));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);

    // Show cursor in the active text field (not for Mode which uses arrow cycling)
    if form.focused != FormField::Mode {
        let cursor_y = match form.focused {
            FormField::Name => popup_area.y + 3 + mode_extra,
            FormField::Command => popup_area.y + 5 + mode_extra,
            FormField::Mode => unreachable!(),
        };
        let field_text = match form.focused {
            FormField::Name => &form.name,
            FormField::Command => &form.command,
            FormField::Mode => unreachable!(),
        };
        let cursor_x = popup_area.x + 12 + field_text.len() as u16;
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
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

#[allow(clippy::too_many_arguments)]
fn render_session_card(
    frame: &mut Frame,
    area: Rect,
    session: &SessionState,
    tick: u64,
    is_selected: bool,
    display_name: Option<&String>,
    card_number: Option<u8>,
    density: CardDensity,
    palette: ColorPalette,
    idle_art: Option<&IdleArtEntry>,
) {
    let is_placeholder = session.agent_type == crate::event::AgentType::None;
    let (status_label, status_style) = if is_placeholder {
        ("No agent", Style::default().fg(Color::DarkGray))
    } else {
        status_style(&session.status)
    };
    let status_color = status_style.fg.unwrap_or(palette.text_secondary);

    let id_display = if session.session_id.len() > 11 {
        &session.session_id[..11]
    } else {
        &session.session_id
    };

    let num_prefix = match card_number {
        Some(n) => format!("{n} "),
        None => String::new(),
    };
    let sel_prefix = if is_selected { "▸ " } else { "" };
    let mut title_left = if let Some(name) = display_name {
        format!(" {sel_prefix}{num_prefix}{} ", name)
    } else {
        format!(
            " {sel_prefix}{num_prefix}{} · {} ",
            session.agent_type, id_display
        )
    };

    let dot = flash_dot(&session.status, tick);
    let status_text = format!(" {} {} ", dot, status_label);
    // area.width includes left+right borders (2 chars)
    let max_title = (area.width as usize).saturating_sub(status_text.chars().count() + 2);
    title_left = truncate_with_ellipsis(&title_left, max_title);

    let border_style = if is_selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(status_color)
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            title_left,
            Style::default()
                .fg(palette.text_primary)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Left)
        .title(
            Line::from(Span::styled(status_text, status_style))
                .alignment(ratatui::layout::Alignment::Right),
        );

    if is_selected {
        block = block.style(Style::default().bg(palette.selected_bg));
    }

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
        let right_spans = vec![
            Span::styled("Last: ", Style::default().fg(palette.text_secondary)),
            Span::raw(format!("{}  ", elapsed)),
            Span::styled("Tools: ", Style::default().fg(palette.text_secondary)),
            Span::raw(session.tool_count.to_string()),
        ];
        let right_len: usize = right_spans.iter().map(|s| s.width()).sum();
        let dir_label_len = 6; // "Dir:  "
        let max_dir = w.saturating_sub(right_len + dir_label_len + 1);

        let dir_display = truncate_with_ellipsis(cwd_display.as_ref(), max_dir);

        lines.push(padded_line(
            vec![
                Span::styled("Dir:  ", Style::default().fg(palette.text_secondary)),
                Span::raw(dir_display),
            ],
            right_spans,
            w,
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Dir:  ", Style::default().fg(palette.text_secondary)),
            Span::raw(cwd_display),
        ]));
    }

    if is_placeholder {
        lines.push(Line::from(Span::styled(
            "Launch an agent to get started",
            Style::default().fg(palette.text_muted),
        )));
    } else {
        let prompts = collect_recent_prompts(session, density.max_prompts());
        for (i, prompt) in prompts.iter().enumerate() {
            let prefix = if i == 0 { "Prmt: " } else { "      " };
            let max_prompt = w.saturating_sub(6);
            let display = truncate_with_ellipsis(prompt, max_prompt);
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(palette.text_secondary)),
                Span::raw(display),
            ]));
        }
    }

    if !wide {
        lines.push(Line::from(vec![
            Span::styled("Last: ", Style::default().fg(palette.text_secondary)),
            Span::raw(format!("{}  ", elapsed)),
            Span::styled("Tools: ", Style::default().fg(palette.text_secondary)),
            Span::raw(session.tool_count.to_string()),
        ]));
    }

    if density != CardDensity::Compact {
        lines.push(Line::from(""));
    }
    let tool_lines = recent_tool_lines(session, density.max_tools(), palette);
    lines.extend(tool_lines);

    let content = Paragraph::new(lines);
    frame.render_widget(content, inner);

    // Overlay ASCII art on top of the normal content (unless dismissed by user)
    if let Some(entry) = idle_art
        && !entry.dismissed
        && let IdleArtPhase::HasArt(ref art) = entry.phase
        && !art.frames.is_empty()
    {
        // Clear the inner area so the Dir line and other content don't bleed through
        frame.render_widget(Clear, inner);

        let frame_index = (tick / 120) as usize % art.frames.len();
        let art_lines: Vec<Line<'_>> = art.frames[frame_index]
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(palette.text_primary),
                ))
            })
            .collect();
        let art_widget = Paragraph::new(art_lines);
        frame.render_widget(art_widget, inner);
    }
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
    let needs_attention =
        *status == SessionStatus::WaitingForInput || *status == SessionStatus::Idle;
    if needs_attention && (tick / 30) % 2 == 1 {
        " "
    } else {
        "●"
    }
}

fn collect_recent_prompts(session: &SessionState, max: usize) -> Vec<String> {
    let mut prompts: Vec<String> = session
        .recent_events
        .iter()
        .rev()
        .filter_map(|e| e.user_prompt.as_ref())
        .take(max)
        .cloned()
        .collect();
    prompts.reverse();

    if prompts.is_empty()
        && let Some(ref p) = session.last_user_prompt
    {
        prompts.push(p.clone());
    }
    prompts
}

fn recent_tool_lines(
    session: &SessionState,
    max_tools: usize,
    palette: ColorPalette,
) -> Vec<Line<'static>> {
    let tool_events: Vec<_> = session
        .recent_events
        .iter()
        .rev()
        .filter(|e| e.event_type == EventType::ToolStart)
        .take(max_tools)
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
            Line::styled(text, Style::default().fg(palette.text_muted))
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

// ---------------------------------------------------------------------------
// Idle ASCII art – background generation & per-tick state machine
// ---------------------------------------------------------------------------

/// Build the "input" string sent to the LLM from session prompts.
fn build_art_input(session: &SessionState) -> String {
    let mut parts: Vec<&str> = session.first_prompts.iter().map(|s| s.as_str()).collect();
    if let Some(ref last) = session.last_user_prompt
        && !session.first_prompts.iter().any(|p| p == last)
    {
        parts.push(last);
    }
    parts.join(" | ")
}

/// Build the "output" string sent to the LLM from recent tool activity.
fn build_art_output(session: &SessionState) -> String {
    let tool_summaries: Vec<String> = session
        .recent_events
        .iter()
        .rev()
        .filter_map(|e| {
            e.tool_name.as_deref().map(|name| {
                if let Some(ref detail) = e.tool_detail {
                    format!("{name}({detail})")
                } else {
                    name.to_string()
                }
            })
        })
        .take(5)
        .collect();
    if tool_summaries.is_empty() {
        "Session idle".to_string()
    } else {
        format!("Used tools: {}", tool_summaries.join(", "))
    }
}

/// Drive the per-session idle art state machine. Called once per tick.
fn update_idle_art(
    idle_art_cache: &mut HashMap<String, IdleArtEntry>,
    config: &IdleArtConfig,
    sessions: &HashMap<String, SessionState>,
    density: CardDensity,
) {
    // Gate: feature disabled or not spacious
    if !config.enabled || density != CardDensity::Spacious {
        idle_art_cache.clear();
        tracing::debug!(
            "idle_art skipped: enabled={}, density={:?}",
            config.enabled,
            density
        );
        return;
    }

    let now = Utc::now();
    let timeout = chrono::Duration::seconds(config.timeout_secs as i64);

    // Remove entries for sessions that no longer exist
    idle_art_cache.retain(|sid, _| sessions.contains_key(sid));

    // Collect session IDs to process (avoid borrowing conflicts)
    let session_ids: Vec<String> = sessions.keys().cloned().collect();

    for sid in &session_ids {
        let session = &sessions[sid];

        if session.status != SessionStatus::Idle {
            idle_art_cache.remove(sid);
            continue;
        }

        let idle_duration = now.signed_duration_since(session.last_activity);
        tracing::debug!(
            "idle_art {sid}: status=Idle, idle_for={}s, timeout={}s",
            idle_duration.num_seconds(),
            config.timeout_secs
        );

        // Session is idle — manage the state machine
        let entry = idle_art_cache.entry(sid.clone()).or_insert(IdleArtEntry {
            phase: IdleArtPhase::Waiting,
            idle_since: session.last_activity,
            dismissed: false,
        });

        // Reset if this is a new idle stretch
        if entry.idle_since != session.last_activity {
            entry.phase = IdleArtPhase::Waiting;
            entry.idle_since = session.last_activity;
            entry.dismissed = false;
        }

        // Retry failed generations after 60s cooldown
        if let IdleArtPhase::Failed(at) = entry.phase
            && at.elapsed() >= std::time::Duration::from_secs(60)
        {
            tracing::debug!("idle_art {sid}: retrying after failure cooldown");
            entry.phase = IdleArtPhase::Waiting;
        }

        // Spawn generation if timeout elapsed
        if matches!(entry.phase, IdleArtPhase::Waiting)
            && now.signed_duration_since(entry.idle_since) >= timeout
        {
            let (tx, rx) = std::sync::mpsc::channel();
            let input = build_art_input(session);
            let output = build_art_output(session);
            let art_config = config.clone();

            tracing::info!(
                "idle_art {sid}: spawning generation (input_len={}, output_len={})",
                input.len(),
                output.len()
            );

            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let result = generate_ascii_art(&input, &output, &art_config).await;
                    match &result {
                        Ok(_) => tracing::info!("idle_art generation succeeded"),
                        Err(e) => tracing::warn!("idle_art generation failed: {e}"),
                    }
                    let _ = tx.send(result.ok());
                });
                entry.phase = IdleArtPhase::Generating(rx);
            } else {
                tracing::warn!("idle_art {sid}: no tokio runtime handle available");
                entry.phase = IdleArtPhase::Failed(std::time::Instant::now());
            }
        }

        // Poll for completion
        let failed_now = IdleArtPhase::Failed(std::time::Instant::now());
        let phase = std::mem::replace(&mut entry.phase, failed_now);
        if let IdleArtPhase::Generating(rx) = phase {
            match rx.try_recv() {
                Ok(Some(art)) => entry.phase = IdleArtPhase::HasArt(art),
                Ok(None) => entry.phase = IdleArtPhase::Failed(std::time::Instant::now()),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    entry.phase = IdleArtPhase::Generating(rx);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    entry.phase = IdleArtPhase::Failed(std::time::Instant::now());
                }
            }
        } else {
            // Put back non-Generating phases
            entry.phase = phase;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentType, EventType};
    use crate::project_config::OrchestrationRoleConfig;
    use chrono::{Duration, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;
    use tempfile::tempdir;

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
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::new();
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &ActiveTabView::Dashboard {
                        exclude_pane_ids: vec![],
                    },
                    &TabBarInfo {
                        show: false,
                        labels: vec!["Dashboard".into()],
                        active_index: 0,
                    },
                )
            })
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
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::new();
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &ActiveTabView::Dashboard {
                        exclude_pane_ids: vec![],
                    },
                    &TabBarInfo {
                        show: false,
                        labels: vec!["Dashboard".into()],
                        active_index: 0,
                    },
                )
            })
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
            first_prompts: Vec::new(),
            pane_id: None,
        };

        let palette = ColorPalette::dark();
        let lines = recent_tool_lines(&session, 3, palette);
        assert_eq!(lines.len(), 3);
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text[0], "  Write — out.txt");
        assert_eq!(text[1], "  Bash");
        assert_eq!(text[2], "  Grep — pattern");

        // Compact mode: only 1 tool (most recent)
        let lines_compact = recent_tool_lines(&session, 1, palette);
        assert_eq!(lines_compact.len(), 1);
        assert_eq!(lines_compact[0].to_string(), "  Grep — pattern");
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
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::new();
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &ActiveTabView::Dashboard {
                        exclude_pane_ids: vec![],
                    },
                    &TabBarInfo {
                        show: false,
                        labels: vec!["Dashboard".into()],
                        active_index: 0,
                    },
                )
            })
            .unwrap();
    }

    #[test]
    fn orchestrator_context_includes_agents_and_protocol() {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};

        let config = OrchestrationConfig {
            name: "code-review".to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "orchestrator".to_string(),
                    command: "claude".to_string(),
                    start: true,
                    description: None,
                    prompt_template: Some("You coordinate the team.".to_string()),
                    clear: true,
                },
                OrchestrationRoleConfig {
                    name: "coder".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: Some("Implements code changes".to_string()),
                    prompt_template: None,
                    clear: true,
                },
                OrchestrationRoleConfig {
                    name: "reviewer".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: true,
                },
            ],
        };

        let content = build_orchestrator_context(&config);

        // Contains the orchestrator's own template.
        assert!(content.contains("You coordinate the team."));
        // Lists worker agents with descriptions.
        assert!(content.contains("**coder**: Implements code changes"));
        assert!(content.contains("**reviewer**: (no description)"));
        // Does NOT list the orchestrator itself.
        assert!(!content.contains("**orchestrator**"));
        // Contains delegation protocol.
        assert!(content.contains("Delegation protocol"));
        assert!(content.contains("dot-agent-deck work-done"));
        // Instructs orchestrator to wait.
        assert!(content.contains("wait for the user to provide instructions"));
    }

    #[test]
    fn orchestrator_context_no_template() {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};

        let config = OrchestrationConfig {
            name: "test".to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "lead".to_string(),
                    command: "claude".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: true,
                },
                OrchestrationRoleConfig {
                    name: "worker".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: Some("Does work".to_string()),
                    prompt_template: None,
                    clear: true,
                },
            ],
        };

        let content = build_orchestrator_context(&config);
        // Starts directly with available agents (no template preamble).
        assert!(content.starts_with("## Available agents"));
        assert!(content.contains("**worker**: Does work"));
    }

    #[test]
    fn prepare_orchestrator_prompt_writes_file() {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};

        let dir = tempfile::tempdir().unwrap();
        let config = OrchestrationConfig {
            name: "test".to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "lead".to_string(),
                    command: "claude".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: true,
                },
                OrchestrationRoleConfig {
                    name: "worker".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: Some("Does work".to_string()),
                    prompt_template: None,
                    clear: true,
                },
            ],
        };

        let cwd = dir.path().to_str().unwrap();
        let prompt = prepare_orchestrator_prompt(&config, cwd);
        assert!(prompt.is_some());
        let prompt = prompt.unwrap();
        // One-liner referencing the file.
        assert!(prompt.contains("orchestrator-context.md"));
        assert!(!prompt.contains('\n'));
        // File was written.
        let file_path = dir.path().join(".dot-agent-deck/orchestrator-context.md");
        assert!(file_path.exists());
        let content = std::fs::read_to_string(file_path).unwrap();
        assert!(content.contains("Available agents"));
        assert!(content.contains("**worker**: Does work"));
    }

    #[test]
    fn test_flash_dot() {
        // WaitingForInput: visible in first half (ticks 0–29), hidden in second half (30–59)
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 0),
            "●"
        );
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 29),
            "●"
        );
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 30),
            " "
        );
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 59),
            " "
        );
        assert_eq!(
            flash_dot(&crate::state::SessionStatus::WaitingForInput, 60),
            "●"
        );
        // Idle also blinks
        assert_eq!(flash_dot(&crate::state::SessionStatus::Idle, 0), "●");
        assert_eq!(flash_dot(&crate::state::SessionStatus::Idle, 30), " ");
        // Working never blinks
        assert_eq!(flash_dot(&crate::state::SessionStatus::Working, 0), "●");
        assert_eq!(flash_dot(&crate::state::SessionStatus::Working, 30), "●");
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
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::new();
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &ActiveTabView::Dashboard {
                        exclude_pane_ids: vec![],
                    },
                    &TabBarInfo {
                        show: false,
                        labels: vec!["Dashboard".into()],
                        active_index: 0,
                    },
                )
            })
            .unwrap();
    }

    // ---------------------------------------------------------------------------
    // Navigation tests
    // ---------------------------------------------------------------------------

    // ---------------------------------------------------------------------------
    // Dir picker tests
    // ---------------------------------------------------------------------------

    fn make_dir_picker(entries: &[&str]) -> DirPickerState {
        let mut picker = DirPickerState {
            current_dir: PathBuf::from("/tmp"),
            entries: entries.iter().copied().map(PathBuf::from).collect(),
            selected: 0,
            scroll_offset: 0,
            filter_text: String::new(),
            filtering: false,
            filtered_indices: Vec::new(),
        };
        picker.refilter();
        picker
    }

    #[test]
    fn dir_picker_refilter_matches_case_insensitive() {
        let mut picker = make_dir_picker(&["..", "/tmp/Alpha", "/tmp/beta"]);
        picker.filter_text = "ALP".to_string();
        picker.refilter();
        assert_eq!(picker.filtered_indices.len(), 2);
        assert_eq!(picker.filtered_indices[0], 0); // parent entry
        assert_eq!(picker.filtered_indices[1], 1); // Alpha matches regardless of case
    }

    #[test]
    fn dir_picker_parent_entry_always_present() {
        let mut picker = make_dir_picker(&["..", "/tmp/app", "/tmp/docs"]);
        picker.filter_text = "zzz".to_string();
        picker.refilter();
        assert_eq!(picker.filtered_indices.len(), 1);
        let idx = picker.filtered_indices[0];
        assert_eq!(picker.entries[idx], PathBuf::from(".."));
    }

    #[test]
    fn dir_picker_selection_wraps() {
        let mut picker = make_dir_picker(&["..", "/tmp/a", "/tmp/b"]);
        let total = picker.filtered_indices.len();
        assert_eq!(total, 3);
        picker.select_previous();
        assert_eq!(picker.selected, total - 1);
        picker.select_next();
        assert_eq!(picker.selected, 0);
        picker.select_next();
        assert_eq!(picker.selected, 1);
        picker.selected = 0;
        picker.select_previous();
        assert_eq!(picker.selected, total - 1);
    }

    #[test]
    fn dir_picker_filter_typing_narrows_entries() {
        let mut ui = default_ui();
        ui.mode = UiMode::DirPicker;
        ui.dir_picker = Some(make_dir_picker(&[
            "..",
            "/tmp/alpha",
            "/tmp/beta",
            "/tmp/Bravo",
        ]));

        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &mut ui,
        );

        let picker = ui.dir_picker.as_ref().unwrap();
        assert!(picker.filtering);
        assert_eq!(picker.filter_text, "b");
        let filtered: Vec<String> = picker
            .filtered_indices
            .iter()
            .map(|&idx| {
                let entry = &picker.entries[idx];
                entry
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| entry.to_string_lossy().to_string())
            })
            .collect();
        assert_eq!(filtered, vec!["..", "beta", "Bravo"]);
    }

    #[test]
    fn dir_picker_backspace_clears_filter_when_empty() {
        let mut ui = default_ui();
        ui.mode = UiMode::DirPicker;
        ui.dir_picker = Some(make_dir_picker(&["..", "/tmp/alpha", "/tmp/beta"]));

        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut ui,
        );

        let picker = ui.dir_picker.as_ref().unwrap();
        assert!(!picker.filtering);
        assert!(picker.filter_text.is_empty());
        assert_eq!(picker.filtered_indices.len(), picker.entries.len());
    }

    #[test]
    fn dir_picker_filter_esc_clears_text() {
        let mut ui = default_ui();
        ui.mode = UiMode::DirPicker;
        ui.dir_picker = Some(make_dir_picker(&["..", "/tmp/app", "/tmp/docs"]));

        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);

        let picker = ui.dir_picker.as_ref().unwrap();
        assert!(picker.filter_text.is_empty());
        assert!(!picker.filtering);
        assert_eq!(picker.filtered_indices.len(), picker.entries.len());
    }

    #[test]
    fn dir_picker_esc_clears_then_closes_picker() {
        let mut ui = default_ui();
        ui.mode = UiMode::DirPicker;
        ui.dir_picker = Some(make_dir_picker(&["..", "/tmp/foo", "/tmp/bar"]));

        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut ui);

        handle_dir_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);
        {
            let picker = ui.dir_picker.as_ref().unwrap();
            assert!(picker.filter_text.is_empty());
            assert!(!picker.filtering);
            assert_eq!(ui.mode, UiMode::DirPicker);
        }

        handle_dir_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);
        assert!(ui.dir_picker.is_none());
        assert_eq!(ui.mode, UiMode::Normal);
    }

    #[test]
    fn dir_picker_refresh_resets_filter_on_navigation() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let child = root.join("alpha");
        let grandchild = child.join("beta");
        std::fs::create_dir_all(&grandchild).unwrap();

        let mut picker = DirPickerState::new(root.clone());
        assert!(picker.entries.iter().any(|entry| entry == &child));

        picker.filter_text = "alpha".to_string();
        picker.refilter();
        let child_pos = picker
            .filtered_indices
            .iter()
            .position(|&idx| picker.entries[idx] == child)
            .expect("alpha entry present");
        picker.selected = child_pos;
        picker.enter_selected();

        assert_eq!(picker.current_dir, child);
        assert!(picker.filter_text.is_empty());
        assert!(!picker.filtering);

        picker.filter_text = "beta".to_string();
        picker.filtering = true;
        picker.refilter();
        assert!(
            picker
                .filtered_indices
                .iter()
                .any(|&idx| picker.entries[idx] == grandchild)
        );

        picker.go_up();

        assert_eq!(picker.current_dir, root);
        assert!(picker.filter_text.is_empty());
        assert!(!picker.filtering);
    }

    #[test]
    fn dir_picker_enter_confirms_when_no_subdirs() {
        let mut ui = default_ui();
        ui.mode = UiMode::DirPicker;
        ui.dir_picker = Some(make_dir_picker(&[".."]));

        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            &mut ui,
        );

        // Goes directly to unified NewPaneForm
        assert_eq!(ui.mode, UiMode::NewPaneForm);
        assert!(ui.new_pane_form.is_some());
        let form = ui.new_pane_form.as_ref().unwrap();
        assert!(form.modes.is_empty());
        assert!(!form.has_mode_field);
        assert_eq!(form.focused, FormField::Name);
    }

    #[test]
    fn dir_picker_filter_mode_q_cancels_picker() {
        let mut ui = default_ui();
        ui.mode = UiMode::DirPicker;
        ui.dir_picker = Some(make_dir_picker(&["..", "/tmp/a"]));

        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut ui,
        );
        handle_dir_picker_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut ui,
        );

        assert!(ui.dir_picker.is_none());
        assert_eq!(ui.mode, UiMode::Normal);
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
            first_prompts: Vec::new(),
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

    // ---------------------------------------------------------------------------
    // Card density tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_choose_density_wide() {
        // Wide layout (no extra stats line)
        // Spacious=11, Normal=9, Compact=7

        // 1 session, 1 col, plenty of height -> Spacious
        assert_eq!(choose_density(1, 1, 20, true), CardDensity::Spacious);

        // 2 sessions, 2 cols = 1 row, height 11 -> Spacious (1*11=11)
        assert_eq!(choose_density(2, 2, 11, true), CardDensity::Spacious);

        // 2 sessions, 2 cols = 1 row, height 10 -> Normal (1*9=9 fits)
        assert_eq!(choose_density(2, 2, 10, true), CardDensity::Normal);

        // 4 sessions, 2 cols = 2 rows, height 18 -> Normal (2*9=18)
        assert_eq!(choose_density(4, 2, 18, true), CardDensity::Normal);

        // 4 sessions, 2 cols = 2 rows, height 17 -> Compact (2*7=14 fits)
        assert_eq!(choose_density(4, 2, 17, true), CardDensity::Compact);

        // Many sessions, small screen -> Compact
        assert_eq!(choose_density(10, 1, 20, true), CardDensity::Compact);

        // Edge: 0 sessions -> Spacious (0 rows needed)
        assert_eq!(choose_density(0, 1, 10, true), CardDensity::Spacious);
    }

    #[test]
    fn test_choose_density_narrow() {
        // Narrow layout: each mode needs 1 extra row for stats line
        // Spacious=12, Normal=10, Compact=8

        // 1 session, height 12 -> Spacious (1*12=12)
        assert_eq!(choose_density(1, 1, 12, false), CardDensity::Spacious);

        // 1 session, height 11 -> Normal (1*10=10 fits)
        assert_eq!(choose_density(1, 1, 11, false), CardDensity::Normal);

        // 2 sessions, 1 col, height 20 -> Normal (2*10=20)
        assert_eq!(choose_density(2, 1, 20, false), CardDensity::Normal);

        // 2 sessions, 1 col, height 19 -> Compact (2*8=16 fits)
        assert_eq!(choose_density(2, 1, 19, false), CardDensity::Compact);
    }

    #[test]
    fn test_collect_recent_prompts_from_events() {
        use std::collections::VecDeque;

        let mut events = VecDeque::new();
        for prompt in ["first prompt", "second prompt", "third prompt"] {
            events.push_back(AgentEvent {
                session_id: "s1".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: EventType::Thinking,
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: Utc::now(),
                user_prompt: Some(prompt.to_string()),
                metadata: HashMap::new(),
                pane_id: None,
            });
        }

        let session = SessionState {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            cwd: None,
            status: SessionStatus::Idle,
            active_tool: None,
            started_at: Utc::now(),
            last_activity: Utc::now(),
            recent_events: events,
            tool_count: 0,
            last_user_prompt: Some("third prompt".to_string()),
            first_prompts: Vec::new(),
            pane_id: None,
        };

        // Spacious: get all 3
        let prompts = collect_recent_prompts(&session, 3);
        assert_eq!(
            prompts,
            vec!["first prompt", "second prompt", "third prompt"]
        );

        // Normal/Compact: get only the most recent
        let prompts = collect_recent_prompts(&session, 1);
        assert_eq!(prompts, vec!["third prompt"]);
    }

    #[test]
    fn test_collect_recent_prompts_fallback_to_last() {
        use std::collections::VecDeque;

        // No prompt events in recent_events, but last_user_prompt is set
        let session = SessionState {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            cwd: None,
            status: SessionStatus::Idle,
            active_tool: None,
            started_at: Utc::now(),
            last_activity: Utc::now(),
            recent_events: VecDeque::new(),
            tool_count: 0,
            last_user_prompt: Some("old prompt".to_string()),
            first_prompts: Vec::new(),
            pane_id: None,
        };

        let prompts = collect_recent_prompts(&session, 3);
        assert_eq!(prompts, vec!["old prompt"]);
    }

    #[test]
    fn test_collect_recent_prompts_empty() {
        use std::collections::VecDeque;

        let session = SessionState {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            cwd: None,
            status: SessionStatus::Idle,
            active_tool: None,
            started_at: Utc::now(),
            last_activity: Utc::now(),
            recent_events: VecDeque::new(),
            tool_count: 0,
            last_user_prompt: None,
            first_prompts: Vec::new(),
            pane_id: None,
        };

        let prompts = collect_recent_prompts(&session, 3);
        assert!(prompts.is_empty());
    }

    // ---------------------------------------------------------------------------
    // keyevent_to_bytes tests
    // ---------------------------------------------------------------------------

    #[test]
    fn keyevent_printable_ascii() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(keyevent_to_bytes(&key), Some(vec![b'a']));

        let key = KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert_eq!(keyevent_to_bytes(&key), Some(vec![b'Z']));
    }

    #[test]
    fn keyevent_enter_tab_backspace_esc() {
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(vec![b'\t'])
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(vec![0x7f])
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn keyevent_ctrl_c_and_ctrl_a() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(keyevent_to_bytes(&key), Some(vec![0x03]));

        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(keyevent_to_bytes(&key), Some(vec![0x01]));

        let key = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(keyevent_to_bytes(&key), Some(vec![0x1a]));
    }

    #[test]
    fn keyevent_alt_prefix() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(keyevent_to_bytes(&key), Some(vec![0x1b, b'x']));
    }

    #[test]
    fn keyevent_arrow_keys() {
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn keyevent_f_keys() {
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)),
            Some(b"\x1b[24~".to_vec())
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::F(13), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn keyevent_special_nav_keys() {
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            Some(b"\x1b[F".to_vec())
        );
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn keyevent_backtab() {
        assert_eq!(
            keyevent_to_bytes(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn handle_pane_input_forwards_printable() {
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        match handle_pane_input_key(key) {
            KeyResult::ForwardToPane(bytes) => assert_eq!(bytes, vec![b'l']),
            other => panic!("Expected ForwardToPane, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Idle ASCII art tests
    // -----------------------------------------------------------------------

    fn idle_art_config(enabled: bool, timeout_secs: u64) -> IdleArtConfig {
        IdleArtConfig {
            enabled,
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5".to_string(),
            timeout_secs,
        }
    }

    // -----------------------------------------------------------------------
    // Unified NewPaneFormState tests
    // -----------------------------------------------------------------------

    fn make_mode(name: &str) -> ModeConfig {
        ModeConfig {
            name: name.to_string(),
            init_command: None,
            panes: vec![],
            rules: vec![],
            reactive_panes: 2,
        }
    }

    fn make_orchestration(name: &str) -> OrchestrationConfig {
        OrchestrationConfig {
            name: name.to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "coder".to_string(),
                    command: "claude".to_string(),
                    start: true,
                    description: None,
                    prompt_template: Some("Code.".to_string()),
                    clear: true,
                },
                OrchestrationRoleConfig {
                    name: "reviewer".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: Some("Reviews code".to_string()),
                    prompt_template: Some("Review.".to_string()),
                    clear: true,
                },
            ],
        }
    }

    #[test]
    fn test_update_idle_art_gated_on_config_disabled() {
        let mut cache = HashMap::new();
        let config = idle_art_config(false, 10);
        let mut sessions = HashMap::new();
        let mut s = make_session(SessionStatus::Idle);
        s.session_id = "s1".to_string();
        s.last_activity = Utc::now() - Duration::seconds(100);
        sessions.insert("s1".to_string(), s);

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Spacious);
        assert!(cache.is_empty(), "Should not create entries when disabled");
    }

    #[test]
    fn test_update_idle_art_gated_on_density() {
        let mut cache = HashMap::new();
        let config = idle_art_config(true, 10);
        let mut sessions = HashMap::new();
        let mut s = make_session(SessionStatus::Idle);
        s.session_id = "s1".to_string();
        s.last_activity = Utc::now() - Duration::seconds(100);
        sessions.insert("s1".to_string(), s);

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Normal);
        assert!(
            cache.is_empty(),
            "Should not create entries in Normal density"
        );

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Compact);
        assert!(
            cache.is_empty(),
            "Should not create entries in Compact density"
        );
    }

    #[test]
    fn unified_form_mode_option_count() {
        let f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("a")],
            vec![],
        );
        assert_eq!(f.mode_option_count(), 2); // "No mode" + 1 mode

        let f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![],
            vec![],
        );
        assert_eq!(f.mode_option_count(), 1); // "No mode" only
    }

    #[test]
    fn unified_form_mode_cycling() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("alpha"), make_mode("beta")],
            vec![],
        );
        assert_eq!(f.selection_index, 0);

        // Can't go below 0
        f.select_previous_mode();
        assert_eq!(f.selection_index, 0);

        f.select_next_mode();
        assert_eq!(f.selection_index, 1);
        f.select_next_mode();
        assert_eq!(f.selection_index, 2);

        // Can't go past last
        f.select_next_mode();
        assert_eq!(f.selection_index, 2);
    }

    #[test]
    fn unified_form_selected_mode() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("k8s"), make_mode("rust-tdd")],
            vec![],
        );

        // Index 0 = "No mode"
        assert!(f.selected_mode().is_none());
        assert_eq!(f.mode_display_name(), "No mode");

        f.selection_index = 1;
        assert_eq!(f.selected_mode().unwrap().name, "k8s");
        assert_eq!(f.mode_display_name(), "k8s");

        f.selection_index = 2;
        assert_eq!(f.selected_mode().unwrap().name, "rust-tdd");
    }

    #[test]
    fn unified_form_selected_orchestration() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("dev")],
            vec![make_orchestration("tdd"), make_orchestration("review")],
        );

        // Index 0 = "No mode", 1 = mode "dev", 2+ = orchestrations
        assert!(f.selected_orchestration().is_none());
        assert!(f.selected_mode().is_none());

        f.selection_index = 1;
        assert!(f.selected_orchestration().is_none());
        assert_eq!(f.selected_mode().unwrap().name, "dev");

        f.selection_index = 2;
        assert!(f.selected_mode().is_none());
        assert_eq!(f.selected_orchestration().unwrap().name, "tdd");
        assert_eq!(f.mode_display_name(), "Orch: tdd");

        f.selection_index = 3;
        assert_eq!(f.selected_orchestration().unwrap().name, "review");
        assert_eq!(f.mode_display_name(), "Orch: review");
    }

    #[test]
    fn unified_form_orchestration_cycling() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("dev")],
            vec![make_orchestration("tdd")],
        );
        // 0=No mode, 1=dev, 2=tdd
        assert_eq!(f.mode_option_count(), 3);

        f.select_next_mode();
        f.select_next_mode();
        assert_eq!(f.selection_index, 2);
        assert_eq!(f.selected_orchestration().unwrap().name, "tdd");

        // Can't go past last
        f.select_next_mode();
        assert_eq!(f.selection_index, 2);
    }

    #[test]
    fn unified_form_tab_cycles_with_mode() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("a")],
            vec![],
        );
        assert_eq!(f.focused, FormField::Mode);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Name);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Command);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Mode); // wraps

        // Reverse
        f.focused = f.prev_field();
        assert_eq!(f.focused, FormField::Command);
    }

    #[test]
    fn unified_form_tab_cycles_without_mode() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![],
            vec![],
        );
        assert!(!f.has_mode_field);
        assert_eq!(f.focused, FormField::Name);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Command);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Name); // wraps, skips Mode

        f.focused = f.prev_field();
        assert_eq!(f.focused, FormField::Command);
    }

    #[test]
    fn unified_form_initial_focus_with_modes() {
        let f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("a")],
            vec![],
        );
        assert_eq!(f.focused, FormField::Mode);
    }

    #[test]
    fn unified_form_initial_focus_without_modes() {
        let f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![],
            vec![],
        );
        assert_eq!(f.focused, FormField::Name);
    }

    #[test]
    fn unified_form_arrow_cycles_mode() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("a"), make_mode("b")],
            vec![],
        ));

        // Right arrow cycles forward
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_new_pane_form_key(key, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().selection_index, 1);

        // Left arrow cycles back
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        handle_new_pane_form_key(key, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().selection_index, 0);
    }

    #[test]
    fn unified_form_enter_navigates_fields() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            "agent".to_string(),
            "claude".to_string(),
            vec![make_mode("a")],
            vec![],
        ));

        // Enter on Mode → Name
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(key, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().focused, FormField::Name);

        // Enter on Name → Command
        handle_new_pane_form_key(key, &mut ui);
        assert_eq!(
            ui.new_pane_form.as_ref().unwrap().focused,
            FormField::Command
        );

        // Enter on Command → submit
        let result = handle_new_pane_form_key(key, &mut ui);
        assert_eq!(ui.mode, UiMode::Normal);
        assert!(ui.new_pane_form.is_none());
        assert!(matches!(result, KeyResult::NewPane(_)));
    }

    #[test]
    fn unified_form_submit_with_mode() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp/proj"),
            "agent".to_string(),
            "claude".to_string(),
            vec![make_mode("k8s-ops")],
            vec![],
        ));

        // Select mode "k8s-ops" (index 1)
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_new_pane_form_key(right, &mut ui);

        // Navigate to Command field and submit
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui); // Mode → Name
        handle_new_pane_form_key(enter, &mut ui); // Name → Command
        let result = handle_new_pane_form_key(enter, &mut ui); // submit

        match result {
            KeyResult::NewPane(req) => {
                assert_eq!(req.dir, PathBuf::from("/tmp/proj"));
                assert!(
                    req.mode_config
                        .as_ref()
                        .is_some_and(|c| c.name == "k8s-ops")
                );
            }
            other => panic!("Expected NewPane, got {:?}", other),
        }
    }

    #[test]
    fn unified_form_submit_no_mode() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            "agent".to_string(),
            "claude".to_string(),
            vec![make_mode("a")],
            vec![],
        ));

        // Stay on "No mode" (index 0), navigate through fields
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui); // Mode → Name
        handle_new_pane_form_key(enter, &mut ui); // Name → Command
        let result = handle_new_pane_form_key(enter, &mut ui);

        match result {
            KeyResult::NewPane(req) => {
                assert!(req.mode_config.is_none());
            }
            other => panic!("Expected NewPane, got {:?}", other),
        }
    }

    #[test]
    fn unified_form_esc_cancels() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("a")],
            vec![],
        ));

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_new_pane_form_key(key, &mut ui);

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(ui.new_pane_form.is_none());
    }

    #[test]
    fn unified_form_typing_in_text_fields() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("a")],
            vec![],
        ));

        // Move to Name field
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui);

        // Type into Name field
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        handle_new_pane_form_key(key, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().name, "x");

        // Backspace
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        handle_new_pane_form_key(key, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().name, "");
    }

    #[test]
    fn unified_form_h_l_types_chars_in_name() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("a")],
            vec![],
        ));

        // Move to Name field
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui);

        // h and l should type characters, not cycle modes
        let key_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        handle_new_pane_form_key(key_h, &mut ui);
        let key_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        handle_new_pane_form_key(key_l, &mut ui);

        assert_eq!(ui.new_pane_form.as_ref().unwrap().name, "hl");
    }

    #[test]
    fn config_gen_prompt_enter_on_yes_sends_prompt() {
        let mut ui = default_ui();
        ui.mode = UiMode::ConfigGenPrompt;
        ui.config_gen_selected = 0; // Yes
        ui.config_gen_target = Some(("pane-1".to_string(), "/my/project".to_string()));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = handle_config_gen_prompt_key(enter, &mut ui);

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(ui.config_gen_target.is_none());
        assert!(
            matches!(result, KeyResult::SendConfigGenPrompt { ref pane_id, ref cwd }
                if pane_id == "pane-1" && cwd == "/my/project")
        );
    }

    #[test]
    fn test_update_idle_art_waiting_before_timeout() {
        let mut cache = HashMap::new();
        let config = idle_art_config(true, 300);
        let mut sessions = HashMap::new();
        let mut s = make_session(SessionStatus::Idle);
        s.session_id = "s1".to_string();
        s.last_activity = Utc::now() - Duration::seconds(10); // only 10s, timeout is 300s
        sessions.insert("s1".to_string(), s);

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Spacious);
        assert!(cache.contains_key("s1"));
        assert!(matches!(cache["s1"].phase, IdleArtPhase::Waiting));
    }

    #[test]
    fn test_update_idle_art_reset_on_active() {
        let mut cache = HashMap::new();
        cache.insert(
            "s1".to_string(),
            IdleArtEntry {
                phase: IdleArtPhase::HasArt(AsciiArtResult {
                    frames: vec!["art".to_string()],
                }),
                idle_since: Utc::now() - Duration::seconds(600),
                dismissed: false,
            },
        );

        let config = idle_art_config(true, 300);
        let mut sessions = HashMap::new();
        let mut s = make_session(SessionStatus::Working); // no longer idle
        s.session_id = "s1".to_string();
        sessions.insert("s1".to_string(), s);

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Spacious);
        assert!(
            !cache.contains_key("s1"),
            "Should remove entry when session is no longer idle"
        );
    }

    #[test]
    fn test_update_idle_art_reset_on_new_idle_stretch() {
        let old_idle_since = Utc::now() - Duration::seconds(600);
        let new_idle_since = Utc::now() - Duration::seconds(5);
        let mut cache = HashMap::new();
        cache.insert(
            "s1".to_string(),
            IdleArtEntry {
                phase: IdleArtPhase::HasArt(AsciiArtResult {
                    frames: vec!["old art".to_string()],
                }),
                idle_since: old_idle_since,
                dismissed: false,
            },
        );

        let config = idle_art_config(true, 300);
        let mut sessions = HashMap::new();
        let mut s = make_session(SessionStatus::Idle);
        s.session_id = "s1".to_string();
        s.last_activity = new_idle_since; // different from old idle_since
        sessions.insert("s1".to_string(), s);

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Spacious);
        assert!(
            matches!(cache["s1"].phase, IdleArtPhase::Waiting),
            "Should reset to Waiting on new idle stretch"
        );
        assert_eq!(cache["s1"].idle_since, new_idle_since);
    }

    #[test]
    fn test_update_idle_art_removes_stale_sessions() {
        let mut cache = HashMap::new();
        cache.insert(
            "gone".to_string(),
            IdleArtEntry {
                phase: IdleArtPhase::Waiting,
                idle_since: Utc::now(),
                dismissed: false,
            },
        );

        let config = idle_art_config(true, 300);
        let sessions = HashMap::new(); // empty — "gone" no longer exists

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Spacious);
        assert!(
            !cache.contains_key("gone"),
            "Should remove entries for non-existent sessions"
        );
    }

    #[test]
    fn test_build_art_input_combines_prompts() {
        let mut s = make_session(SessionStatus::Idle);
        s.first_prompts = vec!["Fix auth".to_string(), "Add tests".to_string()];
        s.last_user_prompt = Some("Run deploy".to_string());

        let input = build_art_input(&s);
        assert_eq!(input, "Fix auth | Add tests | Run deploy");
    }

    #[test]
    fn test_build_art_input_deduplicates_last_prompt() {
        let mut s = make_session(SessionStatus::Idle);
        s.first_prompts = vec!["Fix auth".to_string()];
        s.last_user_prompt = Some("Fix auth".to_string()); // same as first

        let input = build_art_input(&s);
        assert_eq!(input, "Fix auth");
    }

    #[test]
    fn test_build_art_input_empty() {
        let s = make_session(SessionStatus::Idle);
        let input = build_art_input(&s);
        assert_eq!(input, "");
    }

    #[test]
    fn test_build_art_output_with_tools() {
        let mut s = make_session(SessionStatus::Idle);
        s.recent_events.push_back(AgentEvent {
            session_id: "s1".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            tool_name: Some("Bash".to_string()),
            tool_detail: None,
            cwd: None,
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
        });
        let output = build_art_output(&s);
        assert!(output.contains("Bash"));
    }

    #[test]
    fn test_build_art_output_no_tools() {
        let s = make_session(SessionStatus::Idle);
        let output = build_art_output(&s);
        assert_eq!(output, "Session idle");
    }

    #[test]
    fn test_frame_cycling() {
        // 3 frames, cycling at 120 ticks per frame
        let num_frames = 3;
        assert_eq!((0u64 / 120) as usize % num_frames, 0);
        assert_eq!((119u64 / 120) as usize % num_frames, 0);
        assert_eq!((120u64 / 120) as usize % num_frames, 1);
        assert_eq!((239u64 / 120) as usize % num_frames, 1);
        assert_eq!((240u64 / 120) as usize % num_frames, 2);
        assert_eq!((360u64 / 120) as usize % num_frames, 0); // wraps
    }

    #[test]
    fn test_idle_art_has_art_cached() {
        let idle_since = Utc::now() - Duration::seconds(600);
        let mut cache = HashMap::new();
        let art = AsciiArtResult {
            frames: vec!["frame1".to_string(), "frame2".to_string()],
        };
        cache.insert(
            "s1".to_string(),
            IdleArtEntry {
                phase: IdleArtPhase::HasArt(art),
                idle_since,
                dismissed: false,
            },
        );

        let config = idle_art_config(true, 300);
        let mut sessions = HashMap::new();
        let mut s = make_session(SessionStatus::Idle);
        s.session_id = "s1".to_string();
        s.last_activity = idle_since;
        sessions.insert("s1".to_string(), s);

        // Update should NOT reset HasArt to Waiting (same idle_since)
        update_idle_art(&mut cache, &config, &sessions, CardDensity::Spacious);
        assert!(
            matches!(cache["s1"].phase, IdleArtPhase::HasArt(_)),
            "Should keep cached art for same idle stretch"
        );
    }

    #[test]
    fn test_idle_art_failed_stays_failed_within_cooldown() {
        let mut cache = HashMap::new();
        let idle_since = Utc::now() - Duration::seconds(600);
        cache.insert(
            "s1".to_string(),
            IdleArtEntry {
                phase: IdleArtPhase::Failed(std::time::Instant::now()),
                idle_since,
                dismissed: false,
            },
        );

        let config = idle_art_config(true, 300);
        let mut sessions = HashMap::new();
        let mut s = make_session(SessionStatus::Idle);
        s.session_id = "s1".to_string();
        s.last_activity = idle_since;
        sessions.insert("s1".to_string(), s);

        update_idle_art(&mut cache, &config, &sessions, CardDensity::Spacious);
        assert!(
            matches!(cache["s1"].phase, IdleArtPhase::Failed(_)),
            "Failed should stay Failed within cooldown period"
        );
    }

    #[test]
    fn config_gen_prompt_enter_on_no_dismisses() {
        let mut ui = default_ui();
        ui.mode = UiMode::ConfigGenPrompt;
        ui.config_gen_selected = 1; // No
        ui.config_gen_target = Some(("pane-1".to_string(), "/my/project".to_string()));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = handle_config_gen_prompt_key(enter, &mut ui);

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(ui.config_gen_target.is_none());
        assert!(matches!(result, KeyResult::Continue));
    }

    #[test]
    fn config_gen_prompt_enter_on_never_suppresses_dir() {
        let mut ui = default_ui();
        ui.mode = UiMode::ConfigGenPrompt;
        ui.config_gen_selected = 2; // Never
        ui.config_gen_target = Some(("pane-1".to_string(), "/my/project".to_string()));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = handle_config_gen_prompt_key(enter, &mut ui);

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(ui.config_gen_target.is_none());
        assert!(ui.config_gen_state.is_suppressed("/my/project"));
        assert!(matches!(result, KeyResult::Continue));
        assert!(ui.status_message.is_some());
    }

    #[test]
    fn config_gen_prompt_arrow_navigation() {
        let mut ui = default_ui();
        ui.mode = UiMode::ConfigGenPrompt;
        ui.config_gen_selected = 0;

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_config_gen_prompt_key(down, &mut ui);
        assert_eq!(ui.config_gen_selected, 1);

        handle_config_gen_prompt_key(down, &mut ui);
        assert_eq!(ui.config_gen_selected, 2);

        // Can't go past last
        handle_config_gen_prompt_key(down, &mut ui);
        assert_eq!(ui.config_gen_selected, 2);

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        handle_config_gen_prompt_key(up, &mut ui);
        assert_eq!(ui.config_gen_selected, 1);
    }

    #[test]
    fn config_gen_prompt_esc_dismisses() {
        let mut ui = default_ui();
        ui.mode = UiMode::ConfigGenPrompt;
        ui.config_gen_target = Some(("pane-1".to_string(), "/my/project".to_string()));

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let result = handle_config_gen_prompt_key(esc, &mut ui);

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(matches!(result, KeyResult::Continue));
    }
}
