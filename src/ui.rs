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

use crate::agent_pty::TabMembership;
use crate::ascii_art::{AsciiArtResult, generate_ascii_art};
use crate::config;
use crate::config::{BellConfig, DashboardConfig, IdleArtConfig};
use crate::embedded_pane::{EmbeddedPaneController, HydratedPane};
use crate::event::{AgentType, EventType};
use crate::pane::{AgentSpawnOptions, PaneController, PaneError, RenameOutcome};
use crate::project_config::{ModeConfig, OrchestrationConfig, load_project_config};
use crate::state::{AppState, DashboardStats, SessionState, SessionStatus, SharedState};
use crate::tab::{OrchestrationRoleStatus, OrchestrationStatus, Tab, TabId, TabManager};
use crate::tab_layout::fit_tab_labels;
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
    /// PRD #92 F1: secondary y/n confirmation that appears only when the
    /// user has selected **Stop** in the primary QuitConfirm dialog AND
    /// there is at least one managed agent that would be terminated.
    /// Defaults to No (index 0). Pressing y / Enter on Yes confirms the
    /// shutdown; pressing n / Esc / Enter on No returns to QuitConfirm
    /// with Stop still selected so the user can pick a different option
    /// without restarting the Ctrl+C sequence.
    StopConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneLayout {
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
    /// Orchestration tab: same card layout as dashboard, scoped to role panes.
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

    /// PRD #106: the Command field is hidden (and the form treats it as
    /// non-existent for navigation/submission) whenever the user has selected
    /// an orchestration. Orchestrations spawn role panes via commands defined
    /// in `.dot-agent-deck.toml`, so a user-typed command is silently ignored
    /// — we drop the field rather than presenting a false affordance.
    fn command_visible(&self) -> bool {
        self.selected_orchestration().is_none()
    }

    fn next_field(&self) -> FormField {
        let cmd_visible = self.command_visible();
        match self.focused {
            FormField::Mode => FormField::Name,
            FormField::Name => {
                if cmd_visible {
                    FormField::Command
                } else if self.has_mode_field {
                    FormField::Mode
                } else {
                    FormField::Name
                }
            }
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
        let cmd_visible = self.command_visible();
        match self.focused {
            FormField::Mode => {
                if cmd_visible {
                    FormField::Command
                } else {
                    FormField::Name
                }
            }
            FormField::Name => {
                if self.has_mode_field {
                    FormField::Mode
                } else if cmd_visible {
                    FormField::Command
                } else {
                    FormField::Name
                }
            }
            FormField::Command => FormField::Name,
        }
    }
}

/// How long status-bar messages stay visible before clearing. Long enough that
/// errors (e.g. orchestration spawn failures) are readable, short enough that
/// transient info messages don't linger past their usefulness.
const STATUS_MESSAGE_TTL: std::time::Duration = std::time::Duration::from_secs(15);

/// PRD #76 M2.20 — minimum gap between the last forwarded keystroke and an
/// Enter keystroke that follows it on the human-typing path. Agent TUIs like
/// claude treat a CR fused to preceding bytes as newline-in-input, not submit;
/// only a CR separated by a brief pause is honored as Enter. Mirrors the
/// programmatic submit guard in `src/embedded_pane.rs:199` (`SUBMIT_DELAY`),
/// which was tuned empirically — keep the two values in sync.
const SUBMIT_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// Returns how long the human-typing dispatch should sleep before forwarding
/// `bytes` to the pane, to ensure an Enter keystroke arrives as a standalone
/// event rather than fused to recent typing. Returns `Duration::ZERO` unless
/// `bytes` contains `\r` and the elapsed time since the previous forward is
/// below `SUBMIT_DEBOUNCE`. Extracted as a helper so the policy is unit-testable.
fn submit_debounce_duration(
    now: std::time::Instant,
    last: Option<std::time::Instant>,
    bytes: &[u8],
) -> std::time::Duration {
    if !bytes.contains(&b'\r') {
        return std::time::Duration::ZERO;
    }
    let Some(prev) = last else {
        return std::time::Duration::ZERO;
    };
    let elapsed = now.saturating_duration_since(prev);
    SUBMIT_DEBOUNCE.checked_sub(elapsed).unwrap_or_default()
}

/// A prompt queued for injection into a pane once its agent is ready.
/// Used by M5 delegation dispatch when `clear = true` restarts a pane.
struct PendingDispatch {
    pane_id: String,
    prompt: String,
    created_at: std::time::Instant,
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
    /// Selected option in quit confirm modal (0=Detach, 1=Stop, 2=Cancel).
    /// PRD #92 F1 widened this from 2 to 3 options; Detach remains the
    /// default so the existing muscle memory does not become destructive.
    quit_confirm_selected: usize,
    /// PRD #92 F1: selected option in the secondary Stop-confirmation
    /// dialog (0=No, 1=Yes). Defaults to No — the safer default for the
    /// destructive choice. Only meaningful when `mode == StopConfirm`.
    stop_confirm_selected: usize,
    /// PRD #92 F1: cached managed-agent count captured at the moment the
    /// user picks Stop in the QuitConfirm dialog with at least one agent
    /// alive. Used purely to render the secondary dialog's text ("{N}
    /// managed agent(s) will be terminated..."); the daemon's own
    /// registry is authoritative for the actual termination.
    stop_confirm_agent_count: usize,
    /// Orchestration tab IDs whose start-role prompt has already been injected.
    orchestration_prompted: HashSet<TabId>,
    /// Tracks when orchestration tabs were created (for delayed prompt injection).
    orchestration_created_at: HashMap<TabId, std::time::Instant>,
    /// Prompts waiting to be injected into panes once their agent is ready (M5 dispatch).
    pending_dispatches: Vec<PendingDispatch>,
    /// PRD #76 M2.20: timestamp of the most recent keystroke forwarded to a
    /// pane via `ForwardToPane`. Drives the submit-debounce in `PaneInput` mode
    /// so an Enter keystroke arriving fused to preceding typed bytes is
    /// delayed just enough that the agent TUI treats it as a standalone submit
    /// (matches `write_to_pane`'s SUBMIT_DELAY rationale at
    /// src/embedded_pane.rs:199).
    last_pane_keystroke_at: Option<std::time::Instant>,
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
            stop_confirm_selected: 0,
            stop_confirm_agent_count: 0,
            orchestration_prompted: HashSet::new(),
            orchestration_created_at: HashMap::new(),
            pending_dispatches: Vec::new(),
            last_pane_keystroke_at: None,
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

// PRD #76 M2.15 layout-math helpers — single source of truth for the
// dimensions a freshly-spawned agent PTY (`AgentSpawnOptions.rows/cols`)
// and a resize-time `resize_pane_pty` call should agree on. Before this,
// every spawn site hardcoded 24×80 and every resize site reimplemented
// the layout math inline, so the two could (and did) drift. Now the spawn
// callsites and `resize_dashboard_panes` / `resize_mode_tab_panes` all
// call these, so the agent's first frame paints at the eventual size and
// stays there. All helpers return `(rows, cols)` (= `(height, width)` of
// the inner area inside the pane's border).

/// Agent pane of a mode tab: left half × full height minus 3 rows of
/// chrome (tab bar + hints bar). Mirrors the mode-tab render layout.
pub(crate) fn mode_agent_pane_dims(area: Rect) -> (u16, u16) {
    let half_width = (area.width / 2).saturating_sub(2);
    let rows = area.height.saturating_sub(3);
    (rows, half_width)
}

/// Side panes of a mode tab: right half divided equally by `side_count`,
/// minus the per-pane border. `side_count` is clamped to ≥1 so the layout
/// math doesn't divide by zero before the first side pane appears.
pub(crate) fn mode_side_pane_dims(area: Rect, side_count: u16) -> (u16, u16) {
    let half_width = (area.width / 2).saturating_sub(2);
    let count = side_count.max(1);
    let rows = (area.height / count).saturating_sub(2);
    (rows, half_width)
}

/// Shared layout constants for the dashboard and orchestration tab
/// renderers. Extracted into `const`s (rather than inlined at each
/// `Layout::horizontal(...)` callsite) so the SSOT helpers below and the
/// renderer in `draw_active_tab` mathematically agree by construction.
/// PRD #76 M2.15 fixup F3 — the orchestration helper would silently drift
/// from the renderer the next time someone touched one of the percentages
/// if the value lived in two places.
pub(crate) const DASHBOARD_LEFT_PERCENT: u16 = 33;
pub(crate) const DASHBOARD_PANES_PERCENT: u16 = 67;
pub(crate) const ORCHESTRATION_LEFT_PERCENT: u16 = 34;
pub(crate) const ORCHESTRATION_PANES_PERCENT: u16 = 66;

/// Inner helper: right column dims for a dashboard/orchestration-style tab
/// where the right column holds a vertical stack of `pane_count` panes.
/// Factors out the common chrome/stack math so the dashboard (67%) and
/// orchestration (66%) helpers can share the body and only differ on the
/// width percentage.
fn right_column_pane_dims(
    area: Rect,
    width_percent: u16,
    pane_count: u16,
    is_focused: bool,
    layout: PaneLayout,
    show_tab_bar: bool,
) -> (u16, u16) {
    let chrome_rows: u16 = if show_tab_bar { 1 } else { 0 };
    let main_height = area.height.saturating_sub(chrome_rows + 1); // +1 for hints bar
    let right_width = area.width * width_percent / 100;
    let count = pane_count.max(1);
    let chunk_height = match layout {
        PaneLayout::Tiled => main_height / count,
        PaneLayout::Stacked => {
            if is_focused {
                let unfocused = count.saturating_sub(1);
                main_height.saturating_sub(unfocused)
            } else {
                1
            }
        }
    };
    let rows = chunk_height.saturating_sub(2);
    let cols = right_width.saturating_sub(2);
    (rows, cols)
}

/// Dashboard pane: right 67% of width, height divided across `pane_count`
/// panes per the active `PaneLayout`. `is_focused` matters in `Stacked`
/// mode (focused gets the lion's share, unfocused collapse to a 1-row
/// title bar); in `Tiled` it's ignored. `show_tab_bar` matches
/// `TabManager::show_tab_bar` and adds 1 row of chrome.
pub(crate) fn dashboard_pane_dims(
    area: Rect,
    pane_count: u16,
    is_focused: bool,
    layout: PaneLayout,
    show_tab_bar: bool,
) -> (u16, u16) {
    right_column_pane_dims(
        area,
        DASHBOARD_PANES_PERCENT,
        pane_count,
        is_focused,
        layout,
        show_tab_bar,
    )
}

/// Orchestration role pane: right 66% of width (the orchestration
/// renderer uses a `[34%, 66%]` horizontal split — one column narrower
/// than the dashboard's `[33%, 67%]` to leave room for the role status
/// gutter), height divided across `role_count` role panes per `layout`.
///
/// `role_index` and `focused_role_index` matter only in `Stacked` mode.
/// The helper treats the role at `focused_role_index` as expanded so
/// the daemon-side PTY dims match the renderer's "expanded slot"
/// decision in `render_terminal_panes` (see ui.rs Stacked branch around
/// `focused_idx`). When `focused_role_index` is `None`, role 0 is
/// expanded — mirroring the renderer's "expand the first slot if
/// nothing is focused" fallback. In `Tiled` mode both indices are
/// ignored.
///
/// `show_tab_bar` is exposed for symmetry with `dashboard_pane_dims` and
/// because hydration-time callers may briefly be in a "single-tab" state
/// before the orchestration tab is added; production callsites pass the
/// live `TabManager::show_tab_bar()`.
///
/// PRD #76 M2.15 fixup F3 — before this helper, every spawn / resize
/// site for orchestration role panes called `dashboard_pane_dims`, which
/// uses the dashboard's 67% column. The daemon-side PTY ended up one
/// column wider than the rendered area, recreating exactly the
/// spawn-vs-render drift M2.15 was meant to fix.
///
/// PRD #76 M2.15 fixup pass 2 G2 — before this parameter existed, the
/// helper hardcoded `role_index == 0` as the expanded slot. That
/// matched the renderer only when nothing was focused. As soon as the
/// orchestrator handed off to a non-zero role (or the user tabbed
/// across roles), the resize sweep gave role 0 the expanded height
/// while the visibly-expanded focused role kept the collapsed height —
/// recreating the exact spawn-vs-render drift M2.15 was meant to fix.
/// `focused_role_index` lets resize callers thread the renderer's
/// focus decision (via [`focused_orchestration_role_index`]) through
/// to the helper so the two stay aligned by construction.
pub(crate) fn orchestration_role_pane_dims(
    frame_area: Rect,
    role_count: usize,
    role_index: usize,
    focused_role_index: Option<usize>,
    layout: PaneLayout,
    show_tab_bar: bool,
) -> (u16, u16) {
    let is_focused = match focused_role_index {
        Some(focused) => focused == role_index,
        // Mirror the renderer's "expand the first slot if nothing is
        // focused" fallback (see `render_terminal_panes` Stacked
        // branch — when `focused_idx` is None it sets index 0 to
        // `Constraint::Fill(1)`).
        None => role_index == 0,
    };
    right_column_pane_dims(
        frame_area,
        ORCHESTRATION_PANES_PERCENT,
        role_count as u16,
        is_focused,
        layout,
        show_tab_bar,
    )
}

/// PRD #76 M2.15 fixup pass 2 G2 — single source of truth for "which
/// orchestration role is currently the expanded slot". Used by both
/// the resize sweep (so PTY dims match the renderer's expanded slot)
/// and any callsite that needs the same notion of focused-role index.
/// Mirrors `render_terminal_panes` Stacked branch: a slot is focused
/// iff `embedded.focused_pane_id()` points at its pane id. Returns
/// `None` if the embedded controller reports no focused pane, or the
/// focused pane id doesn't belong to `role_pane_ids` (e.g. focus moved
/// to the dashboard before the resize sweep ran).
///
/// `role_pane_ids` may include the empty-string sentinels that the
/// hydration path uses for dead orchestration slots (M2.12); those
/// entries never match a live focused pane id, so they're handled
/// implicitly without an extra filter step.
pub(crate) fn focused_orchestration_role_index(
    embedded: &EmbeddedPaneController,
    role_pane_ids: &[String],
) -> Option<usize> {
    let focused = embedded.focused_pane_id()?;
    role_pane_ids.iter().position(|id| id == &focused)
}

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
    resize_mode_tab_panes_for(pane, &agent_pane_id, &side_pane_ids, area);
}

/// Inner helper: resize a specific mode tab's agent + side panes regardless
/// of which tab is currently active. Pulled out so the M2.15 post-hydration
/// sweep can iterate every rebuilt mode tab without temporarily switching
/// tabs to make each one "active" first.
fn resize_mode_tab_panes_for(
    pane: &dyn PaneController,
    agent_pane_id: &str,
    side_pane_ids: &[String],
    area: Rect,
) {
    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
        let (agent_rows, agent_cols) = mode_agent_pane_dims(area);
        if agent_rows > 0 && agent_cols > 0 {
            let _ = embedded.resize_pane_pty(agent_pane_id, agent_rows, agent_cols);
        }
        let side_count = side_pane_ids.len().max(1) as u16;
        let (side_rows, side_cols) = mode_side_pane_dims(area, side_count);
        if side_rows > 0 && side_cols > 0 {
            for id in side_pane_ids {
                let _ = embedded.resize_pane_pty(id, side_rows, side_cols);
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
    // PRD #76 M2.15 fixup F3: orchestration role panes live in the
    // `[34%, 66%]` column, not the dashboard's `[33%, 67%]`. Branch on
    // active tab so each routes through the matching SSOT helper.
    let is_orchestration = matches!(tab_manager.active_tab(), Tab::Orchestration { .. });
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

        let pane_count = pane_ids.len() as u16;
        let focused = embedded.focused_pane_id();
        let show_tab_bar = tab_manager.show_tab_bar();

        // PRD #76 M2.15 fixup pass 2 G2 — derive the focused role index
        // via the shared helper so the resize sweep agrees with the
        // renderer's "which slot is expanded" decision in Stacked
        // layout. Before this, the helper hardcoded role_index==0 as
        // the expanded slot, so a non-zero focused role would get the
        // collapsed height while role 0 got the expanded height.
        let orch_focused_role_index = if is_orchestration {
            focused_orchestration_role_index(embedded, &pane_ids)
        } else {
            None
        };

        // PRD #76 M2.15: route through the shared layout helpers so the
        // spawn callsites (which call the same helpers to pre-compute
        // `AgentSpawnOptions.rows/cols`) agree by construction with what
        // resize-time eventually applies.
        for (idx, pane_id) in pane_ids.iter().enumerate() {
            let is_focused = focused.as_deref() == Some(pane_id.as_str())
                || (focused.is_none() && pane_id == &pane_ids[0]);
            let (rows, cols) = if is_orchestration {
                orchestration_role_pane_dims(
                    area,
                    pane_ids.len(),
                    idx,
                    orch_focused_role_index,
                    ui.pane_layout,
                    show_tab_bar,
                )
            } else {
                dashboard_pane_dims(area, pane_count, is_focused, ui.pane_layout, show_tab_bar)
            };
            if rows > 0 && cols > 0 {
                let _ = embedded.resize_pane_pty(pane_id, rows, cols);
            }
        }
    }
}

/// PRD #76 M2.15 fixup F1 — saved-session restore spawn dims for a
/// dashboard pane. Computes the eventual layout from the current
/// `frame_area`, the embedded controller's pane count (incremented to
/// account for the pane about to be added), and the live
/// `show_tab_bar` state, then routes through `dashboard_pane_dims`.
/// Each restore site previously called `pane.create_pane(...)` which
/// fell through to `AgentSpawnOptions::default()` and opened the
/// daemon-side PTY at 24×80 — the exact bug M2.15 was meant to
/// eliminate, just on the restore entry point that wasn't on the
/// original radar. The post-loop `resize_dashboard_panes` sweep still
/// runs and reconciles any rounding, but spawning at the right size
/// removes the visible 24×80 hiccup.
///
/// `is_focused=false` is used because the restore loop doesn't focus
/// any pane until the post-loop sweep; the helper is forgiving in
/// Tiled mode where `is_focused` is ignored.
fn dashboard_restore_pane_dims(
    pane: &dyn PaneController,
    tab_manager: &TabManager,
    frame_area: Rect,
) -> (u16, u16) {
    let embedded_pane_count = pane
        .as_any()
        .downcast_ref::<EmbeddedPaneController>()
        .map(|e| e.pane_ids().len())
        .unwrap_or(0);
    let pane_count_after = (embedded_pane_count as u16).saturating_add(1);
    dashboard_pane_dims(
        frame_area,
        pane_count_after,
        false,
        PaneLayout::Tiled,
        tab_manager.show_tab_bar(),
    )
}

/// Inner helper for orchestration role panes: routes through
/// `orchestration_role_pane_dims` (right 66% column) rather than the
/// dashboard's 67% column, matching the orchestration renderer's
/// `[34%, 66%]` horizontal split. Reachable regardless of which tab is
/// active so the M2.15 post-hydration sweep can iterate every rebuilt
/// orchestration tab even though hydration ends with the dashboard
/// active. Uses `PaneLayout::Tiled` as a spawn-time approximation;
/// switching to the orchestration tab re-runs `resize_dashboard_panes`
/// with the real focus state.
fn resize_orchestration_role_panes_for(
    pane: &dyn PaneController,
    role_pane_ids: &[String],
    area: Rect,
    show_tab_bar: bool,
) {
    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
        // Filter out empty-string sentinels for dead orchestration slots
        // (M2.12) so we don't issue a resize against a non-existent pane id.
        // Symptom 2 fix: also drop synthetic dead-slot pane ids
        // (`__dead-slot__-...`) — they have no PTY to resize.
        let live: Vec<&String> = role_pane_ids
            .iter()
            .filter(|id| !id.is_empty() && !is_dead_slot_pane_id(id))
            .collect();
        if live.is_empty() {
            return;
        }
        let role_count = live.len();
        // PRD #76 M2.15 fixup pass 2 G2 — share the focused-role
        // decision with the renderer. `Tiled` is hardcoded below so
        // focus doesn't affect the geometry today, but threading the
        // value keeps the API consistent and prevents future drift if
        // this path ever uses `Stacked`.
        let live_owned: Vec<String> = live.iter().map(|s| (*s).clone()).collect();
        let focused_role_index = focused_orchestration_role_index(embedded, &live_owned);
        for (role_index, pane_id) in live.into_iter().enumerate() {
            let (rows, cols) = orchestration_role_pane_dims(
                area,
                role_count,
                role_index,
                focused_role_index,
                PaneLayout::Tiled,
                show_tab_bar,
            );
            if rows > 0 && cols > 0 {
                let _ = embedded.resize_pane_pty(pane_id, rows, cols);
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
        "To delegate work to an agent, use `delegate` with one command per agent:\n\
         ```bash\n\
         dot-agent-deck delegate --to <role-name> --task \"Task description with context, file paths, and constraints.\"\n\
         ```\n\n\
         To delegate to multiple agents in parallel, make **one call per agent** so each gets its own task:\n\
         ```bash\n\
         dot-agent-deck delegate --to coder --task \"Implement the login endpoint...\"\n\
         dot-agent-deck delegate --to reviewer --task \"Review the auth module...\"\n\
         ```\n\n\
         If all agents should receive the **exact same task**, you may combine them in one call:\n\
         ```bash\n\
         dot-agent-deck delegate --to <role1> --to <role2> --task \"Same task for all.\"\n\
         ```\n\n\
         When all work is complete and you are satisfied with the results:\n\
         ```bash\n\
         dot-agent-deck work-done --done --task \"Final summary of what was accomplished.\"\n\
         ```\n",
    );

    // 4. Important guidelines.
    content.push_str(
        "\n## Important\n\n\
         Wait for the user to tell you what to work on.\n\n\
         Once you know the task, delegate immediately via the CLI commands above. \
         Do NOT ask for confirmation before delegating. \
         Do NOT offer to design, analyze, or plan — that is the workers' job. \
         Do NOT ask 'should I proceed?' or 'do you want me to delegate?' — just delegate. \
         Your only job: understand what needs doing, frame clear task descriptions, and hand off.\n\n\
         Never send a new task to a worker that is still working on a previous task. \
         Wait for its work-done signal before delegating again to the same worker. \
         Delegating to different workers in parallel is fine.\n\n\
         Delegation is one-way: orchestrator → worker. Workers NEVER delegate to other workers \
         — a `dot-agent-deck delegate` call from inside a worker does not route back through your \
         notification stream, so the downstream task is silently dropped and the calling worker \
         waits forever (or signals work-done in a paused state). When briefing a worker, never \
         instruct them to \"delegate the fix to coder\" or \"hand off to <other role>\". \
         Instead, tell them to report the diagnosis back and signal work-done; you (the orchestrator) \
         will delegate the next hop. The chain you coordinate is: worker A diagnoses → reports → \
         you delegate to worker B → worker B works → reports → you re-engage worker A.\n\n\
         When a task related to a PRD is fully completed (all workers done, reviews passed), \
         run `/prd-update-progress` yourself before signaling `--done` or moving to the next task.\n",
    );

    content
}

// ---------------------------------------------------------------------------
// M6: Skill file auto-deployment
// ---------------------------------------------------------------------------

/// Write the orchestrator context to a file and return a one-liner to inject.
/// Multi-line prompts don't submit in Claude Code via PTY, so we use a file reference.
fn prepare_orchestrator_prompt(config: &OrchestrationConfig, cwd: &str) -> Option<String> {
    let dir = std::path::Path::new(cwd).join(".dot-agent-deck");
    std::fs::create_dir_all(&dir).ok()?;
    let file_path = dir.join("orchestrator-context.md");
    let content = build_orchestrator_context(config);
    std::fs::write(&file_path, &content).ok()?;
    Some("Read .dot-agent-deck/orchestrator-context.md for your role, available agents, and delegation protocol. Acknowledge your role and wait for instructions.".to_string())
}

// ---------------------------------------------------------------------------
// PRD #76 M2.12: hydration partition
// ---------------------------------------------------------------------------

/// One mode bucket from [`partition_hydrated_panes`]: the agent pane id
/// captured from a single hydrated daemon record claiming
/// `TabMembership::Mode { name }` for the given `cwd`. Multiple records
/// matching the same `(cwd, mode_name)` are flagged as drift by the
/// partition (only the first survives; the rest are dropped to dashboard).
#[derive(Debug, Clone)]
pub(crate) struct ModeHydrationBucket {
    pub cwd: String,
    pub mode_name: String,
    pub agent_pane_id: String,
}

/// One orchestration bucket from [`partition_hydrated_panes`]:
/// the role slots for a single `(cwd, orchestration_name)` pairing. Each
/// entry carries the role's index, pane id, and the role identity
/// metadata (`role_name`, `is_start_role`) the daemon echoed back via
/// `TabMembership::Orchestration`. The hydration glue expands this to a
/// `Vec<Option<String>>` of length `config.roles.len()`, treating any
/// missing index as a dead slot (design decision 4) and any
/// out-of-range index as config drift (design decision 5). PRD #111:
/// the role identity fields let the hydration site synthesise a
/// minimal `OrchestrationConfig` when the local project config file is
/// absent (laptop TUI reconnecting to a remote daemon).
#[derive(Debug, Clone)]
pub(crate) struct OrchestrationHydrationBucket {
    pub cwd: String,
    pub orchestration_name: String,
    pub role_slots: Vec<OrchestrationRoleSlot>,
}

/// One occupied role slot inside an [`OrchestrationHydrationBucket`].
/// `role_name` and `is_start_role` come directly from the daemon's
/// `TabMembership::Orchestration` payload so the TUI can synthesise a
/// minimal `OrchestrationConfig` even when the local project config
/// file is missing (PRD #111).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrchestrationRoleSlot {
    pub role_index: usize,
    pub pane_id: String,
    pub role_name: String,
    pub is_start_role: bool,
}

/// PRD #111: pick the `OrchestrationConfig` the hydration site uses
/// when rebuilding an orchestration tab. Extracted from the hydration
/// loop so the `local-wins / synthesise-otherwise` selection has a
/// directly-testable seam (reviewer S1 — the in-loop `match` is hard
/// to drive from a unit test without standing up a full hydration
/// fixture).
///
/// Contract:
/// - `Some(local)` → return `local` verbatim. Display-only fields
///   (`description`, `prompt_template`, non-default `clear`) survive,
///   which is the whole point of the local branch.
/// - `None` → synthesise a minimal config from `bucket`'s role-slot
///   metadata. Structurally correct (same name, same role count,
///   same role names, same start role) but enrichment fields are
///   defaulted.
///
/// The hydration call site decides which `tracing::info!` line to
/// emit *before* calling this helper so the "config absent" vs
/// "config drift" distinction (auditor nit) stays observable.
pub(crate) fn resolve_orch_config_for_hydration(
    local: Option<crate::project_config::OrchestrationConfig>,
    bucket: &OrchestrationHydrationBucket,
) -> crate::project_config::OrchestrationConfig {
    if let Some(c) = local {
        return c;
    }
    let synthesis_slots: Vec<crate::project_config::SynthesisRoleSlot> = bucket
        .role_slots
        .iter()
        .map(|s| crate::project_config::SynthesisRoleSlot {
            role_index: s.role_index,
            role_name: s.role_name.clone(),
            is_start_role: s.is_start_role,
        })
        .collect();
    crate::project_config::OrchestrationConfig::synthesize_from_bucket_metadata(
        &bucket.orchestration_name,
        &synthesis_slots,
    )
}

/// Diagnostic info for a hydrated pane the partition couldn't bucket
/// cleanly. Surfaced via `HydrationPartition.rejections` so the caller
/// can emit `tracing::error!` at the hydration site and the partition
/// helper itself stays pure (no I/O, no tracing) — M2.12 fixup reviewer
/// #3.
///
/// Rejected panes are still routed to `dashboard_pane_ids`, so the
/// rejection record is purely informational: the user sees the pane on
/// the dashboard regardless. Today the only rejection reason is a
/// duplicate `(cwd, mode_name)` claim, but the variant shape leaves
/// room for future reasons without breaking call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HydrationRejection {
    /// More than one hydrated pane claimed `Mode { name }` for the same
    /// `cwd`. The first claimant won the bucket; this is the i-th
    /// duplicate that got dropped to the dashboard instead.
    DuplicateMode {
        cwd: String,
        mode_name: String,
        agent_id: String,
        pane_id: String,
    },
}

/// Output of [`partition_hydrated_panes`]: separates hydrated panes into
/// "stays on dashboard" (`dashboard_pane_ids`) and "needs a tab rebuilt"
/// (`mode_buckets`, `orchestration_buckets`). Panes that name a mode or
/// orchestration that doesn't exist in the cwd's project config end up
/// here only via the dispatcher's fallback path — the partition itself
/// is config-agnostic so it stays a pure function and can be unit-tested
/// without spinning up a full TUI.
///
/// `rejections` carries deferred diagnostics for panes that couldn't be
/// bucketed cleanly (e.g. duplicate mode claims). The partition helper
/// stays pure — no I/O, no `tracing` — so the caller is responsible for
/// logging each rejection at the hydration site (M2.12 fixup reviewer
/// #3).
#[derive(Debug, Clone, Default)]
pub(crate) struct HydrationPartition {
    pub dashboard_pane_ids: Vec<String>,
    pub mode_buckets: Vec<ModeHydrationBucket>,
    pub orchestration_buckets: Vec<OrchestrationHydrationBucket>,
    pub rejections: Vec<HydrationRejection>,
}

/// PRD #76 M2.12: partition hydrated daemon panes into dashboard / mode
/// / orchestration buckets based on each agent's recorded
/// `tab_membership`. Pure function for testability.
///
/// Rules:
/// - `tab_membership == None` → dashboard.
/// - `Some(Mode { name })` → mode bucket keyed by `(cwd, mode_name)`.
///   Cwd defaults to `""` when the daemon record omits it (older daemon
///   shape). The first record claiming a `(cwd, mode_name)` wins; later
///   duplicates from the same pairing are logged and dropped to the
///   dashboard so a buggy daemon can't double-build a single mode tab.
/// - `Some(Orchestration { name, role_index })` → orchestration bucket
///   keyed by `(cwd, orch_name)`, collecting `(role_index, pane_id)`.
///   The bucket may be sparse (a role can be missing if its agent died
///   before the TUI reattached); the dispatcher expands this to a
///   `Vec<Option<String>>` of full role-count length.
///
/// Ordering is stable: dashboard panes preserve input order, mode and
/// orchestration buckets preserve the order in which their (cwd, name)
/// pairing was first seen so the user's mental "which tab opened first"
/// model survives reconnect (TabManager appends in iteration order).
/// Key used by [`build_dedupe_budget`] / [`try_consume_dedupe_slot`] to
/// match a saved pane against a daemon-hydrated pane. The tuple is
/// `(dir, name, mode)`. `command` is intentionally excluded — daemon
/// `list_agents` doesn't echo command, so hydration always stores
/// `command = ""` and including it in the key would dedupe nothing for
/// the common case.
type SavedPaneDedupeKey = (String, String, Option<String>);

/// Build the dedupe budget from the hydration metadata. The budget
/// maps `(dir, name, mode)` keys to the COUNT of hydrated panes
/// matching that key. The restore loop consumes slots via
/// [`try_consume_dedupe_slot`] so it drops at most one saved pane per
/// hydrated match — preserving distinct saved panes that happen to
/// share a key (round-10 reviewer #2).
fn build_dedupe_budget(
    pane_metadata: &std::collections::HashMap<String, config::SavedPane>,
) -> std::collections::HashMap<SavedPaneDedupeKey, usize> {
    let mut budget: std::collections::HashMap<SavedPaneDedupeKey, usize> =
        std::collections::HashMap::new();
    for meta in pane_metadata.values() {
        *budget
            .entry((meta.dir.clone(), meta.name.clone(), meta.mode.clone()))
            .or_insert(0) += 1;
    }
    budget
}

/// Returns true and decrements the budget if `saved_pane`'s
/// `(dir, name, mode)` key has a free slot — the caller should then
/// skip restoring it. Returns false otherwise.
fn try_consume_dedupe_slot(
    budget: &mut std::collections::HashMap<SavedPaneDedupeKey, usize>,
    saved_pane: &config::SavedPane,
) -> bool {
    let key = (
        saved_pane.dir.clone(),
        saved_pane.name.clone(),
        saved_pane.mode.clone(),
    );
    if let Some(slot) = budget.get_mut(&key)
        && *slot > 0
    {
        *slot -= 1;
        return true;
    }
    false
}

pub(crate) fn partition_hydrated_panes(hydrated: &[HydratedPane]) -> HydrationPartition {
    use std::collections::HashSet;

    let mut out = HydrationPartition::default();
    // Bucket lookup tables are local: in-memory only, only used during
    // this partition pass. Index into `out.mode_buckets` /
    // `out.orchestration_buckets` keyed by `(cwd, name)`.
    let mut mode_keys: HashSet<(String, String)> = HashSet::new();
    let mut orch_index: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();

    for h in hydrated {
        let cwd = h.cwd.clone().unwrap_or_default();
        match &h.tab_membership {
            None => {
                out.dashboard_pane_ids.push(h.pane_id.clone());
            }
            Some(TabMembership::Mode { name }) => {
                let key = (cwd.clone(), name.clone());
                if !mode_keys.insert(key) {
                    // Duplicate `(cwd, mode_name)` claim. Pure helper:
                    // record the rejection for the caller to log via
                    // `tracing::error!`, then route the pane to the
                    // dashboard. M2.12 fixup reviewer #3.
                    out.rejections.push(HydrationRejection::DuplicateMode {
                        cwd: cwd.clone(),
                        mode_name: name.clone(),
                        agent_id: h.agent_id.clone(),
                        pane_id: h.pane_id.clone(),
                    });
                    out.dashboard_pane_ids.push(h.pane_id.clone());
                    continue;
                }
                out.mode_buckets.push(ModeHydrationBucket {
                    cwd,
                    mode_name: name.clone(),
                    agent_pane_id: h.pane_id.clone(),
                });
            }
            Some(TabMembership::Orchestration {
                name,
                role_index,
                role_name,
                is_start_role,
                orchestration_cwd,
            }) => {
                // Round-12 reviewer #1: bucket by `(orchestration_cwd,
                // name)` — the same identity tuple the daemon uses for
                // `pane_orchestration_map`. Round-9 #2 made each role
                // pane's own cwd independent (workers can live in
                // sub-directories of the orchestration); using the
                // per-pane cwd here would split a 3-role orchestration
                // across 3 buckets on reattach. The orchestration_cwd
                // field is shared across roles, so all three end up in
                // one bucket.
                //
                // Older daemons/clients (pre-round-11) omit the field;
                // fall back to per-pane cwd to keep the partition
                // behaviour stable for that legacy data, but log a
                // debug breadcrumb so a stale producer is visible.
                let bucket_cwd = match orchestration_cwd {
                    Some(c) => c.clone(),
                    None => {
                        tracing::debug!(
                            agent_id = %h.agent_id,
                            pane_id = %h.pane_id,
                            pane_cwd = %cwd,
                            orchestration_name = %name,
                            "hydration: orchestration pane missing orchestration_cwd — \
                             bucketing by per-pane cwd as a legacy fallback"
                        );
                        cwd.clone()
                    }
                };
                let key = (bucket_cwd.clone(), name.clone());
                let idx = match orch_index.get(&key) {
                    Some(i) => *i,
                    None => {
                        let i = out.orchestration_buckets.len();
                        orch_index.insert(key, i);
                        out.orchestration_buckets
                            .push(OrchestrationHydrationBucket {
                                cwd: bucket_cwd,
                                orchestration_name: name.clone(),
                                role_slots: Vec::new(),
                            });
                        i
                    }
                };
                out.orchestration_buckets[idx]
                    .role_slots
                    .push(OrchestrationRoleSlot {
                        role_index: *role_index,
                        pane_id: h.pane_id.clone(),
                        role_name: role_name.clone(),
                        is_start_role: *is_start_role,
                    });
            }
        }
    }

    out
}

/// Build the synthetic dead-slot pane id used to keep a role visible on
/// the orchestration tab even when no live daemon agent backs it. The
/// `__dead-slot__-` prefix is reserved for this synthesis and namespaced
/// by `(cwd, orchestration_name, role_index)` so distinct dead slots
/// never collide and a later reconnect produces the same id (idempotent
/// across reconnects).
///
/// The synthesised id is NOT a real pane: it is intentionally absent
/// from `EmbeddedPaneController::pane_ids()`, so the orchestration tab's
/// right-side terminal grid renders 4 panes for the 4 live agents while
/// the left-side card grid renders 5 cards (one per role) because the
/// placeholder session sitting on the synthetic id satisfies the
/// `pane_id ∈ role_pane_ids` filter in [`render_frame`].
///
/// Follow-up to 0d5e651 (auditor finding #4): the variable-width
/// components are length-prefixed so distinct (cwd,
/// orchestration_name, role_index) tuples can never collide. The
/// previous `-`-separated form was ambiguous whenever cwd or
/// orchestration_name contained hyphens: e.g. (cwd="/a", name="b-c",
/// idx=1) and (cwd="/a-b", name="c", idx=1) both produced
/// `__dead-slot__-/a-b-c-1`.
pub fn dead_slot_pane_id(cwd: &str, orchestration_name: &str, role_index: usize) -> String {
    format!(
        "{DEAD_SLOT_PREFIX}{cwd_len}-{cwd}-{name_len}-{orchestration_name}-{role_index}",
        cwd_len = cwd.len(),
        name_len = orchestration_name.len(),
    )
}

/// Reserved prefix for synthetic dead-slot pane ids produced by
/// [`dead_slot_pane_id`]. Anything starting with this prefix is a
/// placeholder used only by the orchestration tab's card grid — it has
/// no backing PTY, isn't tracked by `EmbeddedPaneController`, and must
/// be skipped by close / managed-pane traversals.
pub const DEAD_SLOT_PREFIX: &str = "__dead-slot__-";

/// Returns true if `pane_id` is one of the synthetic dead-slot ids
/// produced by [`dead_slot_pane_id`]. Used by `close_tab` and
/// `all_managed_pane_ids` to skip synthesised slots — there is no
/// daemon-side pane to close and no real terminal to render on the
/// right-hand side of the orchestration tab.
pub fn is_dead_slot_pane_id(pane_id: &str) -> bool {
    pane_id.starts_with(DEAD_SLOT_PREFIX)
}

/// Fill missing role slots in `role_pane_ids` with synthetic dead-slot
/// pane ids. Returns the list of synthetic ids that were assigned (in
/// `role_index` order) so callers can later seed placeholder sessions
/// for them — or skip the seeding entirely if a subsequent step fails.
///
/// This is the pure half of [`fill_dead_slots_with_placeholders`]: it
/// only mutates `role_pane_ids`, never touches `AppState`. Splitting
/// the two phases is what lets the production hydration loop defer
/// placeholder-session insertion until AFTER
/// `open_orchestration_tab_with_existing_role_panes` succeeds
/// (CodeRabbit PR #118 finding #3). The old fill-first-then-open
/// ordering leaked placeholder sessions into `AppState` whenever the
/// tab-open call returned `Err` — there was no rollback path.
pub fn assign_synthetic_dead_slot_ids(
    role_pane_ids: &mut [Option<String>],
    cwd: &str,
    orchestration_name: &str,
) -> Vec<String> {
    let mut assigned = Vec::new();
    for (role_index, slot) in role_pane_ids.iter_mut().enumerate() {
        if slot.is_none() {
            let synthetic = dead_slot_pane_id(cwd, orchestration_name, role_index);
            assigned.push(synthetic.clone());
            *slot = Some(synthetic);
        }
    }
    assigned
}

/// Fill missing role slots in `role_pane_ids` with synthetic dead-slot
/// pane ids and insert a placeholder session into `state` for each one.
///
/// Symptom 2 (bug task `agent-card-lifecycle-bugs.md`): when an
/// orchestration role's daemon agent dies (e.g., a `clear = false`
/// release agent that runs through its workflow and exits cleanly),
/// `agent_records()` no longer returns it, the hydration bucket loses
/// that role's slot, and the role used to disappear from the
/// orchestration tab on reconnect. This helper bridges the gap: it
/// generates a stable synthetic id per missing role, inserts a
/// `agent_type = None` placeholder session keyed on it (so the dead
/// slot renders as "No agent" in the card grid), and returns the
/// fully-populated `role_pane_ids` to the caller. The synthetic id is
/// NOT a managed pane — `register_pane` is deliberately skipped, and
/// `apply_event` additionally rejects synthetic ids from its
/// auto-register branch so a forged hook event cannot promote one
/// into `managed_pane_ids`.
///
/// Production hydration no longer calls this convenience directly — it
/// uses [`assign_synthetic_dead_slot_ids`] and then seeds placeholder
/// sessions only on the `Ok` branch of `open_orchestration_tab_*` (see
/// CodeRabbit PR #118 finding #3). Tests still use this helper because
/// they exercise the synthetic-id + placeholder shape together.
pub fn fill_dead_slots_with_placeholders(
    role_pane_ids: &mut [Option<String>],
    cwd: &str,
    orchestration_name: &str,
    state: &mut AppState,
) {
    let assigned = assign_synthetic_dead_slot_ids(role_pane_ids, cwd, orchestration_name);
    for synthetic in assigned {
        state.insert_placeholder_session(synthetic, Some(cwd.to_string()), None, None);
    }
}

// ---------------------------------------------------------------------------
// M5 (PRD #93 round-5): delegation dispatch lives in the daemon now.
//
// Earlier rounds had the TUI drain `AppState.delegate_events` /
// `AppState.work_done_events`, build a per-role prompt (file + one-liner),
// optionally restart `clear=true` worker panes, and route the prompt
// through the pane controller. The daemon's hook loop now writes that
// prompt directly into the worker pane's PTY (see
// [`crate::state::AppState::handle_delegate`] /
// [`crate::state::AppState::handle_work_done`]), so the TUI is out of the
// dispatch business.
//
// Two features that previously rode this path are deliberately not
// reimplemented daemon-side and are surfaced here as a follow-up: per-role
// `prompt_template` wrapping and `clear=true` pane restart on delegate.
// They depended on the TUI's `OrchestrationConfig`; pulling that into the
// daemon would re-introduce the cross-process config-load coupling the
// PRD #93 redesign aims to remove. They can be re-added as opt-ins once
// the new dispatch surface settles.
// ---------------------------------------------------------------------------

/// Process pending dispatches — inject prompt once the agent in the pane is ready.
fn process_pending_dispatches(
    ui: &mut UiState,
    pane: &Arc<dyn PaneController>,
    snapshot: &AppState,
) {
    ui.pending_dispatches.retain(|pd| {
        // Fast path: agent fired SessionStart (e.g., Claude Code).
        let agent_ready = snapshot.sessions.values().any(|s| {
            s.pane_id.as_deref() == Some(pd.pane_id.as_str()) && s.agent_type != AgentType::None
        });
        // Slow path: no SessionStart after 10 seconds (e.g., opencode).
        // The agent is likely running but hasn't signaled — inject anyway.
        let timeout_ready =
            !agent_ready && pd.created_at.elapsed() > std::time::Duration::from_secs(10);
        if agent_ready || timeout_ready {
            let _ = pane.write_to_pane(&pd.pane_id, &pd.prompt);
            return false;
        }
        // Hard timeout after 60 seconds — give up.
        if pd.created_at.elapsed() > std::time::Duration::from_secs(60) {
            tracing::warn!(pane_id = %pd.pane_id, "dispatch: timed out waiting for agent");
            return false;
        }
        true
    });
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
    /// PRD #76, M2.5: detach every stream-backed pane (sending an explicit
    /// `KIND_DETACH` frame so the daemon distinguishes voluntary detach
    /// from abrupt disconnect) and then exit. Local-PTY panes are torn
    /// down by the normal quit path — they can't survive process exit.
    DetachAndQuit,
    Focus,
    NewPane(NewPaneRequest),
    SendConfigGenPrompt {
        pane_id: String,
        cwd: String,
    },
    RequestConfigGen,
    /// PRD #92 F2 (PRD #18 follow-through): the user pressed `y` or `n` on a
    /// dashboard card whose status is `WaitingForInput`. The bool is the
    /// approve/deny choice (true = approve). The dispatcher forwards a single
    /// `y` or `n` character to the selected pane's PTY via `write_to_pane`,
    /// which handles the submit-key parity dance (encode → wait `SUBMIT_DELAY`
    /// → CR), so the agent sees the same input it would have seen if the user
    /// had switched into the pane and typed it directly. The status-gating
    /// happens in `handle_normal_key` so this variant is only emitted when
    /// the response is actually warranted.
    SendPermissionResponse(bool),
    /// PRD #92 F1: the user picked **Stop** in the QuitConfirm dialog AND
    /// at least one managed agent is alive. The dispatcher must not
    /// terminate yet — it transitions the TUI into the secondary y/n
    /// confirmation dialog (`UiMode::StopConfirm`), seeded with the
    /// `agent_count` captured at the dialog-open moment so the secondary
    /// dialog's text can name a stable number even if the registry
    /// changes mid-dialog.
    StopConfirmPrompt {
        agent_count: usize,
    },
    /// PRD #92 F1: the user has finalised Stop — either the primary dialog
    /// confirmed it directly (no agents to warn about) or the secondary
    /// y/n dialog selected Yes. The dispatcher saves session state, sends
    /// `KIND_SHUTDOWN` to the daemon (which terminates every managed
    /// agent and exits), and breaks the TUI's main loop.
    StopAndQuit,
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
/// coordinates. Must match the viewport calculation in `terminal_widget.rs`.
fn screen_row_offset(screen: &vt100::Screen, pane_rect: Rect) -> u16 {
    let inner_h = pane_rect.height.saturating_sub(2) as usize;
    let screen_rows = screen.size().0 as usize;
    let cursor_row = screen.cursor_position().0 as usize;
    let anchor = (cursor_row + 1).min(screen_rows);
    let effective_rows = anchor.max(inner_h);
    effective_rows.saturating_sub(inner_h) as u16
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

fn handle_quit_confirm_key(
    key: KeyEvent,
    ui: &mut UiState,
    managed_agents_count: usize,
) -> KeyResult {
    // PRD #93 Phase 2 / M4.2: dialog was Detach/Cancel — every pane is
    // daemon-backed so quitting the TUI was always a detach, never a kill.
    // PRD #92 F1: Stop joins the dialog as a third option. Order is
    // Detach (0, default) / Stop (1) / Cancel (2). Detach stays the
    // default so the existing muscle memory does not become destructive.
    const QUIT_OPTION_COUNT: usize = 3;
    match key.code {
        // Ctrl+C from inside the dialog: skip the explicit KIND_DETACH
        // frame and just exit. The daemon treats the socket close as
        // implicit detach, so agents still survive — the difference vs
        // DetachAndQuit is purely about the daemon-side log
        // distinguishing voluntary detach from EOF.
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyResult::Quit,
        KeyCode::Up | KeyCode::Char('k') => {
            ui.quit_confirm_selected = ui.quit_confirm_selected.saturating_sub(1);
            KeyResult::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if ui.quit_confirm_selected + 1 < QUIT_OPTION_COUNT {
                ui.quit_confirm_selected += 1;
            }
            KeyResult::Continue
        }
        KeyCode::Enter => match ui.quit_confirm_selected {
            0 => KeyResult::DetachAndQuit,
            1 => {
                // PRD #92 F1 Stop: if no managed agents, proceed directly;
                // otherwise step through the secondary y/n dialog so the
                // user has to confirm the destructive action explicitly.
                if managed_agents_count == 0 {
                    KeyResult::StopAndQuit
                } else {
                    KeyResult::StopConfirmPrompt {
                        agent_count: managed_agents_count,
                    }
                }
            }
            _ => {
                ui.mode = UiMode::Normal;
                KeyResult::Continue
            }
        },
        KeyCode::Esc => {
            ui.mode = UiMode::Normal;
            KeyResult::Continue
        }
        _ => KeyResult::Continue,
    }
}

/// PRD #92 F1: secondary y/n confirmation when the user picks Stop in
/// the QuitConfirm dialog AND there is at least one managed agent that
/// would be terminated. Default selection is No (the safer option for a
/// destructive choice). On Yes the dispatcher proceeds to
/// `StopAndQuit`; on No the dialog returns to the primary QuitConfirm
/// with Stop still highlighted so the user can pick a different option
/// without restarting the Ctrl+C sequence. Layout: index 0 = No,
/// index 1 = Yes.
fn handle_stop_confirm_key(key: KeyEvent, ui: &mut UiState) -> KeyResult {
    const STOP_OPTION_COUNT: usize = 2;
    match key.code {
        // Ctrl+C inside the secondary dialog: same hard-quit semantic as
        // the primary dialog. Daemon sees the implicit detach.
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyResult::Quit,
        KeyCode::Up | KeyCode::Char('k') => {
            ui.stop_confirm_selected = ui.stop_confirm_selected.saturating_sub(1);
            KeyResult::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if ui.stop_confirm_selected + 1 < STOP_OPTION_COUNT {
                ui.stop_confirm_selected += 1;
            }
            KeyResult::Continue
        }
        // `y` is a shortcut for Yes regardless of which option is
        // highlighted — matches the convention from similar dialogs.
        KeyCode::Char('y') | KeyCode::Char('Y') => KeyResult::StopAndQuit,
        // `n` is a shortcut for No: return to the primary dialog with
        // Stop selected, so the user can pick Detach or Cancel without
        // re-opening anything.
        KeyCode::Char('n') | KeyCode::Char('N') => {
            ui.stop_confirm_selected = 0;
            ui.mode = UiMode::QuitConfirm;
            KeyResult::Continue
        }
        KeyCode::Enter => match ui.stop_confirm_selected {
            // Index 1 == Yes
            1 => KeyResult::StopAndQuit,
            // Index 0 == No: return to primary dialog (Stop still highlighted).
            _ => {
                ui.stop_confirm_selected = 0;
                ui.mode = UiMode::QuitConfirm;
                KeyResult::Continue
            }
        },
        KeyCode::Esc => {
            ui.stop_confirm_selected = 0;
            ui.mode = UiMode::QuitConfirm;
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
                    // PRD #76 M2.15 fixup F2: the post-focus resize sweep is
                    // performed by the caller (which has access to
                    // `tab_manager` + the real `frame_area`) via
                    // `resize_dashboard_panes`, the same SSOT helper used
                    // at spawn and tab-switch time. The inline `crossterm::
                    // terminal::size()` + hardcoded `(term_w * 67 / 100)` /
                    // `saturating_sub(3)` math that used to live here
                    // diverged from the helpers (no orchestration `66%`
                    // branch, no `show_tab_bar` chrome accounting) so
                    // stacked / orchestration focuses produced
                    // off-by-one-column PTYs until the next user-triggered
                    // resize.
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

/// PRD #68: mirror a selection move made by `handle_normal_key`
/// (j/k/Up/Down) into the embedded controller's focus. The per-frame
/// "sync `ui.selected_index` ← focused pane" block at the top of the
/// outer event loop would otherwise roll the move back on the next
/// iteration. The digit-jump path (`Ctrl+d` → `1`-`9`) already pairs
/// selection with focus via [`focus_deck`]; this helper is its
/// counterpart for the cycling keys. No-op when the index didn't
/// change or the new selection lacks a `pane_id`.
fn mirror_selection_into_focus(
    prev_selected_index: usize,
    ui: &UiState,
    filtered: &[(&String, &SessionState)],
    pane: &dyn PaneController,
) {
    if ui.selected_index != prev_selected_index
        && let Some((_, session)) = filtered.get(ui.selected_index)
        && let Some(pane_id) = session.pane_id.as_ref()
    {
        let _ = pane.focus_pane(pane_id);
    }
}

/// PRD #68 / PR #123 Greptile follow-up: one full Normal-mode key
/// dispatch — `handle_normal_key` plus the focus-mirror that makes
/// j/k/Up/Down actually land. `run_tui`'s `UiMode::Normal` arm and
/// `jk_navigation_mirrors_selection_into_focus` both call this
/// helper, so the test exercises the exact production path; deleting
/// the mirror line from this function would fail the test instead of
/// silently regressing the feature. Inlining either step back into
/// `run_tui` would defeat that — keep the call site in `run_tui` a
/// one-liner to this helper.
fn dispatch_normal_mode_key(
    key: KeyEvent,
    ui: &mut UiState,
    total: usize,
    selected_status: Option<SessionStatus>,
    filtered: &[(&String, &SessionState)],
    pane: &dyn PaneController,
) -> KeyResult {
    let prev_selected_index = ui.selected_index;
    let result = handle_normal_key(key, ui, total, selected_status);
    mirror_selection_into_focus(prev_selected_index, ui, filtered, pane);
    result
}

fn handle_normal_key(
    key: KeyEvent,
    ui: &mut UiState,
    total: usize,
    selected_status: Option<SessionStatus>,
) -> KeyResult {
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
        // PRD #92 F2 (PRD #18 follow-through): y / n approve / deny permission.
        // Only fires when the selected card is in `WaitingForInput` — any
        // other status, or no card selected, no-ops silently so y / n don't
        // accidentally clobber some future keybinding. `KeyModifiers::NONE`
        // is required so Ctrl+n (new pane, handled in the outer dispatch
        // loop) still wins.
        KeyCode::Char('y')
            if total > 0
                && key.modifiers == KeyModifiers::NONE
                && selected_status == Some(SessionStatus::WaitingForInput) =>
        {
            KeyResult::SendPermissionResponse(true)
        }
        KeyCode::Char('n')
            if total > 0
                && key.modifiers == KeyModifiers::NONE
                && selected_status == Some(SessionStatus::WaitingForInput) =>
        {
            KeyResult::SendPermissionResponse(false)
        }
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

/// Decide what value to push to the daemon on a Rename-mode keypress.
/// Returns `Some(text)` only on Enter; the caller then forwards `text` to
/// `PaneController::rename_pane`. An empty/whitespace-only `text` means
/// "clear" — `rename_pane` maps it to a daemon-side `None` so hydrate
/// falls back to the agent_id on reconnect rather than restoring a stale
/// label. Returns `None` for any non-Enter key (typing, Esc, backspace),
/// so the daemon isn't pinged on every keystroke.
///
/// Pulled out of the inline match arm in `run_app` so the empty-Enter
/// clear path stays unit-testable without a full TUI harness (PRD #76
/// M2.11 reviewer P1 & P2.4).
fn rename_commit_value(key: KeyEvent, rename_text: &str) -> Option<String> {
    matches!(key.code, KeyCode::Enter).then(|| rename_text.to_string())
}

/// Apply a [`RenameOutcome`] returned by `PaneController::rename_pane`
/// to the dashboard's two display-name maps (`display_names` keyed
/// by session_id, `pane_display_names` keyed by pane_id). Pulled out
/// of the inline match in `run_app` so the mirroring rules are
/// directly unit-testable without a full TUI harness — the
/// dashboard rename path's correctness is now `controller-outcome
/// → these maps`, with no separate UI-side normalization that could
/// drift from the controller (PRD #76 M2.11 fixup 5).
///
/// Semantics:
/// * `Applied(name)` — insert the trimmed canonical label into
///   both maps so the dashboard card title and any later
///   session-restart restore reflect EXACTLY what the controller
///   stored on `Pane.name` (and queued for the daemon).
/// * `Cleared` — remove the entry from both maps so the card
///   falls back to the agent_id-based default; matches the
///   controller's `display_name: None` clear on the daemon side.
/// * `Rejected` — leave both maps untouched; the prior label
///   stays visible because the controller refused to mutate
///   anything.
fn apply_rename_outcome(
    display_names: &mut HashMap<String, String>,
    pane_display_names: &mut HashMap<String, String>,
    session_id: &str,
    pane_id: &str,
    outcome: RenameOutcome,
) {
    match outcome {
        RenameOutcome::Applied(name) => {
            display_names.insert(session_id.to_string(), name.clone());
            pane_display_names.insert(pane_id.to_string(), name);
        }
        RenameOutcome::Cleared => {
            display_names.remove(session_id);
            pane_display_names.remove(pane_id);
        }
        RenameOutcome::Rejected => {
            // No-op by design. Re-asserting the prior label would
            // require a redundant clone; the maps already hold it.
        }
    }
}

fn handle_rename_key(
    key: KeyEvent,
    ui: &mut UiState,
    _selected_session_id: Option<&str>,
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
        // M2.11 fixup 5 — handler no longer writes to ui.display_names.
        // The dashboard dispatch loop calls `pane.rename_pane` and
        // mirrors the controller-returned RenameOutcome into both
        // display-name maps so the UI reflects EXACTLY what the
        // controller stored (a `"  newname  "` rename lands as
        // `"newname"`; a control-byte rename leaves the existing
        // label intact). Writing the raw rename_text here would
        // re-introduce the divergence reviewer P2 / auditor LOW
        // flagged in fixup 4.
        KeyCode::Enter => {
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
            FormField::Name if form.command_visible() => {
                form.focused = FormField::Command;
            }
            // PRD #106: when the Command field is hidden (orchestration
            // selected), pressing Enter on Name submits — there's no later
            // field to advance to.
            FormField::Name | FormField::Command => {
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

/// Tear down every non-dashboard tab (mode + orchestration) and unregister
/// their pane IDs from `state`.
///
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

    // PRD #111: preferred landing tab after both the hydration block and the
    // `--continue` reconnect block have run. Defaults to dashboard (0); the
    // hydration block overwrites this to the first rebuilt orchestration tab
    // when one exists. Hoisted out of the embedded-pane scope so the
    // `continue_session` block below can honour it instead of unconditionally
    // snapping back to the dashboard — without the hoist, the second
    // `switch_to(0)` in the reconnect path would undo the orchestration
    // landing on every `--continue` reconnect.
    let mut preferred_start_tab: usize = 0;

    // PRD #76 M2.x / M2.11: in external-daemon mode the daemon may already
    // own live agents from a previous TUI session (the user ssh-disconnected
    // and reconnected via `dot-agent-deck connect`). Ask the daemon for its
    // agent list and rebuild stream-backed panes for each one before the
    // event loop starts so the dashboard shows the live sessions instead of
    // "No active sessions". `hydrate_from_daemon` is a no-op for the
    // in-process (`LocalDeck`) controller — the in-process daemon shares
    // the TUI's registry directly. Errors during list_agents/attach are
    // absorbed so a transient daemon hiccup doesn't block startup.
    //
    // M2.11: each hydrated record also carries the agent's `display_name`
    // and `cwd` as stored in the daemon-side registry, so renamed panes
    // and their working directories reappear with their organizational
    // metadata intact — no separate persistence file is required.
    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
        let hydrated = embedded.hydrate_from_daemon();
        for h in &hydrated {
            let mut st = state.blocking_write();
            st.register_pane(h.pane_id.clone());
            // Carry the daemon-recorded cwd through to the placeholder session
            // so the session card shows the right directory until the agent
            // emits its first SessionStart event.
            //
            // PRD #76 M2.13: also carry the daemon-recorded `agent_type`
            // through so the dashboard card renders the real agent label
            // (ClaudeCode / OpenCode) on reconnect instead of "No agent".
            // The daemon captured the value at spawn time from
            // `StartAgent.agent_type` (inferred from the command via
            // `AgentType::from_command`); a `None` here means either the
            // command wasn't recognized or an older daemon predated the
            // field, in which case the placeholder shows "No agent" until
            // the next `SessionStart` event — same legacy behavior.
            // PRD #110 followup: hydration mints the placeholder with the
            // daemon-recorded `agent_id` (carried via `HydratedPane.agent_id`)
            // so the strict-equality reuse guard in `apply_event` lets a
            // post-reconnect `SessionStart` from the same agent remap onto
            // the placeholder. Without this, the placeholder would carry
            // `agent_id=None` and a `SessionStart` with `Some(daemon-id)`
            // would not match → a second card would appear beside the
            // hydrated one.
            st.insert_placeholder_session(
                h.pane_id.clone(),
                h.cwd.clone(),
                h.agent_type.clone(),
                Some(h.agent_id.clone()),
            );
            drop(st);
            let display_name = h.display_name.clone().unwrap_or_else(|| h.agent_id.clone());
            ui.pane_display_names
                .insert(h.pane_id.clone(), display_name.clone());
            ui.pane_names.insert(h.pane_id.clone(), display_name);
            if let Some(dir) = h.cwd.clone() {
                // Mode buckets need a `SavedPane.mode` hint for the
                // existing tab-restore path elsewhere in the UI; the
                // hydration dispatcher below also reads this map for
                // mode tab rebuild, so populate `.mode` from
                // tab_membership rather than leaving it None.
                let mode_hint = match &h.tab_membership {
                    Some(TabMembership::Mode { name }) => Some(name.clone()),
                    _ => None,
                };
                ui.pane_metadata.insert(
                    h.pane_id.clone(),
                    config::SavedPane {
                        dir,
                        name: ui
                            .pane_display_names
                            .get(&h.pane_id)
                            .cloned()
                            .unwrap_or_else(|| h.agent_id.clone()),
                        command: String::new(),
                        mode: mode_hint,
                    },
                );
            }
        }

        // PRD #76 M2.12: partition hydrated panes by tab_membership and
        // rebuild mode/orchestration tabs from the project config. Panes
        // claiming a mode/orchestration that the cwd's project config
        // doesn't know about (config-drift case from design decision 5)
        // get logged loudly and left on the dashboard rather than getting
        // a limbo tab.
        let partition = partition_hydrated_panes(&hydrated);
        // Emit deferred diagnostics from the pure partition helper
        // (M2.12 fixup reviewer #3: partition stays I/O-free; logging
        // lives at the hydration call site).
        for rejection in &partition.rejections {
            match rejection {
                HydrationRejection::DuplicateMode {
                    cwd,
                    mode_name,
                    agent_id,
                    pane_id,
                } => {
                    tracing::error!(
                        cwd = %cwd,
                        mode = %mode_name,
                        agent_id = %agent_id,
                        pane_id = %pane_id,
                        "hydration: duplicate Mode tab_membership for (cwd, name); dropping to dashboard"
                    );
                }
            }
        }
        // Cache cwd → project config so the lookup happens once per
        // distinct cwd regardless of how many buckets share it.
        let mut config_cache: std::collections::HashMap<
            String,
            Option<crate::project_config::ProjectConfig>,
        > = std::collections::HashMap::new();
        let lookup_config = |cache: &mut std::collections::HashMap<
            String,
            Option<crate::project_config::ProjectConfig>,
        >,
                             cwd: &str|
         -> Option<crate::project_config::ProjectConfig> {
            if let Some(cached) = cache.get(cwd) {
                return cached.clone();
            }
            let loaded = match load_project_config(std::path::Path::new(cwd)) {
                Ok(opt) => opt,
                Err(e) => {
                    tracing::error!(
                        cwd = %cwd,
                        error = %e,
                        "hydration: failed to load project config; dropping mode/orchestration tabs to dashboard"
                    );
                    None
                }
            };
            cache.insert(cwd.to_string(), loaded.clone());
            loaded
        };

        // PRD #76 M2.15 fixup pass 2 G1 — compute side-pane dims for
        // hydration mode-tab rebuilds. Side panes spawn fresh from the
        // project config (they're not daemon-tracked), so they need
        // viewport-aware dims at create time. The post-loop hydration
        // resize sweep below also runs, but spawning at the right size
        // avoids the 24×80 hiccup.
        let hydration_frame_area = terminal.get_frame().area();
        for bucket in &partition.mode_buckets {
            let cfg = lookup_config(&mut config_cache, &bucket.cwd);
            let mode_config = cfg
                .as_ref()
                .and_then(|c| c.modes.iter().find(|m| m.name == bucket.mode_name).cloned());
            let Some(mode_config) = mode_config else {
                tracing::error!(
                    cwd = %bucket.cwd,
                    mode = %bucket.mode_name,
                    agent_pane_id = %bucket.agent_pane_id,
                    "hydration: hydrated agent claims mode that is not in project config; dropping to dashboard"
                );
                continue;
            };
            let total_side_count = (mode_config.panes.len() + mode_config.reactive_panes) as u16;
            let side_pane_dims = mode_side_pane_dims(hydration_frame_area, total_side_count);
            match tab_manager.open_mode_tab_with_existing_agent_pane(
                &mode_config,
                &bucket.cwd,
                bucket.agent_pane_id.clone(),
                side_pane_dims,
            ) {
                Ok((_idx, side_ids)) => {
                    for id in &side_ids {
                        state.blocking_write().register_pane(id.clone());
                    }
                }
                Err(e) => {
                    tracing::error!(
                        cwd = %bucket.cwd,
                        mode = %bucket.mode_name,
                        error = %e,
                        "hydration: failed to rebuild mode tab; agent pane stays on dashboard"
                    );
                }
            }
        }

        // PRD #111: remember the first successfully-rebuilt orchestration
        // tab so the post-loop active-tab snap-back can land on it
        // instead of the dashboard for remote reconnects (where the
        // dashboard would otherwise be the only tab the user sees,
        // hiding their work).
        let mut first_orchestration_tab_index: Option<usize> = None;
        for bucket in &partition.orchestration_buckets {
            let cfg = lookup_config(&mut config_cache, &bucket.cwd);
            let local_orch_config = cfg.as_ref().and_then(|c| {
                c.orchestrations
                    .iter()
                    .find(|o| o.name == bucket.orchestration_name)
                    .cloned()
            });
            // PRD #111: when the local project config file can't be
            // resolved (laptop TUI reconnecting to a VM daemon whose
            // `bucket.cwd` doesn't exist locally) or the local file
            // doesn't carry an orchestration with this name (drift),
            // synthesise a minimal `OrchestrationConfig` from the
            // daemon-supplied bucket metadata. The synthesised config
            // is structurally correct (same name, same role count,
            // same role names, same start role) — only display-only
            // enrichment fields (description, prompt_template) are
            // missing. Without this fallback, every remote-reconnect
            // user would see their orchestration panes dumped into the
            // dashboard.
            if local_orch_config.is_none() {
                // PRD #111 auditor nit: distinguish the two
                // "synthesise" cases so operators can tell whether
                // the file is genuinely absent (legitimate remote
                // reconnect — `cfg.is_none()`) or present but
                // missing this orchestration (config drift —
                // `cfg.is_some()`). Same level (info) for both;
                // distinct messages so log search picks them apart.
                if cfg.is_none() {
                    tracing::info!(
                        cwd = %bucket.cwd,
                        orchestration = %bucket.orchestration_name,
                        role_count = bucket.role_slots.len(),
                        "hydration: rebuilding orchestration tab from synthesised config (local .dot-agent-deck.toml absent — remote daemon path)"
                    );
                } else {
                    tracing::info!(
                        cwd = %bucket.cwd,
                        orchestration = %bucket.orchestration_name,
                        role_count = bucket.role_slots.len(),
                        "hydration: rebuilding orchestration tab from synthesised config (local config exists but does not list this orchestration — config drift or stale)"
                    );
                }
            }
            let orch_config = resolve_orch_config_for_hydration(local_orch_config, bucket);
            // Build a Vec<Option<String>> of length config.roles.len()
            // from the role-slot entries. Out-of-range indices are
            // config drift (design decision 5): log loudly and leave
            // the pane on the dashboard.
            let mut role_pane_ids: Vec<Option<String>> = vec![None; orch_config.roles.len()];
            for slot in &bucket.role_slots {
                let role_index = slot.role_index;
                let pane_id = &slot.pane_id;
                if role_index >= orch_config.roles.len() {
                    tracing::error!(
                        cwd = %bucket.cwd,
                        orchestration = %bucket.orchestration_name,
                        role_index = role_index,
                        role_count = orch_config.roles.len(),
                        pane_id = %pane_id,
                        "hydration: orchestration role_index out of range; dropping pane to dashboard"
                    );
                    continue;
                }
                if role_pane_ids[role_index].is_some() {
                    tracing::error!(
                        cwd = %bucket.cwd,
                        orchestration = %bucket.orchestration_name,
                        role_index = role_index,
                        pane_id = %pane_id,
                        "hydration: duplicate role_index in orchestration bucket; keeping first, dropping rest to dashboard"
                    );
                    continue;
                }
                role_pane_ids[role_index] = Some(pane_id.clone());
            }
            // M2.12 fixup auditor #2: if every claimed role slot was
            // rejected above (out-of-range / duplicate), don't rebuild
            // an empty orchestration tab — the user didn't ask for one,
            // and it would only show dead frames. Drop the whole
            // bucket; the rejected panes already routed themselves to
            // the dashboard via the logs above.
            if role_pane_ids.iter().all(Option::is_none) {
                tracing::warn!(
                    cwd = %bucket.cwd,
                    orchestration = %bucket.orchestration_name,
                    role_count = orch_config.roles.len(),
                    "hydration: skipping orchestration tab rebuild — no role slots survived filtering"
                );
                continue;
            }
            // Symptom 2 fix (`.dot-agent-deck/agent-card-lifecycle-bugs.md`):
            // a role whose daemon agent died (most commonly a
            // `clear = false` role that finishes its workflow and
            // exits cleanly) is absent from `agent_records()` and
            // therefore absent from `bucket.role_slots`. Pre-fix the
            // role just disappeared from the orchestration tab on
            // reconnect — the user lost the slot entirely. Assign a
            // synthetic dead-slot pane id to every remaining `None`
            // slot so every role in the config keeps its card on the
            // rebuilt tab, even though only the live ones have
            // backing PTYs on the right.
            //
            // CodeRabbit PR #118 finding #3: split assignment from
            // placeholder-session seeding. Synthetic ids go into
            // `role_pane_ids` first so they're reflected in the
            // `role_statuses` the tab-open call computes (dead slots
            // classify as `Failed`). Placeholder sessions are only
            // inserted into `AppState` once the tab open succeeds —
            // see the matching `Ok` branch below. On `Err`, no
            // placeholder sessions are seeded, so the `Err` arm has
            // nothing to clean up.
            let dead_slot_synthetic_ids = assign_synthetic_dead_slot_ids(
                &mut role_pane_ids,
                &bucket.cwd,
                &bucket.orchestration_name,
            );
            // Now register the orchestrator pane mapping for any live
            // start role so M5 dispatch keeps routing work-done events
            // back to the right place.
            let start_role_index = orch_config.roles.iter().position(|r| r.start).unwrap_or(0);
            let orchestrator_pane = role_pane_ids.get(start_role_index).and_then(|s| s.clone());
            match tab_manager.open_orchestration_tab_with_existing_role_panes(
                &orch_config,
                &bucket.cwd,
                role_pane_ids.clone(),
            ) {
                Ok((tab_index, _)) => {
                    if first_orchestration_tab_index.is_none() {
                        first_orchestration_tab_index = Some(tab_index);
                    }
                    let mut st = state.blocking_write();
                    // CodeRabbit PR #118 finding #3: seed placeholder
                    // sessions for dead slots only after the tab has
                    // been built. If `open_orchestration_tab_*`
                    // returned `Err`, we'd skip this loop entirely and
                    // never orphan placeholder sessions in `AppState`.
                    for synthetic in &dead_slot_synthetic_ids {
                        st.insert_placeholder_session(
                            synthetic.clone(),
                            Some(bucket.cwd.clone()),
                            None,
                            None,
                        );
                    }
                    for (i, role) in orch_config.roles.iter().enumerate() {
                        if let Some(Some(pane_id)) = role_pane_ids.get(i) {
                            // Symptom 2 fix
                            // (`.dot-agent-deck/agent-card-lifecycle-bugs.md`):
                            // dead-slot synthetics are placeholder
                            // sessions only — there is no daemon-side
                            // pane to delegate to, so don't pollute
                            // `pane_role_map` (which `handle_delegate`
                            // queries to find a target pane) with
                            // entries pointing at non-existent panes.
                            if is_dead_slot_pane_id(pane_id) {
                                continue;
                            }
                            st.pane_role_map.insert(pane_id.clone(), role.name.clone());
                            st.pane_cwd_map.insert(pane_id.clone(), bucket.cwd.clone());
                            if role.start {
                                st.orchestrator_pane_ids.insert(pane_id.clone());
                            }
                        }
                    }
                    drop(st);
                    if let Some(pane_id) = orchestrator_pane {
                        let _ = pane_id; // silence "unused" if no further uses below
                    }
                }
                Err(e) => {
                    tracing::error!(
                        cwd = %bucket.cwd,
                        orchestration = %bucket.orchestration_name,
                        error = %e,
                        "hydration: failed to rebuild orchestration tab; role panes stay on dashboard"
                    );
                    // CodeRabbit PR #118 finding #3: nothing to roll
                    // back. Placeholder sessions for the synthetic
                    // ids in `dead_slot_synthetic_ids` were
                    // deliberately NOT inserted into `AppState`
                    // (that step lives in the `Ok` arm above), so
                    // there are no orphaned sessions to clean up.
                }
            }
        }
        // PRD #111: after hydration, decide the landing tab. If at
        // least one orchestration tab was rebuilt, land on the first
        // one so a reconnect (especially the remote-config case where
        // the dashboard would otherwise show nothing relevant) lands
        // the user back in their work. Otherwise — pure dashboard
        // session, mode-only session, or every orchestration rebuild
        // failed — fall back to the dashboard so the user gets the
        // overview first (pre-PRD-111 behaviour). The chosen index is
        // also recorded in `preferred_start_tab` so the
        // `continue_session` block below doesn't snap back to the
        // dashboard on `--continue` reconnects (CodeRabbit PR #114).
        preferred_start_tab = first_orchestration_tab_index.unwrap_or(0);
        tab_manager.switch_to(preferred_start_tab);

        // PRD #76 M2.15: push the real viewport dims to every hydrated
        // pane's daemon-side PTY. `hydrate_from_daemon` rebuilt panes from
        // the daemon's existing PTYs (at whatever dims the previous TUI
        // session resized them to) and the local vt100 parsers were seeded
        // at 24×80. Without this sweep, the agent keeps drawing at the
        // previous viewport size — often visibly mismatched when the new
        // session has a different terminal size — until the user's next
        // window resize or tab switch fires `resize_dashboard_panes` /
        // `resize_mode_tab_panes`. One pass here covers it.
        //
        // We need `terminal.autoresize()` first so `get_frame().area()`
        // reflects the current terminal, not stale init defaults (no
        // `draw()` has happened yet at this point in startup).
        //
        // Each tab gets the inner helper that matches its layout, not
        // the active-tab-only public wrappers — at this point the
        // dashboard is active, so a naive `resize_mode_tab_panes` call
        // would early-return and leave every mode/orchestration tab's
        // panes at the wrong dims until the user navigates to them.
        let _ = terminal.autoresize();
        let frame_area = terminal.get_frame().area();
        resize_dashboard_panes(&*pane, &ui, &tab_manager, frame_area);
        let show_tab_bar = tab_manager.show_tab_bar();
        for tab in tab_manager.tabs() {
            match tab {
                Tab::Mode {
                    agent_pane_id,
                    mode_manager,
                    ..
                } => {
                    resize_mode_tab_panes_for(
                        &*pane,
                        agent_pane_id,
                        &mode_manager.managed_pane_ids(),
                        frame_area,
                    );
                }
                Tab::Orchestration { role_pane_ids, .. } => {
                    resize_orchestration_role_panes_for(
                        &*pane,
                        role_pane_ids,
                        frame_area,
                        show_tab_bar,
                    );
                }
                Tab::Dashboard => {}
            }
        }
    }

    if continue_session {
        // Ensure the terminal has up-to-date dimensions before we resize
        // any PTYs — without this, get_frame().area() may return stale or
        // default values because no draw() call has happened yet.
        let _ = terminal.autoresize();

        let saved = config::SavedSession::load();
        // CodeRabbit round-9 #4 / round-10 #2: dedupe against panes
        // the daemon already hydrated. See `build_dedupe_budget` and
        // `try_consume_dedupe_slot` below for the algorithm.
        let mut remaining_dedupe_budget = build_dedupe_budget(&ui.pane_metadata);
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
            if try_consume_dedupe_slot(&mut remaining_dedupe_budget, saved_pane) {
                tracing::debug!(
                    dir = %saved_pane.dir,
                    name = %saved_pane.name,
                    mode = ?saved_pane.mode,
                    "restore: skipping saved pane — already hydrated from daemon"
                );
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
            // PRD #76 M2.15 fixup F1: route through `create_pane_with_options`
            // with real viewport dims so the restored pane's daemon-side PTY
            // opens at the dashboard layout instead of the legacy 24×80
            // bleed-through from `AgentSpawnOptions::default()`. The
            // post-loop `resize_dashboard_panes` sweep still runs to
            // reconcile any rounding once every restored pane is in place.
            let (rows, cols) =
                dashboard_restore_pane_dims(&*pane, &tab_manager, terminal.get_frame().area());
            // PRD #76 M2.13: infer agent_type from the restored command so
            // the daemon's registry echo on reconnect carries the right
            // type. Local-mode session card pick-up still happens via the
            // next `SessionStart` hook.
            match pane.create_pane_with_options(
                cmd,
                Some(&saved_pane.dir),
                AgentSpawnOptions {
                    display_name: None,
                    tab_membership: None,
                    rows,
                    cols,
                    agent_type: AgentType::from_command(cmd),
                },
            ) {
                Ok((new_id, _resolved)) => {
                    // PRD #110 followup: snapshot the daemon-assigned
                    // `agent_id` so the placeholder is born with it and
                    // the strict-equality reuse guard accepts the
                    // freshly-spawned agent's first `SessionStart`. The
                    // lookup happens outside the `state` write lock —
                    // `pane_agent_id` only touches the controller's panes
                    // mutex.
                    let new_agent_id = pane.pane_agent_id(&new_id);
                    {
                        let mut st = state.blocking_write();
                        st.register_pane(new_id.clone());
                        // PRD #76 M2.13: the daemon-bound spawn above tags
                        // the registry entry with the inferred agent type so
                        // a later reconnect's hydration knows it; the local
                        // placeholder stays at `None` until the next
                        // `SessionStart` hook fires (pre-M2.13 contract).
                        st.insert_placeholder_session(
                            new_id.clone(),
                            Some(saved_pane.dir.clone()),
                            None,
                            new_agent_id,
                        );
                    }
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
            // M2.12: tag the daemon-side agent with this mode tab's
            // membership so the next reconnect can rebuild this tab
            // from `list_agents` rather than dropping the agent to the
            // dashboard. `create_pane_with_options` is the only path
            // that reaches `StartAgent.tab_membership` on the wire.
            let mode_tab_membership = Some(TabMembership::Mode {
                name: mode_config.name.clone(),
            });
            // PRD #76 M2.15: open the daemon-side PTY at the mode-tab agent
            // layout (left half × full height minus chrome) so the agent's
            // first frame paints at the eventual size. The resize call
            // below this match arm still runs and reconciles any rounding;
            // this just removes the visible 24×80 hiccup.
            let frame_area = terminal.get_frame().area();
            let (rows, cols) = mode_agent_pane_dims(frame_area);
            // PRD #76 M2.13: mode-tab agent panes spawn as empty shells
            // (the agent command is sent later via `write_to_pane`), so
            // infer agent_type from the saved command rather than from
            // the spawn command (which is `None` here).
            let mode_agent_type = if saved_pane.command.is_empty() {
                None
            } else {
                AgentType::from_command(Some(saved_pane.command.as_str()))
            };
            match pane.create_pane_with_options(
                None,
                Some(&saved_pane.dir),
                AgentSpawnOptions {
                    display_name: None,
                    tab_membership: mode_tab_membership,
                    rows,
                    cols,
                    agent_type: mode_agent_type,
                },
            ) {
                Ok((new_id, _resolved)) => {
                    state.blocking_write().register_pane(new_id.clone());
                    if !saved_pane.name.is_empty() {
                        let _ = pane.rename_pane(&new_id, &saved_pane.name);
                        ui.pane_display_names
                            .insert(new_id.clone(), saved_pane.name.clone());
                        ui.pane_names
                            .insert(new_id.clone(), saved_pane.name.clone());
                    }
                    ui.pane_metadata.insert(new_id.clone(), saved_pane.clone());
                    // PRD #76 M2.15 fixup pass 2 G1 — compute side-pane
                    // dims so the restored mode's side panes spawn at the
                    // viewport-derived size, not the 24×80 default.
                    let total_side_count =
                        (mode_config.panes.len() + mode_config.reactive_panes) as u16;
                    let side_pane_dims = mode_side_pane_dims(frame_area, total_side_count);
                    match tab_manager.open_mode_tab(
                        &mode_config,
                        &saved_pane.dir,
                        new_id.clone(),
                        side_pane_dims,
                    ) {
                        Ok((_tab_idx, side_ids)) => {
                            for id in &side_ids {
                                state.blocking_write().register_pane(id.clone());
                            }
                            // PRD #76 M2.15: route through the shared
                            // helpers so spawn-time and resize-time use the
                            // same layout math.
                            let frame_area = terminal.get_frame().area();
                            resize_mode_tab_panes_for(&*pane, &new_id, &side_ids, frame_area);
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
                            state.blocking_write().unregister_pane(&new_id);
                            ui.pane_metadata.remove(&new_id);
                            ui.pane_display_names.remove(&new_id);
                            ui.pane_names.remove(&new_id);
                            ui.session_warnings.push(format!(
                                "Warning: failed to restore mode '{}': {e}",
                                mode_config.name
                            ));
                            // Fallback: plain dashboard pane so the user still
                            // gets a usable pane (PRD #69 acceptance criterion).
                            let cmd = if saved_pane.command.is_empty() {
                                None
                            } else {
                                Some(saved_pane.command.as_str())
                            };
                            // PRD #76 M2.15 fixup F1: real viewport dims via
                            // SSOT helper so the fallback dashboard pane
                            // opens at the dashboard layout, not 24×80.
                            let (fb_rows, fb_cols) = dashboard_restore_pane_dims(
                                &*pane,
                                &tab_manager,
                                terminal.get_frame().area(),
                            );
                            // PRD #76 M2.13: infer agent_type from the
                            // saved command for the fallback path too.
                            let fb_agent_type = AgentType::from_command(cmd);
                            match pane.create_pane_with_options(
                                cmd,
                                Some(&saved_pane.dir),
                                AgentSpawnOptions {
                                    display_name: None,
                                    tab_membership: None,
                                    rows: fb_rows,
                                    cols: fb_cols,
                                    agent_type: fb_agent_type.clone(),
                                },
                            ) {
                                Ok((fb_id, _resolved)) => {
                                    // PRD #110 followup: see other create-
                                    // pane sites — placeholder needs the
                                    // daemon agent_id.
                                    let fb_agent_id = pane.pane_agent_id(&fb_id);
                                    {
                                        let mut st = state.blocking_write();
                                        st.register_pane(fb_id.clone());
                                        st.insert_placeholder_session(
                                            fb_id.clone(),
                                            Some(saved_pane.dir.clone()),
                                            None,
                                            fb_agent_id,
                                        );
                                    }
                                    if !saved_pane.name.is_empty() {
                                        let _ = pane.rename_pane(&fb_id, &saved_pane.name);
                                        ui.pane_display_names
                                            .insert(fb_id.clone(), saved_pane.name.clone());
                                        ui.pane_names
                                            .insert(fb_id.clone(), saved_pane.name.clone());
                                    }
                                    ui.pane_metadata.insert(fb_id, saved_pane.clone());
                                }
                                Err(fb_err) => {
                                    ui.session_warnings.push(format!(
                                        "Warning: also failed to create fallback plain pane for '{}': {fb_err}",
                                        saved_pane.name
                                    ));
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    ui.session_warnings.push(format!(
                        "Warning: failed to restore mode pane '{}': {e}",
                        saved_pane.name
                    ));
                    let cmd = if saved_pane.command.is_empty() {
                        None
                    } else {
                        Some(saved_pane.command.as_str())
                    };
                    // PRD #76 M2.15 fixup F1: real viewport dims via SSOT
                    // helper so the outer-error fallback also opens at the
                    // dashboard layout instead of 24×80.
                    let (fb_rows, fb_cols) = dashboard_restore_pane_dims(
                        &*pane,
                        &tab_manager,
                        terminal.get_frame().area(),
                    );
                    // PRD #76 M2.13: infer agent_type from saved command
                    // for this outer-error fallback as well.
                    let fb_agent_type = AgentType::from_command(cmd);
                    match pane.create_pane_with_options(
                        cmd,
                        Some(&saved_pane.dir),
                        AgentSpawnOptions {
                            display_name: None,
                            tab_membership: None,
                            rows: fb_rows,
                            cols: fb_cols,
                            agent_type: fb_agent_type.clone(),
                        },
                    ) {
                        Ok((fb_id, _resolved)) => {
                            // PRD #110 followup: outer-error fallback —
                            // same agent_id plumbing as the inner fallback.
                            let fb_agent_id = pane.pane_agent_id(&fb_id);
                            {
                                let mut st = state.blocking_write();
                                st.register_pane(fb_id.clone());
                                st.insert_placeholder_session(
                                    fb_id.clone(),
                                    Some(saved_pane.dir.clone()),
                                    None,
                                    fb_agent_id,
                                );
                            }
                            if !saved_pane.name.is_empty() {
                                let _ = pane.rename_pane(&fb_id, &saved_pane.name);
                                ui.pane_display_names
                                    .insert(fb_id.clone(), saved_pane.name.clone());
                                ui.pane_names.insert(fb_id.clone(), saved_pane.name.clone());
                            }
                            ui.pane_metadata.insert(fb_id, saved_pane.clone());
                        }
                        Err(fb_err) => {
                            ui.session_warnings.push(format!(
                                "Warning: also failed to create fallback plain pane for '{}': {fb_err}",
                                saved_pane.name
                            ));
                        }
                    }
                }
            }
        }
        // PRD #111 / CodeRabbit PR #114: land on the orchestration tab
        // chosen by the hydration block above (if any) instead of always
        // snapping back to the dashboard. Without the hoist, an
        // unconditional `switch_to(0)` here would undo the M3 fix for
        // users who reconnect with `--continue`. `preferred_start_tab`
        // defaults to 0 (dashboard) when the hydration block didn't run
        // or didn't rebuild any orchestration tab, preserving the prior
        // overview-first behaviour for non-orchestration sessions.
        tab_manager.switch_to(preferred_start_tab);

        // Resize all restored panes to match the terminal layout, focus the first,
        // and enter PaneInput mode so the user can type immediately.
        if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
            let frame_area = terminal.get_frame().area();
            resize_dashboard_panes(&*pane, &ui, &tab_manager, frame_area);
            let ids = embedded.pane_ids();
            if let Some(first_id) = ids.first() {
                let _ = pane.focus_pane(first_id);
                ui.mode = UiMode::PaneInput;
            }
        }
        ui.selected_index = 0;
    }

    'outer: loop {
        // Expire stale status messages
        if let Some((_, created)) = &ui.status_message
            && created.elapsed() > STATUS_MESSAGE_TTL
        {
            ui.status_message = None;
        }

        let snapshot = state.blocking_read().clone();

        // PRD #76 M2.15 fixup pass 2 G1 — refresh each Mode tab's cached
        // side-pane dims from the current frame area so the reactive
        // replacement spawn inside `ModeManager::handle_command` opens
        // the daemon-side PTY at the right size, not the legacy 24×80
        // default. `handle_command` is invoked from
        // `route_reactive_commands` below, which doesn't have
        // `frame_area` in scope — caching on the manager keeps the
        // routing API clean while still tracking viewport changes.
        {
            let frame_area = terminal.get_frame().area();
            for tab in tab_manager.tabs_mut() {
                if let Tab::Mode { mode_manager, .. } = tab {
                    let side_count = mode_manager.managed_pane_ids().len() as u16;
                    let dims = mode_side_pane_dims(frame_area, side_count);
                    mode_manager.set_side_pane_dims(dims);
                }
            }
        }

        // Route new Bash commands through mode tabs for reactive panes.
        let pane_changes = tab_manager.route_reactive_commands(&snapshot.sessions);
        for (old_id, new_id) in &pane_changes {
            let mut st = state.blocking_write();
            st.unregister_pane(old_id);
            st.register_pane(new_id.clone());
            drop(st);
            // Resize the new pane PTY to match the current side pane
            // dimensions. PRD #76 M2.15 fixup F2: route through the SSOT
            // helper instead of recomputing `(width/2).saturating_sub(2)`
            // / `height/side_count - 2` inline — the helper clamps
            // `side_count` to 1 so the previous `checked_div` fallback to
            // `height - 3` is no longer needed.
            if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
                let frame_area = terminal.get_frame().area();
                let side_pane_count = embedded.pane_ids().len().saturating_sub(1) as u16; // exclude agent
                let (pane_rows, half_width) = mode_side_pane_dims(frame_area, side_pane_count);
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
                // Follow-up to 0d5e651 (auditor finding #2): synthetic
                // dead-slot placeholders live on the orchestration tab
                // only. `all_managed_pane_ids` deliberately skips them
                // (close_tab can't `close_pane` a synthetic id), so we
                // must filter them out here too — otherwise the
                // "No agent" ghost card leaks onto the Dashboard.
                all_filtered
                    .into_iter()
                    .filter(|(_, s)| {
                        s.pane_id
                            .as_ref()
                            .is_none_or(|pid| !exclude.contains(pid) && !is_dead_slot_pane_id(pid))
                    })
                    .collect()
            }
            Tab::Orchestration { role_pane_ids, .. } => {
                let mut orch_filtered: Vec<_> = all_filtered
                    .into_iter()
                    .filter(|(_, s)| {
                        s.pane_id
                            .as_ref()
                            .is_some_and(|pid| role_pane_ids.contains(pid))
                    })
                    .collect();
                // Sort by role config order, not numeric pane ID, so recreated
                // panes (clear=true) keep their original card position.
                orch_filtered.sort_by_key(|(_, s)| {
                    s.pane_id
                        .as_ref()
                        .and_then(|pid| role_pane_ids.iter().position(|p| p == pid))
                        .unwrap_or(usize::MAX)
                });
                orch_filtered
            }
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
                Tab::Orchestration { name, status, .. } => match status {
                    OrchestrationStatus::Completed => format!("{name} [done]"),
                    OrchestrationStatus::Delegated => format!("{name} [active]"),
                    OrchestrationStatus::WaitingForOrchestrator => name.clone(),
                },
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
        // Fast path: Claude Code fires SessionStart immediately (agent_type != None).
        // Slow path: agents like opencode don't signal — fall back after 10 seconds.
        for tab in tab_manager.tabs_mut() {
            if let Tab::Orchestration {
                id,
                role_pane_ids,
                start_role_index,
                role_statuses,
                orchestrator_prompt,
                ..
            } = tab
                && orchestrator_prompt.is_some()
                && !ui.orchestration_prompted.contains(id)
            {
                let start_pane_id = &role_pane_ids[*start_role_index];
                let agent_ready = snapshot.sessions.values().any(|s| {
                    s.pane_id.as_deref() == Some(start_pane_id) && s.agent_type != AgentType::None
                });
                let timeout_ready = !agent_ready
                    && ui
                        .orchestration_created_at
                        .get(id)
                        .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(10));
                if agent_ready || timeout_ready {
                    if let Some(prompt) = orchestrator_prompt.take() {
                        let _ = pane.write_and_submit_to_pane(start_pane_id, &prompt);
                    }
                    role_statuses[*start_role_index] = OrchestrationRoleStatus::Working;
                    ui.orchestration_prompted.insert(*id);
                }
            }
        }

        // PRD #93 round-5: dispatch_delegate_events / feedback_worker_results
        // ran here in earlier rounds — they drained delegate/work-done
        // signals from `AppState` and wrote prompts via the pane controller.
        // That entire flow now lives daemon-side; the daemon writes the
        // file-backed prompt and the one-liner directly into the target
        // PTY, so the TUI just renders the bytes as they arrive in the
        // pane scrollback. `process_pending_dispatches` still ferries the
        // orchestrator's *initial* prompt across the agent-ready gate.
        process_pending_dispatches(&mut ui, &pane, &snapshot);

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
            if let Event::Resize(_w, _h) = ev {
                let frame_area = terminal.get_frame().area();
                resize_dashboard_panes(&*pane, &ui, &tab_manager, frame_area);
                resize_mode_tab_panes(&*pane, &tab_manager, frame_area);
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
                            if embedded.mouse_mode_enabled(&pane_id) {
                                let (col, row) = pane_relative_coords(
                                    mouse.column,
                                    mouse.row,
                                    &ui.focused_pane_rect,
                                );
                                let _ = embedded.forward_mouse_scroll(&pane_id, true, col, row);
                            } else {
                                embedded.scroll_pane(&pane_id, 3);
                            }
                        }
                        crossterm::event::MouseEventKind::ScrollDown
                            if ui.mode == UiMode::PaneInput =>
                        {
                            if embedded.mouse_mode_enabled(&pane_id) {
                                let (col, row) = pane_relative_coords(
                                    mouse.column,
                                    mouse.row,
                                    &ui.focused_pane_rect,
                                );
                                let _ = embedded.forward_mouse_scroll(&pane_id, false, col, row);
                            } else {
                                embedded.scroll_pane(&pane_id, -3);
                            }
                        }
                        crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        ) => {
                            // Ctrl+click opens hyperlinks.
                            let has_modifier = mouse
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL);
                            if has_modifier {
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
                                        let row = mouse.row - inner_y;
                                        if let Some(hmap_arc) = embedded.get_hyperlinks(&pane_id)
                                            && let Ok(hmap) = hmap_arc.lock()
                                            && let Some(screen_arc) = embedded.get_screen(&pane_id)
                                            && let Ok(parser) = screen_arc.lock()
                                        {
                                            let offset = screen_row_offset(parser.screen(), rect);
                                            let screen_row = row + offset;
                                            if let Some(url) = hmap.get_row(screen_row) {
                                                let url = url.to_string();
                                                drop(parser);
                                                drop(hmap);
                                                if open::that(&url).is_ok() {
                                                    let display = if url.len() > 60 {
                                                        format!("{}...", &url[..57])
                                                    } else {
                                                        url
                                                    };
                                                    ui.status_message = Some((
                                                        format!("Opened: {display}"),
                                                        std::time::Instant::now(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                            } else if let Some(rect) = ui.focused_pane_rect {
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
                    // PRD #76 M2.20: a paste is a forwarded keystroke event
                    // too — mark the timestamp so a following Enter
                    // (`KeyResult::ForwardToPane(b"\r")`) is debounced and
                    // arrives at the agent as a standalone submit, not fused
                    // with the paste tail.
                    ui.last_pane_keystroke_at = Some(std::time::Instant::now());
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
                let area = terminal.get_frame().area();
                resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
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
                        ui.status_message = None;
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
                        // PRD #76 M2.15 fixup F2: route through the same
                        // SSOT sweep helpers that handle spawn / tab-switch
                        // resize, so a layout toggle can't end up with a
                        // different geometry than the rest of the
                        // codebase. The previous inline math used 67%
                        // (dashboard) for the non-mode-tab branch, which
                        // silently produced a 1-col-wider PTY for
                        // orchestration tabs (66%); it also skipped the
                        // `show_tab_bar` chrome accounting.
                        let frame_area = terminal.get_frame().area();
                        if tab_manager.active_mode_name().is_some() {
                            resize_mode_tab_panes(&*pane, &tab_manager, frame_area);
                        } else {
                            resize_dashboard_panes(&*pane, &ui, &tab_manager, frame_area);
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
                    //
                    // PRD #92 F4: previously the result of `close_pane` was
                    // discarded with `let _ =` and the card / session was
                    // removed unconditionally. A failed `StopAgent` RPC then
                    // left the underlying agent alive in the daemon
                    // registry while the dashboard card vanished — the user
                    // had no visibility and no retry. We now inspect each
                    // close result and preserve the card / session on
                    // failure so the user can see the error in
                    // `ui.status_message` and try again. For group-close
                    // (mode-tab teardown / orchestration tab teardown) we
                    // consume `CloseTabOutcome::closed` to remove
                    // successfully-closed cards and `CloseTabOutcome::failed`
                    // to keep failed cards with a status-bar summary.
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
                                // close_tab returns a per-pane outcome — successful closes
                                // get their cards removed, failed closes keep theirs.
                                match tab_manager.close_tab(tab_idx) {
                                    Ok(outcome) => {
                                        let mut st = state.blocking_write();
                                        for id in &outcome.closed {
                                            st.unregister_pane(id);
                                        }
                                        // Remove sessions whose pane_id is in the closed set ONLY.
                                        // Failed panes retain their sessions so the user can retry.
                                        let closed_set: std::collections::HashSet<&str> =
                                            outcome.closed.iter().map(String::as_str).collect();
                                        st.sessions.retain(|_, s| {
                                            s.pane_id.as_ref().is_none_or(|pid| {
                                                !closed_set.contains(pid.as_str())
                                            })
                                        });
                                        drop(st);
                                        // Clean pane_metadata only for the successfully-closed panes.
                                        for id in &outcome.closed {
                                            ui.pane_metadata.remove(id);
                                        }
                                        if outcome.is_clean() {
                                            ui.status_message = Some((
                                                format!(
                                                    "Closed tab containing pane {closed_pane_id}"
                                                ),
                                                std::time::Instant::now(),
                                            ));
                                        } else {
                                            // List which panes failed plus the first error so the
                                            // status bar stays readable (single-line). Full per-pane
                                            // errors are tracing-logged for the daemon log.
                                            let failed_ids: Vec<&str> = outcome
                                                .failed
                                                .iter()
                                                .map(|(id, _)| id.as_str())
                                                .collect();
                                            let first_err = outcome
                                                .failed
                                                .first()
                                                .map(|(_, e)| e.as_str())
                                                .unwrap_or("");
                                            for (id, e) in &outcome.failed {
                                                tracing::warn!(
                                                    pane_id = %id,
                                                    error = %e,
                                                    "F4: close_pane failed during tab teardown — card preserved"
                                                );
                                            }
                                            ui.status_message = Some((
                                                format!(
                                                    "Close partially failed for {} pane(s) — first error: {first_err}",
                                                    failed_ids.len()
                                                ),
                                                std::time::Instant::now(),
                                            ));
                                        }
                                        let area = terminal.get_frame().area();
                                        resize_dashboard_panes(&*pane, &ui, &tab_manager, area);
                                    }
                                    Err(e) => {
                                        ui.status_message = Some((
                                            format!("Failed to close tab: {e}"),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                            } else {
                                // Plain dashboard pane — close just this one and inspect
                                // the result. On Err the controller has already restored
                                // the local pane state (see EmbeddedPaneController::close_pane),
                                // so we leave the card / session in place for retry.
                                match pane.close_pane(pane_id) {
                                    Ok(()) => {
                                        let mut st = state.blocking_write();
                                        st.sessions.remove(&sid);
                                        st.unregister_pane(&closed_pane_id);
                                        drop(st);
                                        ui.pane_metadata.remove(&closed_pane_id);
                                        ui.status_message = Some((
                                            format!("Closed pane {closed_pane_id}"),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            pane_id = %closed_pane_id,
                                            error = %e,
                                            "F4: close_pane failed — card preserved for retry"
                                        );
                                        ui.status_message = Some((
                                            format!(
                                                "Failed to close pane {closed_pane_id}: {e} — press Ctrl+W to retry"
                                            ),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                            }
                            if ui.mode == UiMode::PaneInput {
                                ui.mode = UiMode::Normal;
                            }
                            // Clamp selected_index so it doesn't point past
                            // the now-shorter card list. (Only meaningful if
                            // at least one pane was actually removed; on a
                            // pure-failure close the card count is unchanged
                            // and this is a no-op since selected_index
                            // already points at the (preserved) card.)
                            if ui.selected_index > 0 {
                                ui.selected_index = ui.selected_index.saturating_sub(1);
                            }
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
                    UiMode::Normal => {
                        let selected_status = selected_id
                            .as_ref()
                            .and_then(|sid| snapshot.sessions.get(sid))
                            .map(|session| session.status.clone());
                        dispatch_normal_mode_key(
                            key,
                            &mut ui,
                            total,
                            selected_status,
                            &filtered,
                            &*pane,
                        )
                    }
                    UiMode::Filter => handle_filter_key(key, &mut ui),
                    UiMode::Help => handle_help_key(key, &mut ui),
                    UiMode::Rename => {
                        // Capture commit intent before the handler clears
                        // `rename_text`. M2.11 fixup 5 — the dispatch
                        // loop drives both the controller call AND the
                        // UI display-name maps so the dashboard mirrors
                        // EXACTLY the controller-resolved label
                        // (Applied → trimmed canonical; Cleared → remove
                        // entries; Rejected → leave existing label
                        // intact). Empty/whitespace-only text reaches
                        // the controller as a "clear" → daemon-side
                        // `None`, so hydrate falls back to the agent_id
                        // on reconnect rather than restoring a stale
                        // label (PRD #76 M2.11 reviewer P1).
                        let commit = rename_commit_value(key, &ui.rename_text);
                        let r = handle_rename_key(key, &mut ui, selected_id.as_deref());
                        if let Some(new_name) = commit
                            && let Some(ref sid) = selected_id
                            && let Some(session) = snapshot.sessions.get(sid)
                            && let Some(ref pane_id) = session.pane_id
                        {
                            // Best-effort daemon update — rename_pane's
                            // own error path already logs and swallows
                            // transient daemon failures, and the daemon
                            // RPC is spawned off the UI thread so a
                            // wedged daemon can't freeze the renderer.
                            match pane.rename_pane(pane_id, &new_name) {
                                Ok(outcome) => apply_rename_outcome(
                                    &mut ui.display_names,
                                    &mut ui.pane_display_names,
                                    sid,
                                    pane_id,
                                    outcome,
                                ),
                                Err(e) => {
                                    tracing::debug!(
                                        pane_id = %pane_id,
                                        error = %e,
                                        "rename_pane returned an error; UI maps unchanged"
                                    );
                                }
                            }
                        }
                        r
                    }
                    UiMode::DirPicker => handle_dir_picker_key(key, &mut ui),
                    UiMode::NewPaneForm => handle_new_pane_form_key(key, &mut ui),
                    UiMode::PaneInput => handle_pane_input_key(key),
                    UiMode::StarPrompt => handle_star_prompt_key(key, &mut ui),
                    UiMode::ConfigGenPrompt => handle_config_gen_prompt_key(key, &mut ui),
                    UiMode::QuitConfirm => {
                        // PRD #92 F1: the dialog needs to know the
                        // managed-agent count to decide whether picking
                        // Stop should go straight to shutdown or step
                        // through the secondary y/n confirmation. We
                        // count pane-backed sessions in the current
                        // snapshot (every dashboard card is a pane, so
                        // this is also "how many cards would lose their
                        // PTY"). Filter is irrelevant here — the user is
                        // about to terminate every agent on the daemon,
                        // not just the visible ones.
                        let managed_agents_count = snapshot
                            .sessions
                            .values()
                            .filter(|s| s.pane_id.is_some())
                            .count();
                        handle_quit_confirm_key(key, &mut ui, managed_agents_count)
                    }
                    UiMode::StopConfirm => handle_stop_confirm_key(key, &mut ui),
                }
            };

            match result {
                KeyResult::Quit => break 'outer,
                KeyResult::DetachAndQuit => {
                    // M2.5: emit explicit `KIND_DETACH` frames for every
                    // stream-backed pane so the daemon distinguishes
                    // voluntary detach from abrupt disconnect, then exit.
                    // Local-PTY panes have nothing to detach from — they
                    // die with the process either way.
                    if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
                        let errs = embedded.detach_all_streams();
                        if !errs.is_empty() {
                            tracing::warn!(
                                error_count = errs.len(),
                                "detach_all_streams reported errors during quit — proceeding"
                            );
                        }
                    }
                    break 'outer;
                }
                KeyResult::StopConfirmPrompt { agent_count } => {
                    // PRD #92 F1: transition to the secondary y/n dialog
                    // with the cached agent count rendered in the prompt.
                    // Default to No (safer default for a destructive
                    // action). Stay in this loop iteration; the next
                    // pass will render `StopConfirm` and route keys to
                    // `handle_stop_confirm_key`.
                    ui.stop_confirm_agent_count = agent_count;
                    ui.stop_confirm_selected = 0;
                    ui.mode = UiMode::StopConfirm;
                }
                KeyResult::StopAndQuit => {
                    // PRD #92 F1: user confirmed Stop (either via the
                    // primary dialog with 0 agents or via the secondary
                    // y/n with Yes). Tell the daemon to terminate every
                    // managed agent and exit, then break the TUI loop.
                    // Session save happens via the normal exit path
                    // (the `'outer` break unwinds into the existing
                    // session-save site so `--continue` is not poisoned).
                    //
                    // PRD #92 F1 followup: `shutdown_daemon` now waits
                    // for an explicit `KIND_SHUTDOWN_ACK` from the
                    // daemon. The original wire used socket-close as
                    // the implicit ack, which an old daemon (predating
                    // `PROTOCOL_VERSION = 2`) would also satisfy by
                    // closing the connection on an unknown frame —
                    // making the upgrade-mismatch case look like a
                    // successful shutdown. On `Err` we now do NOT exit
                    // the TUI; we dismiss the dialogs, surface the
                    // error via `ui.status_message`, and return to
                    // Normal mode so the user can retry, Detach, or
                    // `pkill` from a shell.
                    let shutdown_result = pane
                        .as_any()
                        .downcast_ref::<EmbeddedPaneController>()
                        .map(|embedded| embedded.shutdown_daemon());
                    match shutdown_result {
                        Some(Ok(())) | None => {
                            // Ack received (or non-stream backend, which
                            // can't actually shut down anything). Proceed
                            // to TUI exit via the existing `'outer`
                            // break.
                            break 'outer;
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                error = %e,
                                "shutdown_daemon failed — staying in the TUI so the user can retry or Detach"
                            );
                            ui.status_message = Some((
                                format!(
                                    "Stop failed: {e} — daemon may be incompatible or unresponsive. Try Detach or restart the daemon manually."
                                ),
                                std::time::Instant::now(),
                            ));
                            // Dismiss dialogs and return to Normal mode.
                            // Reset the primary-dialog selection to
                            // Detach so a hammer-Enter recovery picks
                            // the safe option, not Stop again.
                            ui.mode = UiMode::Normal;
                            ui.stop_confirm_selected = 0;
                            ui.quit_confirm_selected = 0;
                        }
                    }
                }
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
                KeyResult::SendPermissionResponse(approve) => {
                    // The handler already gated on `WaitingForInput` plus a
                    // non-empty selection; re-resolve the pane here so the
                    // PTY write goes to the right place. If `pane_id` is
                    // missing (placeholder session with no agent yet), this
                    // silently no-ops — same shape as RequestConfigGen.
                    if let Some(ref sid) = selected_id
                        && let Some(session) = snapshot.sessions.get(sid)
                        && let Some(ref pane_id) = session.pane_id
                    {
                        let key_char = if approve { "y" } else { "n" };
                        match pane.write_to_pane(pane_id, key_char) {
                            Ok(()) => {
                                ui.status_message = Some((
                                    format!(
                                        "Permission {}.",
                                        if approve { "approved" } else { "denied" }
                                    ),
                                    std::time::Instant::now(),
                                ));
                            }
                            Err(e) => {
                                ui.status_message = Some((
                                    format!("Permission response failed: {e}"),
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                }
                KeyResult::NewPane(req) => {
                    if pane.is_available() {
                        let dir_str = req.dir.display().to_string();

                        // Orchestration path — manage own panes, no agent pane.
                        if let Some(mut orch_config) = req.orchestration_config {
                            // PRD #107: use the name the user typed in the form, if any,
                            // so the tab title matches their input rather than always
                            // falling back to the TOML config name or cwd basename.
                            if !req.name.is_empty() {
                                orch_config.name = req.name.clone();
                            }
                            let prompt = prepare_orchestrator_prompt(&orch_config, &dir_str);
                            // PRD #76 M2.15: pre-compute spawn dims using
                            // the orchestration-layout helper (right 66%
                            // column, matching the orchestration
                            // renderer's `[34%, 66%]` split — fixup F3,
                            // not the dashboard's `[33%, 67%]`) so role
                            // PTYs open at the viewport size.
                            let frame_area = terminal.get_frame().area();
                            // show_tab_bar=true: opening an orchestration
                            // adds a new tab next to Dashboard, so the
                            // post-spawn render has ≥2 tabs.
                            // All roles share the same spawn dims (Tiled
                            // layout); pass role_index=0 and
                            // focused_role_index=None so the helper falls
                            // back to "role 0 expanded" — matching the
                            // renderer's "first slot expands" default when
                            // no role is focused yet (panes haven't been
                            // created at this point).
                            // PRD #76 M2.15 fixup pass 2 G2 — `Tiled` is
                            // hardcoded here so the focused_role_index
                            // parameter is geometrically a no-op, but pass
                            // it for symmetry / future safety.
                            let spawn_dims = orchestration_role_pane_dims(
                                frame_area,
                                orch_config.roles.len(),
                                0,
                                None,
                                PaneLayout::Tiled,
                                true,
                            );
                            match tab_manager.open_orchestration_tab(
                                &orch_config,
                                &dir_str,
                                prompt,
                                spawn_dims,
                            ) {
                                Ok((_tab_idx, role_pane_ids)) => {
                                    // PRD #110 followup: snapshot each role
                                    // pane's daemon agent_id before the
                                    // placeholder insert so the strict-
                                    // equality reuse guard accepts each
                                    // role agent's first `SessionStart`.
                                    let role_agent_ids: Vec<Option<String>> = role_pane_ids
                                        .iter()
                                        .map(|id| pane.pane_agent_id(id))
                                        .collect();
                                    {
                                        let mut st = state.blocking_write();
                                        // PRD #76 M2.13: `open_orchestration_tab`
                                        // already tags each role's daemon-bound
                                        // spawn with `AgentType::from_command(...)`,
                                        // so a reconnect's hydration carries the
                                        // right type. The local placeholder stays
                                        // at `None` until each role's first
                                        // `SessionStart` hook fires — preserving
                                        // the orchestration readiness gate's
                                        // 10-second fallback (pre-M2.13 contract).
                                        for (id, agent_id) in
                                            role_pane_ids.iter().zip(role_agent_ids.iter())
                                        {
                                            st.register_pane(id.clone());
                                            st.insert_placeholder_session(
                                                id.clone(),
                                                Some(dir_str.clone()),
                                                None,
                                                agent_id.clone(),
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
                                            if role.start {
                                                st.orchestrator_pane_ids
                                                    .insert(role_pane_ids[i].clone());
                                            }
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

                                    // Record creation time for delayed prompt injection fallback.
                                    if let Tab::Orchestration { id, .. } = tab_manager.active_tab()
                                    {
                                        ui.orchestration_created_at
                                            .insert(*id, std::time::Instant::now());
                                    }

                                    // Role commands are already running — each pane was
                                    // spawned with `role.command` as its initial process
                                    // (see TabManager::open_orchestration_tab in tab.rs).
                                    // Writing the command again here would land its bytes
                                    // in the agent's stdin, polluting the prompt buffer.

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
                            // Thread the form's Name through to `StartAgent.display_name`
                            // so a disconnect or crash between create and rename can't
                            // persist the command-based fallback label on the daemon
                            // (PRD #76 M2.11 reviewer P2). The controller resolves the
                            // form Name + command via `agent_pty::resolve_display_name`
                            // and returns the canonical string it stored on Pane.name
                            // (and sent as `StartAgent.display_name` for stream panes)
                            // so the UI maps below mirror EXACTLY what the daemon has —
                            // no separate normalization helper to drift (M2.11 fixup 4).
                            let form_name = Some(req.name.as_str());
                            // PRD #76 M2.12: tag the agent pane with its
                            // mode-tab membership so the daemon-side
                            // registry can echo it back via `list_agents`
                            // on the next reconnect, letting the
                            // hydration partition rebuild this mode tab
                            // instead of stranding the agent on the
                            // dashboard.
                            let tab_membership =
                                req.mode_config.as_ref().map(|m| TabMembership::Mode {
                                    name: m.name.clone(),
                                });
                            // PRD #76 M2.15: pre-compute spawn dims via the
                            // shared layout helpers so the agent PTY opens
                            // at the eventual size (no 24×80 → resize
                            // hiccup). Mode-tab panes use the mode-agent
                            // layout (left half × height-minus-chrome);
                            // dashboard panes use the dashboard right-67%
                            // column layout with `is_focused=true` (this
                            // path immediately focuses the new pane via
                            // `focus_pane(&new_id)` below).
                            let frame_area = terminal.get_frame().area();
                            let (spawn_rows, spawn_cols) = if req.mode_config.is_some() {
                                mode_agent_pane_dims(frame_area)
                            } else {
                                let embedded_pane_count = pane
                                    .as_any()
                                    .downcast_ref::<EmbeddedPaneController>()
                                    .map(|e| e.pane_ids().len())
                                    .unwrap_or(0);
                                let pane_count_after =
                                    (embedded_pane_count as u16).saturating_add(1);
                                dashboard_pane_dims(
                                    frame_area,
                                    pane_count_after,
                                    true,
                                    ui.pane_layout,
                                    tab_manager.show_tab_bar(),
                                )
                            };
                            // PRD #76 M2.13: infer agent_type from the
                            // form's command (the canonical "what runs in
                            // this pane" hint) — `cmd` is `None` for mode
                            // panes (which spawn empty and run the
                            // command later via `write_to_pane`), so use
                            // `req.command` directly to cover both flows.
                            // The inferred type goes ONLY into the daemon-
                            // bound spawn options so a remote reconnect's
                            // hydration carries it; the local placeholder
                            // stays at `None` until the first `SessionStart`
                            // hook fires (pre-M2.13 contract).
                            let spawn_agent_type = if req.command.is_empty() {
                                None
                            } else {
                                AgentType::from_command(Some(req.command.as_str()))
                            };
                            match pane.create_pane_with_options(
                                cmd,
                                Some(&dir_str),
                                AgentSpawnOptions {
                                    display_name: form_name,
                                    tab_membership,
                                    rows: spawn_rows,
                                    cols: spawn_cols,
                                    agent_type: spawn_agent_type,
                                },
                            ) {
                                Ok((new_id, resolved_name)) => {
                                    // Register so only events from our panes are accepted,
                                    // and create a placeholder session for an immediate dashboard card.
                                    //
                                    // PRD #110 followup: snapshot the daemon
                                    // agent_id so the placeholder survives the
                                    // strict-equality reuse guard on the
                                    // freshly-spawned agent's first
                                    // `SessionStart`.
                                    let new_agent_id = pane.pane_agent_id(&new_id);
                                    {
                                        let mut st = state.blocking_write();
                                        st.register_pane(new_id.clone());
                                        st.insert_placeholder_session(
                                            new_id.clone(),
                                            Some(dir_str.clone()),
                                            None,
                                            new_agent_id,
                                        );
                                    }
                                    ui.pane_display_names
                                        .insert(new_id.clone(), resolved_name.clone());
                                    ui.pane_names.insert(new_id.clone(), resolved_name);
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
                                        // PRD #76 M2.15 fixup pass 2 G1 — compute
                                        // side-pane dims so the side panes spawn
                                        // at the viewport-derived size, not the
                                        // 24×80 default.
                                        let total_side_count = (mode_config.panes.len()
                                            + mode_config.reactive_panes)
                                            as u16;
                                        let side_pane_dims = mode_side_pane_dims(
                                            terminal.get_frame().area(),
                                            total_side_count,
                                        );
                                        match tab_manager.open_mode_tab(
                                            &mode_config,
                                            &dir_str,
                                            new_id.clone(),
                                            side_pane_dims,
                                        ) {
                                            Ok((_tab_idx, side_ids)) => {
                                                for id in &side_ids {
                                                    state
                                                        .blocking_write()
                                                        .register_pane(id.clone());
                                                }
                                                let _ = pane.focus_pane(&new_id);
                                                ui.mode = UiMode::PaneInput;
                                                // PRD #76 M2.15: shared
                                                // layout helpers — spawn
                                                // and resize math are now
                                                // identical.
                                                let frame_area = terminal.get_frame().area();
                                                resize_mode_tab_panes_for(
                                                    &*pane, &new_id, &side_ids, frame_area,
                                                );
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
                                        // PRD #76 M2.15: route through the
                                        // shared dashboard-layout helper so
                                        // spawn-time and resize-time math
                                        // can't drift.
                                        let frame_area = terminal.get_frame().area();
                                        resize_dashboard_panes(
                                            &*pane,
                                            &ui,
                                            &tab_manager,
                                            frame_area,
                                        );
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
                        // PRD #76 M2.20: separate a CR-bearing keystroke from
                        // the preceding typed bytes so the agent TUI treats it
                        // as a standalone submit, not newline-in-input.
                        let sleep = submit_debounce_duration(
                            std::time::Instant::now(),
                            ui.last_pane_keystroke_at,
                            &bytes,
                        );
                        if !sleep.is_zero() {
                            std::thread::sleep(sleep);
                        }
                        if let Err(e) = embedded.write_raw_bytes(&pane_id, &bytes) {
                            ui.status_message =
                                Some((format!("PTY write failed: {e}"), std::time::Instant::now()));
                        }
                        ui.last_pane_keystroke_at = Some(std::time::Instant::now());
                    }
                }
                KeyResult::Continue => {}
            }

            // M2.11 fixup 5 — the deferred display_names ↔
            // pane_display_names mirror that used to live here was
            // the path that copied raw rename_text into
            // pane_display_names (so a `"  newname  "` rename
            // landed as `"  newname  "` in the dashboard while the
            // controller stored `"newname"`). Both maps are now
            // updated inline by the Rename dispatch arm above
            // using the controller-returned RenameOutcome, and
            // every other write path already touches both maps
            // (the new-pane handler, the apply-pending-names
            // block, and the restore paths). Reintroducing the
            // deferred mirror would re-open the divergence
            // reviewer P2 / auditor LOW flagged.

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

    // Snapshot the session for --continue restore *before* tearing down mode
    // tabs. The teardown loop unregisters every mode-tab pane id from
    // `state.managed_pane_ids`; if the snapshot ran after teardown, the
    // `retain` step would drop the mode-tab agent pane (which carries
    // `mode = Some(...)`) and the mode field would never reach disk (PRD #69).
    {
        let live_panes = state.blocking_read().managed_pane_ids.clone();

        let session = config::SavedSession::snapshot(
            &mut ui.pane_metadata,
            &ui.pane_display_names,
            &live_panes,
        );
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

    // PRD #93 Phase 2: there's no local-deck teardown path anymore.
    // Dropping `pane` closes the attach sockets, the daemon observes
    // EOF, and the agents survive — same property the round-7 loop
    // already guarded for the external-daemon case.

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

        // Render the tab bar. Cap long labels so trailing tabs don't clip
        // off the right edge of `Tabs`'s clipped output — every tab stays
        // at least partially visible for click-to-switch.
        let fitted_labels = fit_tab_labels(&tab_bar.labels, chunks[0].width);
        let titles: Vec<Line> = fitted_labels
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
    let (pane_ids, dashboard_area, panes_area) = match tab_view {
        ActiveTabView::Dashboard { exclude_pane_ids } => {
            let pane_ids: Vec<String> = all_pane_ids
                .into_iter()
                .filter(|id| !exclude_pane_ids.contains(id))
                .collect();
            let (dashboard_area, panes_area) = if pane_ids.is_empty() {
                (main_area, None)
            } else {
                let chunks = Layout::horizontal([
                    Constraint::Percentage(DASHBOARD_LEFT_PERCENT),
                    Constraint::Percentage(DASHBOARD_PANES_PERCENT),
                ])
                .split(main_area);
                (chunks[0], Some(chunks[1]))
            };
            (pane_ids, dashboard_area, panes_area)
        }
        ActiveTabView::Orchestration { role_pane_ids, .. } => {
            let pane_ids: Vec<String> = all_pane_ids
                .into_iter()
                .filter(|id| role_pane_ids.contains(id))
                .collect();
            let (dashboard_area, panes_area) = if pane_ids.is_empty() {
                (main_area, None)
            } else {
                let chunks = Layout::horizontal([
                    Constraint::Percentage(ORCHESTRATION_LEFT_PERCENT),
                    Constraint::Percentage(ORCHESTRATION_PANES_PERCENT),
                ])
                .split(main_area);
                (chunks[0], Some(chunks[1]))
            };
            (pane_ids, dashboard_area, panes_area)
        }
        _ => unreachable!(),
    };

    // Orchestration tabs use the same dashboard card rendering as the main dashboard.

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
    if ui.mode == UiMode::StopConfirm {
        render_stop_confirm(
            frame,
            ui.stop_confirm_selected,
            ui.stop_confirm_agent_count,
            palette,
        );
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
    let popup_width = 64u16.min(area.width.saturating_sub(4));
    let popup_height = 10u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    // PRD #93 Phase 2 / M4.2: dialog was Detach/Cancel — every pane is
    // daemon-backed so quitting the TUI was always a detach, never a kill.
    // PRD #92 F1: Stop joins as a third option (index 1) to restore the
    // pre-daemon "user gesture that takes everything down". Detach stays
    // the default so existing muscle memory does not become destructive.
    let options = [
        ("Detach", "leave agents running on the daemon"),
        ("Stop", "shut down agents and daemon"),
        ("Cancel", "return to dashboard"),
    ];

    let mut text = vec![
        Line::from(""),
        Line::styled(
            "  Quit dot-agent-deck?",
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

/// PRD #92 F1: secondary y/n confirmation dialog when the user picked
/// Stop with at least one managed agent alive. Renders the count
/// explicitly so the user sees exactly how many agents are about to be
/// terminated. Default selection is No (index 0).
fn render_stop_confirm(
    frame: &mut Frame,
    selected: usize,
    agent_count: usize,
    palette: ColorPalette,
) {
    let area = frame.area();
    let popup_width = 68u16.min(area.width.saturating_sub(4));
    let popup_height = 10u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let header = if agent_count == 1 {
        "  1 managed agent will be terminated and the daemon will shut down.".to_string()
    } else {
        format!("  {agent_count} managed agents will be terminated and the daemon will shut down.")
    };

    let options = [("No", "return to the previous dialog"), ("Yes", "confirm")];

    let mut text = vec![
        Line::from(""),
        Line::styled(
            header,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::from("  Continue?"),
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
            format!("  {cursor} {label:<5} \u{2014} {desc}"),
            style,
        ));
    }

    text.push(Line::from(""));
    text.push(Line::styled(
        "  y / Enter on Yes confirms  ·  n / Esc / Enter on No returns to Quit dialog",
        Style::default().fg(Color::DarkGray),
    ));

    let block = Block::default()
        .title(" Stop ")
        .title_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
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
        Line::from("  j / Down        Select next card"),
        Line::from("  k / Up          Select previous card"),
        Line::from("  1-9             Jump to pane N"),
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
        Line::from("  Ctrl+click      Open hyperlink"),
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

/// Footer-hint string for the unified new-pane form. Factored out so the
/// focus-dependent wording can be unit-tested without driving a TestBackend.
/// `name_submits` is true when focus is on Name and the Command field is
/// hidden (orchestration selected) — i.e. Enter on Name submits the form.
fn new_pane_form_footer_hint(has_mode_field: bool, name_submits: bool) -> &'static str {
    if has_mode_field {
        if name_submits {
            "  Tab: switch  \u{25c0}\u{25b6}: mode  Enter: submit  Esc: cancel"
        } else {
            "  Tab: switch  \u{25c0}\u{25b6}: mode  Enter: next  Esc: cancel"
        }
    } else {
        "  Tab: switch field  Enter: next/confirm  Esc: cancel"
    }
}

fn render_new_pane_form(frame: &mut Frame, form: &NewPaneFormState, palette: ColorPalette) {
    let area = frame.area();
    let popup_width = 56.min(area.width.saturating_sub(4));
    // The mode field (when modes exist) or the tip line (when they don't)
    // each need 2 extra rows.  Always reserve them.
    let mode_extra: u16 = 2;
    // PRD #106: when the Command field is hidden (orchestration selected) the
    // form is two rows shorter — Command's label row plus its spacing row.
    let cmd_visible = form.command_visible();
    let cmd_rows: u16 = if cmd_visible { 2 } else { 0 };
    let popup_height = (10 + mode_extra + cmd_rows).min(area.height.saturating_sub(4));
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
    if cmd_visible {
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
    }
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    // PRD #106 follow-up: when the Command field is hidden (orchestration
    // selected) and focus is on Name, Enter submits — surface that instead of
    // the generic "Enter: next" wording, which is misleading in that state.
    let name_submits = form.focused == FormField::Name && !cmd_visible;
    let footer = new_pane_form_footer_hint(form.has_mode_field, name_submits);
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

/// Convert screen-absolute mouse coordinates to pane-relative coordinates.
/// Returns (col, row) relative to the pane's inner area (inside border).
fn pane_relative_coords(screen_col: u16, screen_row: u16, pane_rect: &Option<Rect>) -> (u16, u16) {
    if let Some(rect) = pane_rect {
        let col = screen_col.saturating_sub(rect.x + 1); // +1 for border
        let row = screen_row.saturating_sub(rect.y + 1);
        (col, row)
    } else {
        (screen_col, screen_row)
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

    #[test]
    fn dead_slot_pane_id_is_deterministic_per_role() {
        // Same (cwd, name, role_index) must produce the same id so a
        // reconnect doesn't keep minting fresh placeholder cards on
        // every reattach.
        let a = dead_slot_pane_id("/work", "tdd-cycle", 4);
        let b = dead_slot_pane_id("/work", "tdd-cycle", 4);
        assert_eq!(a, b);
        // Different role_index → different id.
        let c = dead_slot_pane_id("/work", "tdd-cycle", 3);
        assert_ne!(a, c);
        // Different orchestration → different id.
        let d = dead_slot_pane_id("/work", "other-cycle", 4);
        assert_ne!(a, d);
        // is_dead_slot_pane_id accepts the synthesized id and rejects
        // a normal numeric pane id.
        assert!(is_dead_slot_pane_id(&a));
        assert!(!is_dead_slot_pane_id("42"));
    }

    // Follow-up to 0d5e651 (auditor finding #4): the old format
    // `__dead-slot__-{cwd}-{name}-{idx}` was ambiguous whenever cwd
    // or orchestration_name contained hyphens. Two distinct tuples
    // could produce the same synthetic id, which would then alias
    // their placeholder sessions. Pin that the length-prefixed format
    // disambiguates the textbook collision case.
    #[test]
    fn dead_slot_pane_id_disambiguates_hyphenated_inputs() {
        // Under the old `-`-separated form both inputs formatted to
        // `__dead-slot__-/a-b-c-1`. Under the length-prefixed form
        // they are guaranteed distinct.
        let a = dead_slot_pane_id("/a", "b-c", 1);
        let b = dead_slot_pane_id("/a-b", "c", 1);
        assert_ne!(
            a, b,
            "differently-hyphenated (cwd, orchestration_name) tuples \
             must produce distinct synthetic ids"
        );
    }
}

// ---------------------------------------------------------------------------
// PRD #77 — L1 harness public surface
// ---------------------------------------------------------------------------
//
// Stable test-only entry points consumed by `tests/render_dashboard.rs`.
// They are always-public (not feature-gated) because Rust integration
// tests cannot enable a crate feature on demand — but no production
// caller exercises them. See PRD #77 Decision 2 for the L1 / L2 split.

/// Card density tier picked by the dashboard's adaptive layout
/// (`choose_density`). Public so L1 snapshot tests can pin a specific
/// tier rather than depending on the runtime calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardDensityKind {
    Compact,
    Normal,
    Spacious,
}

impl From<CardDensityKind> for CardDensity {
    fn from(kind: CardDensityKind) -> Self {
        match kind {
            CardDensityKind::Compact => CardDensity::Compact,
            CardDensityKind::Normal => CardDensity::Normal,
            CardDensityKind::Spacious => CardDensity::Spacious,
        }
    }
}

/// L1 harness helper — render exactly one session card at the requested
/// density into a fresh `TestBackend` buffer and return it for snapshot
/// assertions.
///
/// Wraps the internal `render_session_card` so the L1 snapshot test in
/// `tests/render_dashboard.rs` can pin a card's text layout without
/// re-implementing the renderer. See PRD #77 catalog entry
/// `dashboard/pane/004`.
#[allow(clippy::too_many_arguments)]
pub fn render_card_to_buffer(
    session: &SessionState,
    display_name: Option<&str>,
    card_number: Option<u8>,
    density: CardDensityKind,
    palette: ColorPalette,
    tick: u64,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let display_name_owned = display_name.map(str::to_string);
    terminal
        .draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height,
            };
            render_session_card(
                frame,
                area,
                session,
                tick,
                false,
                display_name_owned.as_ref(),
                card_number,
                density.into(),
                palette,
                None,
            );
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}
