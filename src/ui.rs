use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
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

/// PRD #128 Direction B-1 — minimum gap between SessionStart being observed
/// for the start-role pane and the orchestrator's role prompt being written
/// into it. Claude Code's `SessionStart` hook fires before its TUI input is
/// in submit-CR-aware mode on slower environments (remote daemon, weak VM,
/// scheduler jitter). A CR that arrives during that window is treated as a
/// newline in the input buffer, not as a submit — the role prompt lands
/// visibly but never dispatches. Holding the write until this buffer has
/// elapsed gives Claude Code's TUI enough time to finish booting on those
/// environments; the gap exists on laptop-local too but is short enough
/// there that the symptom doesn't reproduce.
///
/// 500 ms is tuned to comfortably cover the laptop/remote gap observed
/// against the M1.1 instrumented build (see PRD #128). Visible to the user
/// as a brief delay between the orchestration tab opening and the prompt
/// appearing in the input box; small enough not to feel laggy, large enough
/// to make the failure mode disappear.
pub const SPAWN_TIME_READINESS_BUFFER: std::time::Duration = std::time::Duration::from_millis(500);

/// PRD #128 Direction B-1 — returns whether the spawn-time orchestrator
/// role prompt should fire NOW. Returns `false` if `ready_since` is set
/// but `SPAWN_TIME_READINESS_BUFFER` hasn't elapsed yet; `true` once it
/// has. `ready_since == None` means the readiness gate isn't engaged yet
/// (caller drives that — e.g. the timeout-ready fallback path that
/// ignores SessionStart and fires after 10 s); the helper returns `true`
/// in that case so the caller's own gating wins.
///
/// Extracted so the policy is unit-testable AND so the integration test
/// at `tests/spawn_time_role_prompt_submit_after_session_start.rs` can
/// drive the same gate the TUI loop uses without spinning up the loop
/// itself.
pub fn should_inject_spawn_time_prompt(
    ready_since: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match ready_since {
        Some(t) => now.saturating_duration_since(t) >= SPAWN_TIME_READINESS_BUFFER,
        None => true,
    }
}

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
    /// PRD #80: screen rects of the clickable buttons rendered this frame,
    /// paired with the [`Action`] each one triggers. Populated during render
    /// and consulted FIRST on a mouse Down/Up so a button click short-circuits
    /// before the existing pane/selection/scroll/hyperlink logic. Empty until
    /// M2 renders the button bar — the foundation (field + hit-test) lands in
    /// M1 so the render side has a stable home to populate.
    #[allow(dead_code)]
    button_rects: Vec<(Action, Rect)>,
    /// PRD #80 M3: screen rects of each tab's `[×]` close affordance, paired
    /// with the tab index to close (only closeable Mode/Orchestration tabs;
    /// the Dashboard at index 0 is excluded). Populated each render, consulted
    /// on a mouse Down/Up AFTER `button_rects` but BEFORE `tab_header_rects`,
    /// so the `[×]` beats the surrounding header.
    tab_close_rects: Vec<(usize, Rect)>,
    /// PRD #80 M3: screen rects of each tab's clickable header, paired with the
    /// tab index to switch to. Populated each render; a click here that missed
    /// the `[×]` switches tabs.
    tab_header_rects: Vec<(usize, Rect)>,
    /// PRD #80 M4: screen rects of each rendered dashboard card, paired with
    /// its flat selection index. Populated each dashboard render; consulted on
    /// a mouse Down/Up AFTER the button/tab rects — a single click selects the
    /// card, a double-click focuses its pane.
    card_rects: Vec<(usize, Rect)>,
    /// PRD #80 M5: screen rects of the clickable buttons in the currently
    /// shown modal/overlay (quit-confirm, config-gen, star-prompt, help),
    /// paired with the [`Action`] each fires. Repopulated each render by
    /// `render_overlays` and cleared when no modal is shown. Because the modal
    /// is topmost, these are hit-tested FIRST (and exclusively) while a modal
    /// is active, ahead of the bottom-bar / tab / card rects behind it.
    modal_button_rects: Vec<(Action, Rect)>,
    /// PRD #80 M7: directory-picker `[Confirm]`/`[Cancel]`/`[Filter]` button
    /// rects, paired with the [`Action`] each fires. Populated by
    /// `render_overlays` while the picker is shown, cleared otherwise.
    picker_button_rects: Vec<(Action, Rect)>,
    /// PRD #80 M7: directory-picker row rects, paired with the row's index
    /// into `DirPickerState.filtered_indices` (matching `selected`). A single
    /// click selects the row, a double-click descends, and a click on the
    /// `..` row goes up. Populated while the picker is shown, cleared
    /// otherwise.
    picker_row_rects: Vec<(usize, Rect)>,
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
    /// PRD #128 Direction B-1 — tracks the moment SessionStart was first
    /// observed for each orchestration tab's start-role pane. The role
    /// prompt is held until `SPAWN_TIME_READINESS_BUFFER` has elapsed
    /// since this timestamp, giving Claude Code's TUI time to enter
    /// submit-CR-aware mode on slower environments. Cleared when the
    /// prompt finally fires (entry never re-added).
    orchestration_ready_since: HashMap<TabId, std::time::Instant>,
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
            orchestration_ready_since: HashMap::new(),
            pending_dispatches: Vec::new(),
            last_pane_keystroke_at: None,
            button_rects: Vec::new(),
            tab_close_rects: Vec::new(),
            tab_header_rects: Vec::new(),
            card_rects: Vec::new(),
            modal_button_rects: Vec::new(),
            picker_button_rects: Vec::new(),
            picker_row_rects: Vec::new(),
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

#[derive(Debug, Clone)]
pub struct NewPaneRequest {
    dir: PathBuf,
    name: String,
    command: String,
    mode_config: Option<ModeConfig>,
    orchestration_config: Option<OrchestrationConfig>,
}

/// PRD #80: the single action layer. Every keyboard-only command and (from
/// M2 onward) every clickable affordance maps to exactly one `Action`
/// variant, and [`dispatch_action`] is the one place each variant executes.
/// The keystroke branch in `run_tui` is a thin `KeyEvent -> Option<Action>`
/// mapper; a future mouse Down on a button rect produces the *same* variant,
/// so key and click cannot drift apart.
///
/// Text input (typing into the filter/rename fields, the new-pane form) stays
/// keyboard-driven by design — the per-mode handlers keep applying those
/// local-state mutations inline and return [`Action::Continue`] when there is
/// nothing for the dispatcher to do.
#[derive(Debug, Clone)]
pub enum Action {
    /// Nothing for the dispatcher to do — the per-mode handler already
    /// applied any local-state mutation (e.g. a typed character).
    Continue,
    Quit,
    /// PRD #76, M2.5: detach every stream-backed pane (sending an explicit
    /// `KIND_DETACH` frame so the daemon distinguishes voluntary detach
    /// from abrupt disconnect) and then exit. Local-PTY panes are torn
    /// down by the normal quit path — they can't survive process exit.
    DetachAndQuit,
    Focus,
    /// PRD #80: open the directory picker to start a new pane (Ctrl+N). This
    /// is the global "New Pane" command and the target of the M2 button bar's
    /// `[New Pane Ctrl+N]` button — distinct from [`Action::SpawnPane`], which
    /// is the *result* of submitting the new-pane form.
    NewPane,
    /// PRD #80: close the selected pane — or the entire mode/orchestration tab
    /// it belongs to (Ctrl+W).
    CloseSelected,
    /// PRD #80: toggle the embedded-pane layout between stacked and tiled
    /// (Ctrl+T).
    ToggleLayout,
    /// PRD #80: leave `PaneInput` and return to Normal (command) mode on the
    /// current tab (Ctrl+D).
    DetachToNormal,
    /// PRD #80 M2: open the help overlay (or close it if already open) — the
    /// `?` key and the `[Help ?]` button share this one path.
    ToggleHelp,
    /// PRD #80: Ctrl+PageDown — advance to the next tab (clamped, gated on a
    /// visible tab bar). Distinct from [`Action::CycleTabNext`], which wraps.
    GlobalNextTab,
    /// PRD #80: Ctrl+PageUp — go to the previous tab (clamped, gated on a
    /// visible tab bar).
    GlobalPrevTab,
    /// PRD #80: Normal-mode Tab / Right / `l` — cycle to the next tab (wraps).
    CycleTabNext,
    /// PRD #80: Normal-mode BackTab / Left / `h` — cycle to the previous tab
    /// (wraps).
    CycleTabPrev,
    /// PRD #80 M3: switch to the tab at this index — the outcome of clicking a
    /// tab header in the strip (the keyboard equivalent of Tab / Ctrl+PageDown
    /// landing on that tab). The index is from the most recent render, stable
    /// for the click that produced it.
    SelectTab(usize),
    /// PRD #80 M3: close the tab at this index — the outcome of clicking a
    /// Mode/Orchestration tab's `[×]` affordance, reusing Ctrl+W's tab-teardown
    /// semantics for the clicked tab (not necessarily the active one).
    CloseTab(usize),
    /// PRD #80 M4: select the dashboard card at this index — the outcome of a
    /// single click on a card. Per PRD #68 a click selects exactly card N (the
    /// same card a j/k linear-cycle would land on), mirroring the selection
    /// into the embedded focus the way `dispatch_normal_mode_key` does.
    SelectCard(usize),
    /// PRD #80 M4: enter filter mode (the `/` key and the `[Filter /]` button
    /// share this path).
    EnterFilter,
    /// PRD #80 M4: enter rename mode for the selected card (the `r` key and the
    /// `[Rename r]` button share this path; a no-op when no card is selected).
    EnterRename,
    /// PRD #80 M5: quit-confirm `[Stop]` button — resolve exactly as Enter-on-
    /// Stop does: go straight to shutdown when there are no managed agents,
    /// otherwise step through the secondary Stop-confirm dialog. The agent
    /// count is computed in `dispatch_action` (which has the snapshot).
    RequestStop,
    /// PRD #80 M5: dismiss the current modal back to Normal (quit-confirm
    /// `[Cancel]`).
    DismissModal,
    /// PRD #80 M5: config-gen `[Yes]` — send the config-gen prompt to the
    /// pending target pane (same outcome as Enter-on-Yes).
    ConfigGenConfirm,
    /// PRD #80 M5: config-gen `[No]` — dismiss the prompt for now (clears the
    /// target, returns to Normal).
    ConfigGenDismiss,
    /// PRD #80 M5: config-gen `[Never]` — suppress the prompt for this
    /// directory and return to Normal.
    ConfigGenSuppress,
    /// PRD #80 M5: star-prompt `[Star]` — open the repo URL and stop asking
    /// (== `s`).
    StarConfirm,
    /// PRD #80 M5: star-prompt `[Snooze]` — snooze the prompt (== `l` / Esc).
    StarSnooze,
    /// PRD #80 M5: star-prompt `[Dismiss]` — stop asking permanently (== `d`).
    StarDismiss,
    /// PRD #80 M6: filter-row `[Apply]` — commit the filter and return to
    /// Normal, KEEPING the typed filter text (== Enter in filter mode).
    ApplyFilter,
    /// PRD #80 M6: filter-row `[Cancel]` — clear the filter and return to
    /// Normal (== Esc in filter mode).
    CancelFilter,
    /// PRD #80 M6: rename-row `[Save]` — commit the rename on the selected
    /// card and return to Normal (== Enter in rename mode).
    SaveRename,
    /// PRD #80 M6: rename-row `[Cancel]` — abandon the rename and return to
    /// Normal, leaving the existing name untouched (== Esc in rename mode).
    CancelRename,
    /// PRD #80 M7: single-click a directory-picker row — set the picker's
    /// selection to this filtered-list index (== j/k landing on it).
    PickerSelectRow(usize),
    /// PRD #80 M7: double-click a directory-picker row — select it then
    /// descend into it (== Enter / l). The `..` row is handled via
    /// [`Action::PickerParent`] instead.
    PickerEnterRow(usize),
    /// PRD #80 M7: click the `..` row / breadcrumb — go up one directory
    /// (== h / Backspace / Left).
    PickerParent,
    /// PRD #80 M7: picker `[Confirm]` — confirm the current directory and
    /// advance to the new-pane form (== Space).
    PickerConfirm,
    /// PRD #80 M7: picker `[Cancel]` — close the picker (== q / Esc).
    PickerCancel,
    /// PRD #80 M7: picker `[Filter]` — open the picker's filter input (== `/`).
    PickerFilter,
    /// PRD #80: Normal-mode digit `1`-`9` — jump to card N and focus its pane.
    FocusCard(usize),
    /// PRD #80: on a mode tab, move the in-tab side-pane focus down (j/Down).
    ModeTabSelectNext,
    /// PRD #80: on a mode tab, move the in-tab side-pane focus up (k/Up).
    ModeTabSelectPrev,
    /// PRD #80: on a mode tab, enter `PaneInput` on the focused side/agent
    /// pane (Enter).
    ModeTabFocus,
    /// PRD #80: on a mode tab, reset in-tab focus back to the agent pane
    /// (Esc).
    ModeTabReset,
    /// PRD #80: the new-pane form was submitted — spawn the requested pane.
    /// This is the *outcome* of the form, distinct from [`Action::NewPane`],
    /// which merely opens the picker that leads to the form.
    SpawnPane(NewPaneRequest),
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

fn handle_pane_input_key(key: KeyEvent) -> Action {
    if let Some(bytes) = keyevent_to_bytes(&key) {
        Action::ForwardToPane(bytes)
    } else {
        Action::Continue
    }
}

fn handle_quit_confirm_key(key: KeyEvent, ui: &mut UiState, managed_agents_count: usize) -> Action {
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
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => {
            ui.quit_confirm_selected = ui.quit_confirm_selected.saturating_sub(1);
            Action::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if ui.quit_confirm_selected + 1 < QUIT_OPTION_COUNT {
                ui.quit_confirm_selected += 1;
            }
            Action::Continue
        }
        KeyCode::Enter => match ui.quit_confirm_selected {
            0 => Action::DetachAndQuit,
            1 => {
                // PRD #92 F1 Stop: if no managed agents, proceed directly;
                // otherwise step through the secondary y/n dialog so the
                // user has to confirm the destructive action explicitly.
                if managed_agents_count == 0 {
                    Action::StopAndQuit
                } else {
                    Action::StopConfirmPrompt {
                        agent_count: managed_agents_count,
                    }
                }
            }
            _ => {
                ui.mode = UiMode::Normal;
                Action::Continue
            }
        },
        KeyCode::Esc => {
            ui.mode = UiMode::Normal;
            Action::Continue
        }
        _ => Action::Continue,
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
fn handle_stop_confirm_key(key: KeyEvent, ui: &mut UiState) -> Action {
    const STOP_OPTION_COUNT: usize = 2;
    match key.code {
        // Ctrl+C inside the secondary dialog: same hard-quit semantic as
        // the primary dialog. Daemon sees the implicit detach.
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => {
            ui.stop_confirm_selected = ui.stop_confirm_selected.saturating_sub(1);
            Action::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if ui.stop_confirm_selected + 1 < STOP_OPTION_COUNT {
                ui.stop_confirm_selected += 1;
            }
            Action::Continue
        }
        // `y` is a shortcut for Yes regardless of which option is
        // highlighted — matches the convention from similar dialogs.
        KeyCode::Char('y') | KeyCode::Char('Y') => Action::StopAndQuit,
        // `n` is a shortcut for No: return to the primary dialog with
        // Stop selected, so the user can pick Detach or Cancel without
        // re-opening anything.
        KeyCode::Char('n') | KeyCode::Char('N') => {
            ui.stop_confirm_selected = 0;
            ui.mode = UiMode::QuitConfirm;
            Action::Continue
        }
        KeyCode::Enter => match ui.stop_confirm_selected {
            // Index 1 == Yes
            1 => Action::StopAndQuit,
            // Index 0 == No: return to primary dialog (Stop still highlighted).
            _ => {
                ui.stop_confirm_selected = 0;
                ui.mode = UiMode::QuitConfirm;
                Action::Continue
            }
        },
        KeyCode::Esc => {
            ui.stop_confirm_selected = 0;
            ui.mode = UiMode::QuitConfirm;
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_star_prompt_key(key: KeyEvent, ui: &mut UiState) -> Action {
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
            Action::Continue
        }
        KeyCode::Char('l') | KeyCode::Esc => {
            ui.star_prompt_state.snooze();
            ui.mode = UiMode::Normal;
            Action::Continue
        }
        KeyCode::Char('d') => {
            ui.star_prompt_state.dismiss_permanently();
            ui.mode = UiMode::Normal;
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_config_gen_prompt_key(key: KeyEvent, ui: &mut UiState) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            ui.config_gen_selected = ui.config_gen_selected.saturating_sub(1);
            Action::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if ui.config_gen_selected < 2 {
                ui.config_gen_selected += 1;
            }
            Action::Continue
        }
        KeyCode::Enter => match ui.config_gen_selected {
            0 => {
                // Yes — send prompt and focus pane.
                ui.mode = UiMode::Normal;
                if let Some((pane_id, cwd)) = ui.config_gen_target.take() {
                    return Action::SendConfigGenPrompt { pane_id, cwd };
                }
                Action::Continue
            }
            1 => {
                // No — dismiss for now, hint stays on card.
                ui.config_gen_target = None;
                ui.mode = UiMode::Normal;
                Action::Continue
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
                Action::Continue
            }
        },
        KeyCode::Esc => {
            ui.config_gen_target = None;
            ui.mode = UiMode::Normal;
            Action::Continue
        }
        _ => Action::Continue,
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
) -> Action {
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
) -> Action {
    // Ctrl+C from dashboard: show quit confirmation
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
    }
    match key.code {
        // Dashboard card navigation (linear cycling)
        KeyCode::Char('j') | KeyCode::Down => {
            if total > 0 {
                ui.selected_index = (ui.selected_index + 1) % total;
            }
            Action::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if total > 0 {
                ui.selected_index = (ui.selected_index + total - 1) % total;
            }
            Action::Continue
        }
        // Left/Right/h/l handled in main loop for tab switching
        // PRD #80 M4: route `/` through the shared action so the key and the
        // `[Filter /]` button funnel into the same `dispatch_action` path.
        KeyCode::Char('/') => Action::EnterFilter,
        // PRD #80 M2: route `?` through the shared action so the key and the
        // `[Help ?]` button funnel into the same `dispatch_action` path.
        KeyCode::Char('?') => Action::ToggleHelp,
        // PRD #80 M4: route `r` through the shared action so the key and the
        // `[Rename r]` button funnel into the same `dispatch_action` path. The
        // `total > 0` guard mirrors the prior behavior (rename needs a card).
        KeyCode::Char('r') if total > 0 => Action::EnterRename,
        KeyCode::Enter if total > 0 => Action::Focus,
        KeyCode::Char('g') if total > 0 => Action::RequestConfigGen,
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
            Action::SendPermissionResponse(true)
        }
        KeyCode::Char('n')
            if total > 0
                && key.modifiers == KeyModifiers::NONE
                && selected_status == Some(SessionStatus::WaitingForInput) =>
        {
            Action::SendPermissionResponse(false)
        }
        KeyCode::Esc => {
            if !ui.filter_text.is_empty() {
                ui.filter_text.clear();
            }
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_filter_key(key: KeyEvent, ui: &mut UiState) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
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
    Action::Continue
}

fn handle_help_key(key: KeyEvent, ui: &mut UiState) -> Action {
    match key.code {
        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
            ui.mode = UiMode::Normal;
        }
        _ => {}
    }
    Action::Continue
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
) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
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
    Action::Continue
}

fn handle_dir_picker_key(key: KeyEvent, ui: &mut UiState) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
    }
    let picker = match ui.dir_picker.as_mut() {
        Some(p) => p,
        None => {
            ui.mode = UiMode::Normal;
            return Action::Continue;
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
        return Action::Continue;
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
                return Action::Continue;
            }
            if picker.filtered_indices.is_empty() {
                return Action::Continue;
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
            return Action::Continue;
        }
        _ => {}
    }
    Action::Continue
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

fn handle_new_pane_form_key(key: KeyEvent, ui: &mut UiState) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
    }
    let form = match ui.new_pane_form.as_mut() {
        Some(f) => f,
        None => {
            ui.mode = UiMode::Normal;
            return Action::Continue;
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
                return Action::SpawnPane(req);
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
    Action::Continue
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
/// PRD #80: control-flow signal returned by [`dispatch_action`]. Most actions
/// mutate state and yield [`Flow::Continue`]; the quit/stop/detach actions
/// yield [`Flow::Break`] so the caller breaks the TUI's outer loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Break,
}

/// PRD #80: map a Ctrl-modified `KeyEvent` to the global command [`Action`] it
/// triggers (works from any UI mode). Returns `None` for any key this layer
/// does not own, so it falls through to the per-mode handlers. This is part of
/// the thin `KeyEvent -> Option<Action>` mapper that pairs with
/// [`dispatch_action`]; the M2 button bar produces the SAME variants from a
/// click, so key and click cannot drift.
pub fn global_ctrl_action(key: &KeyEvent) -> Option<Action> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        // Ctrl+d: enter Normal (command) mode, stay on current tab.
        KeyCode::Char('d') => Some(Action::DetachToNormal),
        // Ctrl+t: toggle layout.
        KeyCode::Char('t') => Some(Action::ToggleLayout),
        // Ctrl+n: new pane (open directory picker).
        KeyCode::Char('n') => Some(Action::NewPane),
        // Ctrl+w: close selected pane (or entire mode/orchestration tab).
        KeyCode::Char('w') => Some(Action::CloseSelected),
        // Ctrl+PageDown / Ctrl+PageUp: move between tabs.
        KeyCode::PageDown => Some(Action::GlobalNextTab),
        KeyCode::PageUp => Some(Action::GlobalPrevTab),
        _ => None,
    }
}

/// PRD #80: map a Normal-mode tab-cycling key (Tab / Shift+Tab / Left / Right /
/// h / l) to its [`Action`]. Returns `None` for anything else.
fn cycle_tab_action(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => Some(Action::CycleTabNext),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => Some(Action::CycleTabPrev),
        _ => None,
    }
}

/// PRD #80: map an in-tab navigation key on a mode tab (j/k/Up/Down/Enter/Esc)
/// to its [`Action`]. The caller only invokes this when the active tab is a
/// `Tab::Mode`, so the returned action is always meaningful there.
fn mode_tab_nav_action(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ModeTabSelectNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ModeTabSelectPrev),
        KeyCode::Enter => Some(Action::ModeTabFocus),
        KeyCode::Esc => Some(Action::ModeTabReset),
        _ => None,
    }
}

/// PRD #80: a clickable affordance that carries its keyboard shortcut inline
/// (e.g. `[New Pane Ctrl+N]`) plus the [`Action`] it triggers. Render and
/// hit-test live together (see [`Button::render`] and [`hit_test_button`]) so
/// they cannot drift. From M2 the button bar renders these and records each
/// one's screen rect in `UiState::button_rects`; a mouse Down hit-tested
/// against those rects produces exactly the same `Action` the keyboard maps
/// to, which is what makes the mouse/keyboard parity self-evident in code.
#[derive(Debug, Clone)]
pub struct Button {
    /// Human-readable command name, e.g. `New Pane`.
    pub label: String,
    /// The keyboard shortcut shown inline, e.g. `Ctrl+N`. Empty for buttons
    /// that have no keyboard equivalent (none in the parity-only PRD #80).
    pub shortcut: String,
    /// The action this button triggers when clicked — identical to the action
    /// the inline shortcut maps to.
    pub action: Action,
    /// Whether the button is currently actionable. A disabled button renders
    /// dimmed; M2+ decides whether to record/ignore its rect.
    pub enabled: bool,
}

impl Button {
    /// Construct a button. `label`/`shortcut` accept anything `Into<String>`.
    pub fn new(
        label: impl Into<String>,
        shortcut: impl Into<String>,
        action: Action,
        enabled: bool,
    ) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.into(),
            action,
            enabled,
        }
    }

    /// The full on-screen text: `[Label Shortcut]`, or `[Label]` when the
    /// button has no shortcut. The narrow-terminal fallback (shortcut-only)
    /// arrives with the M2 button bar.
    pub fn display_label(&self) -> String {
        if self.shortcut.is_empty() {
            format!("[{}]", self.label)
        } else {
            format!("[{} {}]", self.label, self.shortcut)
        }
    }

    /// The narrow-terminal fallback label: just the bracketed shortcut, e.g.
    /// `[Ctrl+N]` (or `[New Pane]` if the button has no shortcut). Used by the
    /// button bar when the full `[Label Shortcut]` set doesn't fit, so every
    /// command stays identifiable without a mid-label truncation.
    pub fn shortcut_only_label(&self) -> String {
        if self.shortcut.is_empty() {
            format!("[{}]", self.label)
        } else {
            format!("[{}]", self.shortcut)
        }
    }

    /// The `(Action, Rect)` pair to record in `UiState::button_rects` for the
    /// button placed at `area` — the same pair [`Button::render`] returns,
    /// exposed without a `Buffer` so hit-test recording (and unit tests) need
    /// not render. A click in `area` resolves back to `action` via
    /// [`hit_test_button`], which is what keeps render and hit-test in
    /// lockstep.
    pub fn pair(&self, area: Rect) -> (Action, Rect) {
        (self.action.clone(), area)
    }

    /// Styling for the button text — normal for an enabled button, dimmed for
    /// a disabled one.
    fn style(&self, palette: &ColorPalette) -> Style {
        if self.enabled {
            Style::default()
                .fg(palette.text_primary)
                .bg(palette.tab_bar_bg)
        } else {
            Style::default()
                .fg(palette.text_muted)
                .bg(palette.tab_bar_bg)
                .add_modifier(Modifier::DIM)
        }
    }

    /// Render `text` (an already-chosen full or shortcut-only label) into `buf`
    /// at `area` and return the `(Action, Rect)` pair to record in
    /// `UiState::button_rects`. Keeping render and the recorded rect in one
    /// call is what stops the click target from drifting from what's drawn.
    fn render_text(
        &self,
        text: &str,
        area: Rect,
        buf: &mut Buffer,
        palette: &ColorPalette,
    ) -> (Action, Rect) {
        let span = Span::styled(text.to_string(), self.style(palette));
        buf.set_span(area.x, area.y, &span, area.width);
        self.pair(area)
    }

    /// Render the full `[Label Shortcut]` button into `buf` at `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer, palette: &ColorPalette) -> (Action, Rect) {
        self.render_text(&self.display_label(), area, buf, palette)
    }

    /// Render the narrow-terminal `[Shortcut]` fallback into `buf` at `area`.
    pub fn render_compact(
        &self,
        area: Rect,
        buf: &mut Buffer,
        palette: &ColorPalette,
    ) -> (Action, Rect) {
        self.render_text(&self.shortcut_only_label(), area, buf, palette)
    }
}

/// PRD #80: hit-test a mouse Down/Up position against the button rects recorded
/// during render. Returns the [`Action`] of the first button whose rect
/// contains the point, or `None` if the click misses every button — in which
/// case the caller falls through to the existing pane/selection/scroll/
/// hyperlink logic. Per the PRD's hit-test order, this runs BEFORE pane-region
/// logic so a button click short-circuits.
pub fn hit_test_button(button_rects: &[(Action, Rect)], col: u16, row: u16) -> Option<Action> {
    button_rects
        .iter()
        .find_map(|(action, rect)| point_in_rect(rect, col, row).then(|| action.clone()))
}

/// Whether the cell `(col, row)` falls inside `rect` (upper bounds exclusive).
fn point_in_rect(rect: &Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// PRD #80 M3: hit-test a click against the tab `[×]` close rects, returning a
/// [`Action::CloseTab`] for the first match. Checked AFTER `button_rects` but
/// BEFORE the header rects so the `[×]` beats the surrounding tab header.
fn hit_test_tab_close(tab_close_rects: &[(usize, Rect)], col: u16, row: u16) -> Option<Action> {
    tab_close_rects
        .iter()
        .find_map(|(idx, rect)| point_in_rect(rect, col, row).then_some(Action::CloseTab(*idx)))
}

/// PRD #80 M3: hit-test a click against the tab header rects, returning a
/// [`Action::SelectTab`] for the first match (a click that missed the `[×]`).
fn hit_test_tab_header(tab_header_rects: &[(usize, Rect)], col: u16, row: u16) -> Option<Action> {
    tab_header_rects
        .iter()
        .find_map(|(idx, rect)| point_in_rect(rect, col, row).then_some(Action::SelectTab(*idx)))
}

/// PRD #80 M4: hit-test a click against the dashboard card rects, returning the
/// flat selection index of the first card whose rect contains the point.
/// Checked AFTER the button/tab affordances; single vs double click handling
/// (select vs focus) is decided by the caller using `UiState::last_click`.
fn hit_test_card(card_rects: &[(usize, Rect)], col: u16, row: u16) -> Option<usize> {
    card_rects
        .iter()
        .find_map(|(idx, rect)| point_in_rect(rect, col, row).then_some(*idx))
}

/// PRD #80 M3: close the tab at `idx` and reconcile shared state, reusing the
/// same teardown the Ctrl+W tab-close path performs — unregister every
/// successfully-closed pane, drop the matching sessions (keeping any that
/// failed to close so the user can retry), clean their metadata, and resweep
/// the dashboard layout. Used by [`Action::CloseTab`] (a `[×]` click); the
/// Ctrl+W card path keeps its own inline message that names the specific pane.
fn close_tab_by_index(
    idx: usize,
    ui: &mut UiState,
    state: &SharedState,
    tab_manager: &mut TabManager,
    pane: &dyn PaneController,
    frame_area: Rect,
) {
    match tab_manager.close_tab(idx) {
        Ok(outcome) => {
            let mut st = state.blocking_write();
            for id in &outcome.closed {
                st.unregister_pane(id);
            }
            // Drop only the sessions whose pane was actually closed; failed
            // panes keep their card/session so the user can retry.
            let closed_set: std::collections::HashSet<&str> =
                outcome.closed.iter().map(String::as_str).collect();
            st.sessions.retain(|_, s| {
                s.pane_id
                    .as_ref()
                    .is_none_or(|pid| !closed_set.contains(pid.as_str()))
            });
            drop(st);
            for id in &outcome.closed {
                ui.pane_metadata.remove(id);
            }
            if outcome.is_clean() {
                ui.status_message = Some(("Closed tab".to_string(), std::time::Instant::now()));
            } else {
                let failed_ids: Vec<&str> =
                    outcome.failed.iter().map(|(id, _)| id.as_str()).collect();
                let first_err = outcome
                    .failed
                    .first()
                    .map(|(_, e)| e.as_str())
                    .unwrap_or("");
                for (id, e) in &outcome.failed {
                    tracing::warn!(
                        pane_id = %id,
                        error = %e,
                        "M3: close_pane failed during [×] tab teardown — card preserved"
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
            resize_dashboard_panes(pane, ui, tab_manager, frame_area);
        }
        Err(e) => {
            ui.status_message = Some((
                format!("Failed to close tab: {e}"),
                std::time::Instant::now(),
            ));
        }
    }
    // Closing the active mode/orchestration tab returns focus to the
    // dashboard, so leave PaneInput just like the Ctrl+W path does.
    if ui.mode == UiMode::PaneInput {
        ui.mode = UiMode::Normal;
    }
    if ui.selected_index > 0 {
        ui.selected_index = ui.selected_index.saturating_sub(1);
    }
}

/// PRD #92 F1 / PRD #80 M5: finalise Stop — tell the daemon to terminate every
/// managed agent and exit. Returns [`Flow::Break`] on success (the TUI's outer
/// loop unwinds into the normal session-save + exit path); on a shutdown error
/// it surfaces the failure, returns to Normal mode, and yields
/// [`Flow::Continue`] so the user can retry or Detach. Shared by the
/// `Action::StopAndQuit` arm (Enter/`y` on Yes) and `Action::RequestStop`
/// (the `[Stop]` button) when there are no managed agents.
fn perform_stop_and_quit(ui: &mut UiState, pane: &dyn PaneController) -> Flow {
    let shutdown_result = pane
        .as_any()
        .downcast_ref::<EmbeddedPaneController>()
        .map(|embedded| embedded.shutdown_daemon());
    match shutdown_result {
        Some(Ok(())) | None => Flow::Break,
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
            ui.mode = UiMode::Normal;
            ui.stop_confirm_selected = 0;
            ui.quit_confirm_selected = 0;
            Flow::Continue
        }
    }
}

/// PRD #80 M5: send the config-gen prompt to `pane_id`, focus it for execution,
/// and surface the outcome. Shared by the `Action::SendConfigGenPrompt` arm
/// (Enter-on-Yes via the key handler) and `Action::ConfigGenConfirm` (the
/// `[Yes]` button).
fn send_config_gen_prompt(
    pane_id: &str,
    cwd: &str,
    ui: &mut UiState,
    pane: &dyn PaneController,
    tab_manager: &mut TabManager,
    frame_area: Rect,
) {
    let prompt = crate::config_gen::config_gen_prompt(cwd);
    match pane.write_to_pane(pane_id, &prompt) {
        Ok(()) => {
            // Focus the pane so the user can press Enter to execute.
            if let Some(tab_idx) = tab_manager.tab_index_for_pane(pane_id) {
                tab_manager.switch_to(tab_idx);
                resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                resize_mode_tab_panes(pane, tab_manager, frame_area);
            }
            let _ = pane.focus_pane(pane_id);
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

/// PRD #80 M6: commit a rename for the selected card — push `new_name` to the
/// daemon via `PaneController::rename_pane` and mirror the controller-resolved
/// outcome into the dashboard's display-name maps. Shared by the `Rename`-mode
/// Enter key (via `run_tui`) and the `[Save]` button (`Action::SaveRename`) so
/// click and key resolve identically. Best-effort: `rename_pane`'s own error
/// path logs and swallows transient daemon failures.
fn commit_rename(
    new_name: &str,
    ui: &mut UiState,
    pane: &dyn PaneController,
    snapshot: &AppState,
    selected_id: Option<&str>,
) {
    if let Some(sid) = selected_id
        && let Some(session) = snapshot.sessions.get(sid)
        && let Some(ref pane_id) = session.pane_id
    {
        match pane.rename_pane(pane_id, new_name) {
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
}

/// PRD #80: the single funnel. Every command [`Action`] — whether it came from
/// a keystroke (today) or a button click (M2 on) — executes here and nowhere
/// else. The keystroke branch in `run_tui` is a thin `KeyEvent -> Option<
/// Action>` mapper that hands the chosen action to this function. New actions
/// add one arm here and one mapping entry; that is the structural guarantee
/// that key and click stay in parity.
///
/// `frame_area` is the current frame area captured by the caller (constant
/// within a render iteration), standing in for the `terminal.get_frame().
/// area()` reads the inlined code used. Returns [`Flow::Break`] when the action
/// should break the TUI's outer loop (quit / detach-and-quit / stop-and-quit).
#[allow(clippy::too_many_arguments)]
fn dispatch_action(
    action: Action,
    ui: &mut UiState,
    pane: &dyn PaneController,
    state: &SharedState,
    tab_manager: &mut TabManager,
    snapshot: &AppState,
    filtered: &[(&String, &SessionState)],
    selected_id: Option<&str>,
    frame_area: Rect,
) -> Flow {
    match action {
        // ===== PRD #80 global command actions (formerly inline in run_tui) =====
        // Ctrl+n: new pane (open directory picker).
        Action::NewPane => {
            ui.mode = UiMode::DirPicker;
            ui.dir_picker = Some(DirPickerState::new(
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            ));
        }
        // Ctrl+t: toggle layout.
        Action::ToggleLayout => {
            ui.pane_layout = match ui.pane_layout {
                PaneLayout::Stacked => PaneLayout::Tiled,
                PaneLayout::Tiled => PaneLayout::Stacked,
            };
            let mode_name = match ui.pane_layout {
                PaneLayout::Stacked => "stacked",
                PaneLayout::Tiled => "tiled",
            };
            // PRD #76 M2.15 fixup F2: route through the same SSOT sweep helpers
            // that handle spawn / tab-switch resize.
            if tab_manager.active_mode_name().is_some() {
                resize_mode_tab_panes(pane, tab_manager, frame_area);
            } else {
                resize_dashboard_panes(pane, ui, tab_manager, frame_area);
            }
            ui.status_message = Some((format!("Layout: {mode_name}"), std::time::Instant::now()));
        }
        // Ctrl+d: enter Normal (command) mode, stay on current tab.
        Action::DetachToNormal => {
            // Re-suppress the prompt in reactive panes when leaving PaneInput
            // so automated output stays clean.
            if ui.mode == UiMode::PaneInput
                && let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>()
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
        }
        // `?` / [Help ?] button: open the help overlay, or close it if already
        // open. Both key and button reach this one arm.
        Action::ToggleHelp => {
            ui.mode = if ui.mode == UiMode::Help {
                UiMode::Normal
            } else {
                UiMode::Help
            };
        }
        // Ctrl+w: close selected pane (or entire mode tab if it's the agent pane).
        //
        // PRD #92 F4: inspect each close result and preserve the card / session
        // on failure so the user can see the error and retry.
        Action::CloseSelected => {
            if let Some(sid) = filtered.get(ui.selected_index).map(|(id, _)| (*id).clone())
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
                    match tab_manager.close_tab(tab_idx) {
                        Ok(outcome) => {
                            let mut st = state.blocking_write();
                            for id in &outcome.closed {
                                st.unregister_pane(id);
                            }
                            // Remove sessions whose pane_id is in the closed set ONLY.
                            let closed_set: std::collections::HashSet<&str> =
                                outcome.closed.iter().map(String::as_str).collect();
                            st.sessions.retain(|_, s| {
                                s.pane_id
                                    .as_ref()
                                    .is_none_or(|pid| !closed_set.contains(pid.as_str()))
                            });
                            drop(st);
                            // Clean pane_metadata only for the successfully-closed panes.
                            for id in &outcome.closed {
                                ui.pane_metadata.remove(id);
                            }
                            if outcome.is_clean() {
                                ui.status_message = Some((
                                    format!("Closed tab containing pane {closed_pane_id}"),
                                    std::time::Instant::now(),
                                ));
                            } else {
                                let failed_ids: Vec<&str> =
                                    outcome.failed.iter().map(|(id, _)| id.as_str()).collect();
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
                            resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                        }
                        Err(e) => {
                            ui.status_message = Some((
                                format!("Failed to close tab: {e}"),
                                std::time::Instant::now(),
                            ));
                        }
                    }
                } else {
                    // Plain dashboard pane — close just this one and inspect the result.
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
                // Clamp selected_index so it doesn't point past the now-shorter list.
                if ui.selected_index > 0 {
                    ui.selected_index = ui.selected_index.saturating_sub(1);
                }
            }
        }
        // Ctrl+PageDown: next tab (clamped, gated on a visible tab bar).
        Action::GlobalNextTab => {
            if tab_manager.show_tab_bar() {
                let prev_idx = tab_manager.active_index();
                tab_manager.switch_to(prev_idx + 1);
                if prev_idx != tab_manager.active_index() {
                    resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                    resize_mode_tab_panes(pane, tab_manager, frame_area);
                }
            }
        }
        // Ctrl+PageUp: previous tab (clamped, gated on a visible tab bar).
        Action::GlobalPrevTab => {
            if tab_manager.show_tab_bar() {
                let prev_idx = tab_manager.active_index();
                if prev_idx > 0 {
                    tab_manager.switch_to(prev_idx - 1);
                    if prev_idx != tab_manager.active_index() {
                        resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                        resize_mode_tab_panes(pane, tab_manager, frame_area);
                    }
                }
            }
        }
        // Normal-mode Tab / Right / l: cycle to the next tab (wraps).
        Action::CycleTabNext => {
            let count = tab_manager.tab_count();
            if count > 0 {
                let prev_idx = tab_manager.active_index();
                let next = (prev_idx + 1) % count;
                tab_manager.switch_to(next);
                if prev_idx != tab_manager.active_index() {
                    resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                    resize_mode_tab_panes(pane, tab_manager, frame_area);
                }
            }
        }
        // Normal-mode BackTab / Left / h: cycle to the previous tab (wraps).
        Action::CycleTabPrev => {
            let count = tab_manager.tab_count();
            if count > 0 {
                let prev_idx = tab_manager.active_index();
                let prev = (prev_idx + count - 1) % count;
                tab_manager.switch_to(prev);
                if prev_idx != tab_manager.active_index() {
                    resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                    resize_mode_tab_panes(pane, tab_manager, frame_area);
                }
            }
        }
        // PRD #80 M3: click a tab header → switch to that tab (same resize
        // sweep as the keyboard tab-switch paths).
        Action::SelectTab(idx) => {
            let prev_idx = tab_manager.active_index();
            tab_manager.switch_to(idx);
            if prev_idx != tab_manager.active_index() {
                resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                resize_mode_tab_panes(pane, tab_manager, frame_area);
            }
        }
        // PRD #80 M3: click a tab's [×] → close that tab, reusing Ctrl+W's
        // tab-teardown semantics for the clicked tab.
        Action::CloseTab(idx) => {
            close_tab_by_index(idx, ui, state, tab_manager, pane, frame_area);
        }
        // PRD #80 M4: single-click a card → select exactly that card (PRD #68),
        // mirroring the move into the embedded focus so the per-frame
        // "selected_index ← focused pane" sync doesn't roll it back — the same
        // pattern `dispatch_normal_mode_key` uses for j/k.
        Action::SelectCard(idx) => {
            if idx < filtered.len() {
                let prev = ui.selected_index;
                ui.selected_index = idx;
                mirror_selection_into_focus(prev, ui, filtered, pane);
            }
        }
        // PRD #80 M4: `/` key or [Filter /] button → filter mode.
        Action::EnterFilter => {
            ui.mode = UiMode::Filter;
            ui.filter_text.clear();
        }
        // PRD #80 M4: `r` key or [Rename r] button → rename mode for the
        // selected card. No-op with no cards, matching the `r` key's guard.
        Action::EnterRename => {
            if !filtered.is_empty() {
                ui.mode = UiMode::Rename;
                ui.rename_text.clear();
            }
        }
        // Normal-mode digit 1-9: jump to card N and focus its pane.
        Action::FocusCard(idx) => {
            // Dismiss idle art on the target card.
            if let Some((sid, _)) = filtered.get(idx)
                && let Some(entry) = ui.idle_art_cache.get_mut(*sid)
            {
                entry.dismissed = true;
            }
            focus_deck(idx, ui, filtered, snapshot, state, pane);
            resize_dashboard_panes(pane, ui, tab_manager, frame_area);
        }
        // Mode tab in-tab navigation (j/Down): move side-pane focus down.
        Action::ModeTabSelectNext => {
            if let Tab::Mode {
                focused_side_pane_index,
                mode_manager,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                let side_ids = mode_manager.managed_pane_ids();
                let side_count = side_ids.len();
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
                let focus_id = match *focused_side_pane_index {
                    None => agent_pane_id.clone(),
                    Some(i) => side_ids
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| agent_pane_id.clone()),
                };
                let _ = pane.focus_pane(&focus_id);
            }
        }
        // Mode tab in-tab navigation (k/Up): move side-pane focus up.
        Action::ModeTabSelectPrev => {
            if let Tab::Mode {
                focused_side_pane_index,
                mode_manager,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                let side_ids = mode_manager.managed_pane_ids();
                let side_count = side_ids.len();
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
            }
        }
        // Mode tab in-tab navigation (Enter): focus the selected side/agent pane.
        Action::ModeTabFocus => {
            if let Tab::Mode {
                focused_side_pane_index,
                mode_manager,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                let side_ids = mode_manager.managed_pane_ids();
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
                    // Restore a minimal prompt so the user can interact with the
                    // shell in this reactive pane.
                    if is_reactive {
                        let _ = pane
                            .write_to_pane(&target_pane_id, "export PS1='$ ' PS2='> ' PROMPT='$ '");
                    }
                    ui.status_message = Some((
                        "PaneInput mode — type to interact, Ctrl+d for dashboard".to_string(),
                        std::time::Instant::now(),
                    ));
                }
            }
        }
        // Mode tab in-tab navigation (Esc): reset focus back to the agent pane.
        Action::ModeTabReset => {
            if let Tab::Mode {
                focused_side_pane_index,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                *focused_side_pane_index = None;
                let _ = pane.focus_pane(agent_pane_id);
            }
        }
        Action::Quit => return Flow::Break,
        Action::DetachAndQuit => {
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
            return Flow::Break;
        }
        Action::StopConfirmPrompt { agent_count } => {
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
        Action::StopAndQuit => {
            // PRD #92 F1: user confirmed Stop (either via the primary dialog
            // with 0 agents or via the secondary y/n with Yes). The shutdown +
            // error-recovery sequence is shared with the `[Stop]` button (see
            // [`perform_stop_and_quit`]).
            return perform_stop_and_quit(ui, pane);
        }
        Action::Focus => {
            // Dismiss idle art on the focused card
            if let Some(sid) = selected_id
                && let Some(entry) = ui.idle_art_cache.get_mut(sid)
            {
                entry.dismissed = true;
            }
            if let Some(sid) = selected_id
                && let Some(session) = snapshot.sessions.get(sid)
            {
                if let Some(ref pane_id) = session.pane_id {
                    if let Some(tab_idx) = tab_manager.tab_index_for_pane(pane_id) {
                        tab_manager.switch_to(tab_idx);
                        let area = frame_area;
                        resize_dashboard_panes(pane, ui, tab_manager, area);
                        resize_mode_tab_panes(pane, tab_manager, area);
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
        Action::RequestConfigGen => {
            if let Some(sid) = selected_id
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
        Action::SendPermissionResponse(approve) => {
            // The handler already gated on `WaitingForInput` plus a
            // non-empty selection; re-resolve the pane here so the
            // PTY write goes to the right place. If `pane_id` is
            // missing (placeholder session with no agent yet), this
            // silently no-ops — same shape as RequestConfigGen.
            if let Some(sid) = selected_id
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
        Action::SpawnPane(req) => {
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
                                    st.pane_role_map
                                        .insert(role_pane_ids[i].clone(), role.name.clone());
                                    st.pane_cwd_map
                                        .insert(role_pane_ids[i].clone(), dir_str.clone());
                                    if role.start {
                                        st.orchestrator_pane_ids.insert(role_pane_ids[i].clone());
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
                            let area = frame_area;
                            resize_dashboard_panes(pane, ui, tab_manager, area);

                            // Record creation time for delayed prompt injection fallback.
                            if let Tab::Orchestration { id, .. } = tab_manager.active_tab() {
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
                    let tab_membership = req.mode_config.as_ref().map(|m| TabMembership::Mode {
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
                    let (spawn_rows, spawn_cols) = if req.mode_config.is_some() {
                        mode_agent_pane_dims(frame_area)
                    } else {
                        let embedded_pane_count = pane
                            .as_any()
                            .downcast_ref::<EmbeddedPaneController>()
                            .map(|e| e.pane_ids().len())
                            .unwrap_or(0);
                        let pane_count_after = (embedded_pane_count as u16).saturating_add(1);
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
                                let total_side_count =
                                    (mode_config.panes.len() + mode_config.reactive_panes) as u16;
                                let side_pane_dims =
                                    mode_side_pane_dims(frame_area, total_side_count);
                                match tab_manager.open_mode_tab(
                                    &mode_config,
                                    &dir_str,
                                    new_id.clone(),
                                    side_pane_dims,
                                ) {
                                    Ok((_tab_idx, side_ids)) => {
                                        for id in &side_ids {
                                            state.blocking_write().register_pane(id.clone());
                                        }
                                        let _ = pane.focus_pane(&new_id);
                                        ui.mode = UiMode::PaneInput;
                                        // PRD #76 M2.15: shared
                                        // layout helpers — spawn
                                        // and resize math are now
                                        // identical.
                                        resize_mode_tab_panes_for(
                                            pane, &new_id, &side_ids, frame_area,
                                        );
                                        // Start commands now that panes are correctly sized
                                        let _ = tab_manager.start_mode_commands();
                                        // Send the agent pane command after resize
                                        // so it starts at the correct PTY dimensions.
                                        if let Some(ref init_cmd) = mode_config.init_command {
                                            let _ = pane.write_to_pane(&new_id, init_cmd);
                                        }
                                        if let Some(saved) = ui.pane_metadata.get(&new_id) {
                                            let agent_cmd = saved.command.clone();
                                            if !agent_cmd.is_empty() {
                                                let _ = pane.write_to_pane(&new_id, &agent_cmd);
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
                                resize_dashboard_panes(pane, ui, tab_manager, frame_area);
                                ui.status_message = Some((
                                    format!("Created pane {new_id} in {dir_str}"),
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        Err(e) => {
                            ui.status_message =
                                Some((format!("New pane failed: {e}"), std::time::Instant::now()));
                        }
                    }
                } // close else (non-orchestration path)
            }
        }
        Action::SendConfigGenPrompt { pane_id, cwd } => {
            send_config_gen_prompt(&pane_id, &cwd, ui, pane, tab_manager, frame_area);
        }
        // ===== PRD #80 M5: modal button actions =====
        // quit-confirm [Stop]: resolve like Enter-on-Stop — straight to
        // shutdown with no managed agents, else the secondary Stop dialog.
        Action::RequestStop => {
            let count = snapshot
                .sessions
                .values()
                .filter(|s| s.pane_id.is_some())
                .count();
            if count == 0 {
                return perform_stop_and_quit(ui, pane);
            }
            ui.stop_confirm_agent_count = count;
            ui.stop_confirm_selected = 0;
            ui.mode = UiMode::StopConfirm;
        }
        // quit-confirm [Cancel]: return to the dashboard.
        Action::DismissModal => {
            ui.mode = UiMode::Normal;
        }
        // config-gen [Yes]: send the prompt to the pending target pane.
        Action::ConfigGenConfirm => {
            ui.mode = UiMode::Normal;
            if let Some((pane_id, cwd)) = ui.config_gen_target.take() {
                send_config_gen_prompt(&pane_id, &cwd, ui, pane, tab_manager, frame_area);
            }
        }
        // config-gen [No]: dismiss for now (hint stays on the card).
        Action::ConfigGenDismiss => {
            ui.config_gen_target = None;
            ui.mode = UiMode::Normal;
        }
        // config-gen [Never]: suppress the prompt for this directory.
        Action::ConfigGenSuppress => {
            if let Some((_, ref cwd)) = ui.config_gen_target {
                ui.config_gen_state.suppress_dir(cwd);
            }
            ui.config_gen_target = None;
            ui.mode = UiMode::Normal;
            ui.status_message = Some((
                "Config prompt suppressed for this directory.".to_string(),
                std::time::Instant::now(),
            ));
        }
        // star-prompt [Star]: open the repo and stop asking (== `s`).
        Action::StarConfirm => {
            let msg = if open::that("https://github.com/vfarcic/dot-agent-deck").is_ok() {
                "Thanks for starring! ⭐".to_string()
            } else {
                "Visit github.com/vfarcic/dot-agent-deck to star ⭐".to_string()
            };
            ui.star_prompt_state.dismiss_permanently();
            ui.mode = UiMode::Normal;
            ui.status_message = Some((msg, std::time::Instant::now()));
        }
        // star-prompt [Snooze]: snooze (== `l` / Esc).
        Action::StarSnooze => {
            ui.star_prompt_state.snooze();
            ui.mode = UiMode::Normal;
        }
        // star-prompt [Dismiss]: stop asking permanently (== `d`).
        Action::StarDismiss => {
            ui.star_prompt_state.dismiss_permanently();
            ui.mode = UiMode::Normal;
        }
        // ===== PRD #80 M6: inline-edit button actions =====
        // filter [Apply]: commit and keep the typed filter (== Enter).
        Action::ApplyFilter => {
            ui.mode = UiMode::Normal;
        }
        // filter [Cancel]: clear the filter (== Esc).
        Action::CancelFilter => {
            ui.filter_text.clear();
            ui.mode = UiMode::Normal;
        }
        // rename [Save]: commit the rename on the selected card (== Enter).
        Action::SaveRename => {
            let new_name = ui.rename_text.clone();
            ui.rename_text.clear();
            ui.mode = UiMode::Normal;
            commit_rename(&new_name, ui, pane, snapshot, selected_id);
        }
        // rename [Cancel]: abandon, leaving the existing name (== Esc).
        Action::CancelRename => {
            ui.rename_text.clear();
            ui.mode = UiMode::Normal;
        }
        // ===== PRD #80 M7: directory-picker click actions =====
        // single-click a row → select it (== j/k landing on it).
        Action::PickerSelectRow(idx) => {
            if let Some(picker) = ui.dir_picker.as_mut()
                && idx < picker.filtered_indices.len()
            {
                picker.selected = idx;
            }
        }
        // double-click a row → select then descend (== Enter / l).
        Action::PickerEnterRow(idx) => {
            let Some(picker) = ui.dir_picker.as_mut() else {
                return Flow::Continue;
            };
            if idx < picker.filtered_indices.len() {
                picker.selected = idx;
            }
            // Mirror the l/Enter key arm: with no subdirs, confirm the current
            // directory; otherwise descend into the (now-selected) row.
            if !picker.has_subdirs() {
                transition_after_dir_pick(ui);
            } else if !picker.filtered_indices.is_empty() {
                picker.enter_selected();
            }
        }
        // click the `..` row / breadcrumb → go up (== h / Backspace / Left).
        Action::PickerParent => {
            if let Some(picker) = ui.dir_picker.as_mut() {
                picker.go_up();
            }
        }
        // [Confirm] → confirm the current directory → new-pane form (== Space).
        Action::PickerConfirm => {
            transition_after_dir_pick(ui);
        }
        // [Cancel] → close the picker (== q / Esc).
        Action::PickerCancel => {
            ui.dir_picker = None;
            ui.mode = UiMode::Normal;
        }
        // [Filter] → open the picker's filter input (== `/`).
        Action::PickerFilter => {
            if let Some(picker) = ui.dir_picker.as_mut() {
                picker.filtering = true;
            }
        }
        Action::ForwardToPane(bytes) => {
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
        Action::Continue => {}
    }
    Flow::Continue
}

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
                // PRD #128 Direction B-1 — record the moment SessionStart
                // was first observed for this orchestration so the
                // readiness buffer can be measured from it. The timeout
                // path bypasses the buffer because by then Claude Code
                // either fired SessionStart far earlier or never will, so
                // any further wait would just deepen the user-visible
                // hang.
                if agent_ready {
                    ui.orchestration_ready_since
                        .entry(*id)
                        .or_insert_with(std::time::Instant::now);
                }
                let buffer_elapsed = if timeout_ready {
                    true
                } else {
                    should_inject_spawn_time_prompt(
                        ui.orchestration_ready_since.get(id).copied(),
                        std::time::Instant::now(),
                    )
                };
                if (agent_ready || timeout_ready) && buffer_elapsed {
                    if let Some(prompt) = orchestrator_prompt.take() {
                        // PRD #128 audit S2: emit a one-shot operator-visible
                        // log right before the write fires. Distinct target
                        // from `pane_write` so an operator can flip on the
                        // buffer trail without also enabling the per-byte
                        // trace. `debug!` (not `trace!`) because this is an
                        // operational, once-per-spawn event.
                        if let Some(rs) = ui.orchestration_ready_since.get(id) {
                            tracing::debug!(
                                target: "spawn_time_buffer",
                                elapsed_ms = rs.elapsed().as_millis() as u64,
                                pane_id = %start_pane_id,
                                "spawn-time readiness buffer elapsed; proceeding with role prompt write"
                            );
                        }
                        let _ = pane.write_and_submit_to_pane(start_pane_id, &prompt);
                    }
                    role_statuses[*start_role_index] = OrchestrationRoleStatus::Working;
                    ui.orchestration_prompted.insert(*id);
                    // PRD #128 audit N1: the ready-since timestamp is
                    // load-bearing only between SessionStart and the
                    // buffered write. Once the write fires this entry is
                    // dead state — drop it so the map size matches the
                    // count of pending orchestration spawns rather than
                    // accumulating over the session.
                    ui.orchestration_ready_since.remove(id);
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
                // PRD #80 M2/M3: clickable affordances take precedence. A
                // left-button Down on a recorded rect fires its Action through
                // the SAME `dispatch_action` funnel as the keystroke, then
                // short-circuits the rest of mouse handling. The paired Up on a
                // rect is consumed (without re-dispatching) so it can't fall
                // through to the selection/copy path. Scroll events are not
                // Down/Up, so they bypass this layer entirely and reach the
                // forwarding branch unchanged.
                //
                // Hit-test order: button bar FIRST, then each tab's `[×]`
                // close rect (so `[×]` beats the surrounding header), then the
                // tab header rects (switch). A miss falls through to the
                // existing pane/selection/scroll logic below.
                let is_down = matches!(
                    mouse.kind,
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
                );
                let is_up = matches!(
                    mouse.kind,
                    crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left)
                );

                // PRD #80 M7: the directory picker is a topmost overlay — when
                // it's open, its [Confirm]/[Cancel]/[Filter] buttons and row
                // rects are the only click targets, hit-tested here and
                // exclusively (a miss inside the picker is consumed, never
                // falling through to the dashboard behind it).
                if ui.mode == UiMode::DirPicker && (is_down || is_up) {
                    let col = mouse.column;
                    let row = mouse.row;
                    // Buttons first.
                    let mut picker_action = hit_test_button(&ui.picker_button_rects, col, row);
                    // Then rows (single = select, double = enter, `..` = up).
                    if picker_action.is_none()
                        && let Some(&(i, _)) = ui
                            .picker_row_rects
                            .iter()
                            .find(|(_, r)| point_in_rect(r, col, row))
                    {
                        // Is this the parent (`..`) row? Then a click of any
                        // kind navigates up.
                        let is_parent = ui.dir_picker.as_ref().is_some_and(|p| {
                            p.filtered_indices
                                .get(i)
                                .and_then(|&ei| p.entries.get(ei))
                                .map(|e| e == Path::new(".."))
                                .unwrap_or(false)
                        });
                        if is_parent {
                            picker_action = Some(Action::PickerParent);
                        } else {
                            // Single vs double click via the shared last_click
                            // multi-click discrimination (400ms / same row /
                            // within 3 cols).
                            let now = std::time::Instant::now();
                            let click_count = if let Some((t, lc, lr, cnt)) = ui.last_click {
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
                            if is_down {
                                ui.last_click = Some((now, col, row, click_count));
                            }
                            picker_action = Some(if click_count >= 2 {
                                Action::PickerEnterRow(i)
                            } else {
                                Action::PickerSelectRow(i)
                            });
                        }
                    }
                    if let Some(action) = picker_action
                        && is_down
                    {
                        let frame_area = terminal.get_frame().area();
                        let flow = dispatch_action(
                            action,
                            &mut ui,
                            &*pane,
                            &state,
                            &mut tab_manager,
                            &snapshot,
                            &filtered,
                            None,
                            frame_area,
                        );
                        if flow == Flow::Break {
                            break 'outer;
                        }
                    }
                    // Consume every Down/Up while the picker is open — don't
                    // fall through to the dashboard pane/selection logic.
                    if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }

                // PRD #80 M5: when a modal/overlay is active it is topmost, so
                // its buttons are hit-tested FIRST and exclusively — the
                // bottom-bar / tab / card rects behind it are not clickable.
                // When no modal is up, fall back to the M2/M3 chain.
                let modal_active = matches!(
                    ui.mode,
                    UiMode::QuitConfirm
                        | UiMode::StopConfirm
                        | UiMode::ConfigGenPrompt
                        | UiMode::StarPrompt
                        | UiMode::Help
                );
                // PRD #80 M6: in the inline-edit modes the bottom row IS the
                // input; its [Apply]/[Cancel] / [Save]/[Cancel] buttons live in
                // `button_rects`, and any other click is consumed below so it
                // keeps the field focused instead of exiting the mode.
                let text_input_mode = matches!(ui.mode, UiMode::Filter | UiMode::Rename);
                let mouse_action = if !(is_down || is_up) {
                    None
                } else if modal_active {
                    hit_test_button(&ui.modal_button_rects, mouse.column, mouse.row)
                } else {
                    hit_test_button(&ui.button_rects, mouse.column, mouse.row)
                        .or_else(|| {
                            hit_test_tab_close(&ui.tab_close_rects, mouse.column, mouse.row)
                        })
                        .or_else(|| {
                            hit_test_tab_header(&ui.tab_header_rects, mouse.column, mouse.row)
                        })
                };
                if let Some(action) = mouse_action {
                    if is_down {
                        let frame_area = terminal.get_frame().area();
                        let selected_id: Option<String> =
                            filtered.get(ui.selected_index).map(|(id, _)| (*id).clone());
                        let flow = dispatch_action(
                            action,
                            &mut ui,
                            &*pane,
                            &state,
                            &mut tab_manager,
                            &snapshot,
                            &filtered,
                            selected_id.as_deref(),
                            frame_area,
                        );
                        if flow == Flow::Break {
                            break 'outer;
                        }
                    }
                    // Consume the event (both Down and Up) — do not fall through
                    // to pane focus / selection / scroll logic.
                    if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }

                // PRD #80 M4: dashboard card click — checked AFTER the button /
                // tab affordances and BEFORE the existing pane/selection logic.
                // A single click selects exactly that card (PRD #68); a
                // double-click within the multi-click window focuses its pane
                // (the same Action::Focus the Enter key dispatches). The paired
                // Up is consumed. Cards are their own rect region, so this
                // coexists with the in-pane text-selection multi-click path.
                if !modal_active
                    && !text_input_mode
                    && (is_down || is_up)
                    && let Some(card_idx) = hit_test_card(&ui.card_rects, mouse.column, mouse.row)
                {
                    if is_down {
                        // Double-click discrimination mirrors the text-selection
                        // path: same row, within 3 columns, inside the 400ms
                        // window.
                        let now = std::time::Instant::now();
                        let click_count = if let Some((t, lc, lr, cnt)) = ui.last_click {
                            if now.duration_since(t).as_millis() < 400
                                && lr == mouse.row
                                && mouse.column.abs_diff(lc) <= 3
                            {
                                (cnt + 1).min(3)
                            } else {
                                1
                            }
                        } else {
                            1
                        };
                        ui.last_click = Some((now, mouse.column, mouse.row, click_count));

                        let frame_area = terminal.get_frame().area();
                        // Single click → select exactly this card.
                        dispatch_action(
                            Action::SelectCard(card_idx),
                            &mut ui,
                            &*pane,
                            &state,
                            &mut tab_manager,
                            &snapshot,
                            &filtered,
                            None,
                            frame_area,
                        );
                        // Double click → focus the now-selected card's pane.
                        if click_count >= 2 {
                            let selected_id: Option<String> =
                                filtered.get(ui.selected_index).map(|(id, _)| (*id).clone());
                            let flow = dispatch_action(
                                Action::Focus,
                                &mut ui,
                                &*pane,
                                &state,
                                &mut tab_manager,
                                &snapshot,
                                &filtered,
                                selected_id.as_deref(),
                                frame_area,
                            );
                            if flow == Flow::Break {
                                break 'outer;
                            }
                        }
                    }
                    // Consume both Down and Up — don't fall through to the
                    // pane-focus / selection / scroll logic below.
                    if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }

                // PRD #80 M6: in filter / rename mode, a click that missed the
                // [Apply]/[Cancel] / [Save]/[Cancel] buttons lands "in the
                // field" — consume it so the field stays focused (typing stays
                // keyboard) rather than falling through to pane/selection logic
                // that could exit the edit.
                if text_input_mode && (is_down || is_up) {
                    if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }

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
                    // (`Action::ForwardToPane(b"\r")`) is debounced and
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

            // ---------------------------------------------------------------
            // PRD #80: map this KeyEvent to one `Action`, then run it through
            // `dispatch_action` — the single place every command action
            // executes (keystroke today, mouse click from M2 on). The blocks
            // below are the thin `KeyEvent -> Option<Action>` mapper. Text
            // input (filter / rename / new-pane-form typing) stays
            // keyboard-driven inside the per-mode handlers, which mutate their
            // own field and return `Action::Continue`.
            // ---------------------------------------------------------------
            let frame_area = terminal.get_frame().area();
            let mut action: Option<Action> = None;

            // 1..9 in Normal mode: jump to card N and focus its pane.
            if ui.mode == UiMode::Normal
                && let KeyCode::Char(c @ '1'..='9') = key.code
                && key.modifiers == KeyModifiers::NONE
            {
                action = Some(Action::FocusCard((c as usize) - ('1' as usize)));
            }

            // Global Ctrl+key shortcuts (work from any mode / future pane focus).
            if action.is_none() && key.modifiers.contains(KeyModifiers::CONTROL) {
                action = global_ctrl_action(&key);
            }

            // Tab / Shift+Tab / Left / Right / h / l: cycle tabs in Normal mode.
            if action.is_none() && ui.mode == UiMode::Normal {
                action = cycle_tab_action(&key);
            }

            let selected_id: Option<String> =
                filtered.get(ui.selected_index).map(|(id, _)| (*id).clone());

            // On a mode tab in Normal mode, j/k navigate side panes, Enter
            // focuses, Esc resets.
            if action.is_none()
                && ui.mode == UiMode::Normal
                && matches!(tab_manager.active_tab(), Tab::Mode { .. })
            {
                action = mode_tab_nav_action(&key);
            }

            // Mode-specific key handling (only when no shortcut claimed the key).
            if action.is_none() {
                action = Some(match ui.mode {
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
                        if let Some(new_name) = commit {
                            // Shared with the `[Save]` button (Action::SaveRename).
                            commit_rename(
                                &new_name,
                                &mut ui,
                                &*pane,
                                &snapshot,
                                selected_id.as_deref(),
                            );
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
                });
            }

            let flow = dispatch_action(
                action.unwrap_or(Action::Continue),
                &mut ui,
                &*pane,
                &state,
                &mut tab_manager,
                &snapshot,
                &filtered,
                selected_id.as_deref(),
                frame_area,
            );
            if flow == Flow::Break {
                break 'outer;
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

/// PRD #80: render the top tab strip into `area`. `labels` are the per-tab
/// titles (already in tab order, Dashboard at index 0); `closeable` marks,
/// per tab, whether it carries a `[×]` close affordance (Dashboard never
/// does — see the M3 acceptance criteria). Extracted from `render_frame` so
/// both the live render and the L1 `render_tab_bar_to_buffer` seam exercise
/// the same code path, keeping render and (M3) hit-test in lockstep the way
/// the button bar does.
/// The clickable rects produced by [`render_tab_strip`], keyed by tab index
/// (Dashboard = 0). `headers` covers every visible tab (click → switch);
/// `closes` covers only the closeable tabs' `[×]` glyphs (click → close).
struct TabStripRects {
    headers: Vec<(usize, Rect)>,
    closes: Vec<(usize, Rect)>,
}

fn render_tab_strip(
    frame: &mut Frame,
    area: Rect,
    labels: &[String],
    closeable: &[bool],
    active_index: usize,
    palette: &ColorPalette,
) -> TabStripRects {
    // Fill the tab-bar row with the distinct background first.
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.tab_bar_bg)),
        area,
    );

    // Cap long labels so trailing tabs stay at least partially visible for
    // click-to-switch (same width-fitting the `Tabs` widget previously used).
    let fitted_labels = fit_tab_labels(labels, area.width);

    let base_style = Style::default()
        .fg(palette.text_muted)
        .bg(palette.tab_bar_bg);
    // Active tab: inverted colors for high contrast (matches the prior look).
    let active_style = Style::default()
        .fg(palette.terminal_bg)
        .bg(palette.text_secondary)
        .add_modifier(Modifier::BOLD);

    let mut headers = Vec::with_capacity(fitted_labels.len());
    let mut closes = Vec::new();
    let end = area.x.saturating_add(area.width);
    let buf = frame.buffer_mut();
    let mut x = area.x;

    for (i, label) in fitted_labels.iter().enumerate() {
        if x >= end {
            break;
        }
        let style = if i == active_index {
            active_style
        } else {
            base_style
        };

        // Divider between tabs (not before the first).
        if i > 0 {
            let (after, _) = buf.set_span(x, area.y, &Span::styled("│", base_style), end - x);
            x = after;
            if x >= end {
                break;
            }
        }

        let header_start = x;

        // Label segment, padded with a space on each side.
        let (after, _) = buf.set_span(
            x,
            area.y,
            &Span::styled(format!(" {label} "), style),
            end - x,
        );
        x = after;

        // Close affordance for Mode/Orchestration tabs (never the Dashboard).
        if *closeable.get(i).unwrap_or(&false) && x < end {
            let glyph_start = x;
            let (after, _) = buf.set_span(x, area.y, &Span::styled("[×]", style), end - x);
            x = after;
            closes.push((
                i,
                Rect {
                    x: glyph_start,
                    y: area.y,
                    width: x.saturating_sub(glyph_start),
                    height: 1,
                },
            ));
        }

        // The whole tab segment (label + any close glyph) is the click target
        // for switching; the `[×]` rect is hit-tested first so it wins there.
        headers.push((
            i,
            Rect {
                x: header_start,
                y: area.y,
                width: x.saturating_sub(header_start),
                height: 1,
            },
        ));
    }

    TabStripRects { headers, closes }
}

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
    // PRD #80 M4: rebuilt below only for the dashboard card grid; clearing here
    // means non-dashboard views (and the zero-card dashboard) leave no stale
    // card rects for the mouse hit-test.
    ui.card_rects.clear();

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

        // PRD #80 M3: the Dashboard tab (always index 0) carries no close
        // affordance; Mode and Orchestration tabs do. Pass the per-tab
        // closeable mask to the tab-strip renderer, and record the clickable
        // header / [×] rects for the mouse hit-test.
        let closeable: Vec<bool> = (0..tab_bar.labels.len()).map(|i| i != 0).collect();
        let strip = render_tab_strip(
            frame,
            chunks[0],
            &tab_bar.labels,
            &closeable,
            tab_bar.active_index,
            &palette,
        );
        ui.tab_header_rects = strip.headers;
        ui.tab_close_rects = strip.closes;

        (chunks[1], chunks[2])
    } else {
        // No tab strip this frame — drop any stale rects so a click can't hit a
        // tab affordance that isn't on screen.
        ui.tab_header_rects.clear();
        ui.tab_close_rects.clear();
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
        render_bottom_bar(
            frame,
            ui,
            hints_area,
            has_pane_control,
            &dashboard_context_buttons(!filtered.is_empty()),
        );

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
        render_bottom_bar(
            frame,
            ui,
            hints_area,
            has_pane_control,
            &dashboard_context_buttons(!filtered.is_empty()),
        );
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
            let card_area = col_chunks[col_idx];
            render_session_card(
                frame,
                card_area,
                session,
                tick,
                is_selected,
                display_name,
                card_number,
                density,
                palette,
                idle_art,
            );
            // PRD #80 M4: record this card's screen rect (paired with its flat
            // selection index) for the mouse hit-test. Safe to mutate `ui` here
            // — `display_name` / `idle_art` were the only live `ui` borrows and
            // their last use was the `render_session_card` call above.
            ui.card_rects.push((flat_index, card_area));
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
    render_bottom_bar(
        frame,
        ui,
        hints_area,
        has_pane_control,
        &dashboard_context_buttons(!filtered.is_empty()),
    );

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
    // PRD #80 M5/M7: rebuilt below for whichever modal/overlay is shown;
    // cleared here so a click can't hit an affordance from a prior frame once
    // the overlay closes.
    ui.modal_button_rects.clear();
    ui.picker_button_rects.clear();
    ui.picker_row_rects.clear();
    if ui.mode == UiMode::Help {
        ui.modal_button_rects = render_help_overlay(frame, active_mode_name, palette);
    }
    if ui.mode == UiMode::DirPicker {
        // Capture the picker's row/button rects after the `dir_picker` borrow
        // ends so they can be stored back on `ui`.
        let captured = ui
            .dir_picker
            .as_mut()
            .map(|picker| render_dir_picker(frame, picker, palette));
        if let Some((rows, buttons)) = captured {
            ui.picker_row_rects = rows;
            ui.picker_button_rects = buttons;
        }
    }
    if ui.mode == UiMode::NewPaneForm
        && let Some(ref form) = ui.new_pane_form
    {
        render_new_pane_form(frame, form, palette);
    }
    if ui.mode == UiMode::StarPrompt {
        ui.modal_button_rects = render_star_prompt(frame, palette);
    }
    if ui.mode == UiMode::ConfigGenPrompt {
        ui.modal_button_rects = render_config_gen_prompt(frame, ui.config_gen_selected, palette);
    }
    if ui.mode == UiMode::QuitConfirm {
        ui.modal_button_rects = render_quit_confirm(frame, ui.quit_confirm_selected, palette);
    }
    if ui.mode == UiMode::StopConfirm {
        // M5 adds no buttons to the secondary Stop-confirm dialog (not in the
        // contract); its keystrokes (y/n/Enter/Esc) remain the only path.
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

    // Full-width hints bar — mode tabs show only the global buttons (no
    // dashboard context buttons).
    render_bottom_bar(frame, ui, hints_area, has_pane_control, &[]);

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

/// PRD #80 M2: the five global commands the persistent button bar exposes,
/// each carrying its inline keyboard shortcut (so the bar doubles as the
/// legend it replaced). The pane-dependent commands (New Pane / Close /
/// Toggle Layout) are disabled — rendered dimmed — when no pane controller is
/// available; Help and Quit are always actionable. Shortcuts mirror the
/// keyboard handlers: `global_ctrl_action` (Ctrl+N/W/T), `?` → Help,
/// Ctrl+C → Quit.
fn global_bar_buttons(has_pane_control: bool) -> Vec<Button> {
    vec![
        Button::new("New Pane", "Ctrl+N", Action::NewPane, has_pane_control),
        Button::new("Close", "Ctrl+W", Action::CloseSelected, has_pane_control),
        Button::new(
            "Toggle Layout",
            "Ctrl+T",
            Action::ToggleLayout,
            has_pane_control,
        ),
        Button::new("Help", "?", Action::ToggleHelp, true),
        Button::new("Quit", "Ctrl+C", Action::Quit, true),
    ]
}

/// PRD #80 M4: the dashboard-only context buttons appended to the global bar
/// while on the dashboard in Normal mode, each carrying its inline shortcut.
/// Filter is always actionable; Rename / Generate-config act on the selected
/// card, so they're disabled (dimmed) when there are no cards — matching the
/// `r` / `g` keys' `total > 0` guard.
fn dashboard_context_buttons(has_cards: bool) -> Vec<Button> {
    vec![
        Button::new("Filter", "/", Action::EnterFilter, true),
        Button::new("Rename", "r", Action::EnterRename, has_cards),
        Button::new("Generate", "g", Action::RequestConfigGen, has_cards),
    ]
}

/// PRD #80 M2: render the persistent global button bar into `area` (one row)
/// and return the `(Action, Rect)` pairs to record in `UiState::button_rects`
/// so a later click hit-tests back to the right action. Two-tier labels: the
/// full `[Label Shortcut]` set when it fits the row width, otherwise the
/// shortcut-only `[Shortcut]` fallback (the same width-pressure approach the
/// old status legend faced). Buttons are separated by one space and laid out
/// left to right; a button that would overflow the row is dropped whole rather
/// than truncated mid-label, so every rendered button stays identifiable.
fn render_button_bar(
    frame: &mut Frame,
    palette: &ColorPalette,
    area: Rect,
    has_pane_control: bool,
    extra_buttons: &[Button],
) -> Vec<(Action, Rect)> {
    const SEP: u16 = 1;
    // Global commands first, then any context-specific buttons (e.g. the
    // dashboard's Filter / Rename / Generate). One funnel, one bar.
    let mut buttons = global_bar_buttons(has_pane_control);
    buttons.extend(extra_buttons.iter().cloned());

    // Choose full vs shortcut-only based on whether the full set fits the row.
    let full_width: u16 = buttons
        .iter()
        .map(|b| b.display_label().chars().count() as u16)
        .sum::<u16>()
        + SEP * (buttons.len().saturating_sub(1) as u16);
    let use_full = full_width <= area.width;

    let end = area.x.saturating_add(area.width);
    let mut rects = Vec::with_capacity(buttons.len());
    let buf = frame.buffer_mut();
    let mut x = area.x;
    for (i, button) in buttons.iter().enumerate() {
        if i > 0 {
            x = x.saturating_add(SEP);
        }
        let label = if use_full {
            button.display_label()
        } else {
            button.shortcut_only_label()
        };
        let w = label.chars().count() as u16;
        if x.saturating_add(w) > end {
            // Would overflow the row — drop this (and every later) button whole.
            break;
        }
        let rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        let pair = if use_full {
            button.render(rect, buf, palette)
        } else {
            button.render_compact(rect, buf, palette)
        };
        rects.push(pair);
        x = x.saturating_add(w);
    }
    rects
}

fn render_bottom_bar(
    frame: &mut Frame,
    ui: &mut UiState,
    area: Rect,
    has_pane_control: bool,
    extra_buttons: &[Button],
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
            // PRD #80 M6: inline [Apply] / [Cancel] buttons at the right edge,
            // alongside the `/ <text>` input. Typing stays keyboard; clicking
            // the field keeps it focused (handled in the mouse branch).
            let buttons = [
                Button::new("Apply", "", Action::ApplyFilter, true),
                Button::new("Cancel", "", Action::CancelFilter, true),
            ];
            ui.button_rects = render_right_aligned_buttons(frame, &buttons, area, &ui.palette);
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
            // PRD #80 M6: inline [Save] / [Cancel] buttons at the right edge,
            // alongside the `Rename: <text>` input.
            let buttons = [
                Button::new("Save", "", Action::SaveRename, true),
                Button::new("Cancel", "", Action::CancelRename, true),
            ];
            ui.button_rects = render_right_aligned_buttons(frame, &buttons, area, &ui.palette);
        }
        UiMode::PaneInput => {
            // PRD #80 M6: while interacting with a pane, keep the status
            // message (e.g. "PaneInput mode …") on the left and expose the
            // [Detach Ctrl+D] affordance at the right edge — clicking it
            // returns to the dashboard exactly as Ctrl+D does.
            if let Some((ref msg, _)) = ui.status_message {
                let line = Line::styled(msg.as_str(), Style::default().fg(Color::Yellow));
                frame.render_widget(Paragraph::new(line), area);
            }
            let buttons = [Button::new(
                "Detach",
                "Ctrl+D",
                Action::DetachToNormal,
                true,
            )];
            ui.button_rects = render_right_aligned_buttons(frame, &buttons, area, &ui.palette);
        }
        _ => {
            if let Some((ref msg, _)) = ui.status_message {
                let line = Line::styled(msg.as_str(), Style::default().fg(Color::Yellow));
                frame.render_widget(Paragraph::new(line), area);
                // A transient status message occupies the bar row this frame;
                // no buttons are drawn, so nothing is hit-testable.
                ui.button_rects.clear();
            } else {
                // PRD #80 M2: the persistent global button bar replaces the
                // legacy status legend (no duplication — each button carries
                // the same shortcut the legend used to show inline).
                let rects =
                    render_button_bar(frame, &ui.palette, area, has_pane_control, extra_buttons);
                // Preserve the "update available" badge by right-aligning it
                // after the bar when set (it's a separate notification, not the
                // removed legend text).
                if let Some(ref latest) = ui.update_available {
                    let badge = format!(
                        " Update available: v{latest} (current: v{}) ",
                        env!("DAD_VERSION")
                    );
                    let bw = badge.chars().count() as u16;
                    if bw < area.width {
                        let badge_area = Rect {
                            x: area.x + area.width - bw,
                            y: area.y,
                            width: bw,
                            height: 1,
                        };
                        frame.render_widget(
                            Paragraph::new(Line::styled(
                                badge,
                                Style::default()
                                    .fg(Color::Black)
                                    .bg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            )),
                            badge_area,
                        );
                    }
                }
                ui.button_rects = rects;
            }
        }
    }
}

/// PRD #80 M5: render a left-aligned row of modal buttons into `row` and
/// return their `(Action, Rect)` pairs for the mouse hit-test. Modal buttons
/// always use full `[Label]` labels (the popups are sized to fit them); a
/// button that would overflow `row` is dropped whole. Indented by `indent`
/// cells to align with the popup's body text.
fn render_modal_button_row(
    frame: &mut Frame,
    buttons: &[Button],
    row: Rect,
    indent: u16,
    palette: &ColorPalette,
) -> Vec<(Action, Rect)> {
    const SEP: u16 = 1;
    let end = row.x.saturating_add(row.width);
    let mut rects = Vec::with_capacity(buttons.len());
    let buf = frame.buffer_mut();
    let mut x = row.x.saturating_add(indent);
    for (i, button) in buttons.iter().enumerate() {
        if i > 0 {
            x = x.saturating_add(SEP);
        }
        let w = button.display_label().chars().count() as u16;
        if x.saturating_add(w) > end {
            break;
        }
        let rect = Rect {
            x,
            y: row.y,
            width: w,
            height: 1,
        };
        rects.push(button.render(rect, buf, palette));
        x = x.saturating_add(w);
    }
    rects
}

/// PRD #80 M6: render `buttons` right-aligned within `area` (one row) and
/// return their `(Action, Rect)` pairs. Used by the inline-edit rows
/// (filter / rename) and the PaneInput detach affordance, which keep their
/// prompt / status text on the left and place the buttons at the right edge.
fn render_right_aligned_buttons(
    frame: &mut Frame,
    buttons: &[Button],
    area: Rect,
    palette: &ColorPalette,
) -> Vec<(Action, Rect)> {
    const SEP: u16 = 1;
    let total: u16 = buttons
        .iter()
        .map(|b| b.display_label().chars().count() as u16)
        .sum::<u16>()
        + SEP * (buttons.len().saturating_sub(1) as u16);
    let bx = area.x + area.width.saturating_sub(total);
    let row = Rect {
        x: bx,
        y: area.y,
        width: total,
        height: 1,
    };
    render_modal_button_row(frame, buttons, row, 0, palette)
}

fn render_quit_confirm(
    frame: &mut Frame,
    selected: usize,
    palette: ColorPalette,
) -> Vec<(Action, Rect)> {
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

    // PRD #80 M5: explicit clickable buttons ALONGSIDE the option list above.
    // Drawn on the blank row just above the keyboard hint. [Stop] resolves
    // like Enter-on-Stop (see Action::RequestStop).
    let buttons = [
        Button::new("Detach", "", Action::DetachAndQuit, true),
        Button::new("Stop", "", Action::RequestStop, true),
        Button::new("Cancel", "", Action::DismissModal, true),
    ];
    let btn_row = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + popup_area.height.saturating_sub(3),
        width: popup_area.width.saturating_sub(2),
        height: 1,
    };
    render_modal_button_row(frame, &buttons, btn_row, 1, &palette)
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

fn render_star_prompt(frame: &mut Frame, palette: ColorPalette) -> Vec<(Action, Rect)> {
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

    // PRD #80 M5: explicit clickable buttons alongside the existing hint line
    // (the `s Star  l Later  d Don't ask again` row stays). Drawn on the last
    // inner row so neither the URL nor the hint is clobbered.
    let buttons = [
        Button::new("Star", "", Action::StarConfirm, true),
        Button::new("Snooze", "", Action::StarSnooze, true),
        Button::new("Dismiss", "", Action::StarDismiss, true),
    ];
    let btn_row = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + popup_area.height.saturating_sub(2),
        width: popup_area.width.saturating_sub(2),
        height: 1,
    };
    render_modal_button_row(frame, &buttons, btn_row, 1, &palette)
}

fn render_config_gen_prompt(
    frame: &mut Frame,
    selected: usize,
    palette: ColorPalette,
) -> Vec<(Action, Rect)> {
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

    // PRD #80 M5: explicit clickable buttons alongside the option list.
    let buttons = [
        Button::new("Yes", "", Action::ConfigGenConfirm, true),
        Button::new("No", "", Action::ConfigGenDismiss, true),
        Button::new("Never", "", Action::ConfigGenSuppress, true),
    ];
    let btn_row = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + popup_area.height.saturating_sub(3),
        width: popup_area.width.saturating_sub(2),
        height: 1,
    };
    render_modal_button_row(frame, &buttons, btn_row, 1, &palette)
}

fn render_help_overlay(
    frame: &mut Frame,
    active_mode_name: Option<&str>,
    palette: ColorPalette,
) -> Vec<(Action, Rect)> {
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

    // PRD #80 M5: explicit clickable [Close] button alongside the existing
    // "Press ? or Esc to close" hint. Drawn on the footer's blank first row.
    let close_button = [Button::new("Close", "", Action::ToggleHelp, true)];
    let btn_row = Rect {
        x: footer_area.x,
        y: footer_area.y,
        width: footer_area.width,
        height: 1,
    };
    render_modal_button_row(frame, &close_button, btn_row, 2, &palette)
}

/// PRD #80 M7: clickable geometry returned by [`render_dir_picker`] — the row
/// rects (each paired with its index into `DirPickerState.filtered_indices`)
/// and the `[Confirm]`/`[Cancel]`/`[Filter]` button rects.
type PickerClickTargets = (Vec<(usize, Rect)>, Vec<(Action, Rect)>);

/// Renders the directory picker and returns its clickable geometry:
/// `(row_rects, button_rects)`. `row_rects` pairs each visible entry's screen
/// `Rect` with its index into `filtered_indices` (matching
/// `DirPickerState.selected`); `button_rects` carries the `[Confirm]` /
/// `[Cancel]` / `[Filter]` affordances. PRD #80 M7.
fn render_dir_picker(
    frame: &mut Frame,
    picker: &mut DirPickerState,
    palette: ColorPalette,
) -> PickerClickTargets {
    let area = frame.area();
    let popup_width = 60.min(area.width.saturating_sub(4));
    let popup_height = 20u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    // Reserve lines so controls remain visible regardless of list length.
    // PRD #80 M7 adds one reserved line for the [Confirm]/[Cancel]/[Filter]
    // button row.
    let show_filter_row = picker.filtering || !picker.filter_text.is_empty();
    let mut reserved_lines = 6; // current dir + blank + blank + button row + two footers
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

    // Screen y of line index `n` within the popup (inside the top border).
    let line_y = |n: usize| popup_area.y + 1 + n as u16;
    let row_x = popup_area.x + 1;
    let row_width = popup_area.width.saturating_sub(2);
    let mut row_rects: Vec<(usize, Rect)> = Vec::new();

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
            let line_idx = lines.len();
            lines.push(Line::styled(format!("{prefix}{name}{suffix}"), style));
            // Record this row's clickable rect, keyed by its filtered-list
            // index `i` (== what `picker.selected` holds).
            row_rects.push((
                i,
                Rect {
                    x: row_x,
                    y: line_y(line_idx),
                    width: row_width,
                    height: 1,
                },
            ));
        }
    }

    lines.push(Line::from(""));
    // PRD #80 M7: reserved button row (rendered over after the Paragraph).
    let button_line_idx = lines.len();
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

    // PRD #80 M7: clickable [Confirm] / [Cancel] / [Filter] affordances on the
    // reserved button row, alongside the footer hints.
    let buttons = [
        Button::new("Confirm", "", Action::PickerConfirm, true),
        Button::new("Cancel", "", Action::PickerCancel, true),
        Button::new("Filter", "", Action::PickerFilter, true),
    ];
    let btn_row = Rect {
        x: row_x,
        y: line_y(button_line_idx),
        width: row_width,
        height: 1,
    };
    let button_rects = render_modal_button_row(frame, &buttons, btn_row, 1, &palette);

    (row_rects, button_rects)
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

// ---------------------------------------------------------------------------
// PRD #77 — L1 harness public surface
// ---------------------------------------------------------------------------
//
// Stable test-only entry points consumed by `tests/render_dashboard.rs`.
// They are `pub` because Rust integration tests cannot enable a crate
// feature on demand, but `#[doc(hidden)]` so they do not surface in
// `cargo doc` output — they are not a library API surface for
// downstream consumers. See PRD #77 Decision 2 for the L1 / L2 split
// and M2.1 auditor Nit 1 for why this is hidden rather than
// `pub(crate)`-gated.

/// Card density tier picked by the dashboard's adaptive layout
/// (`choose_density`). Hidden-public so L1 snapshot tests can pin a
/// specific tier rather than depending on the runtime calculation.
#[doc(hidden)]
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

impl CardDensityKind {
    /// Rendered card height in rows at this density. `wide = true`
    /// matches the in-process layout's "card is wide enough to show
    /// the stats row inline" branch; the L1 snapshot test uses this
    /// to size its `TestBackend` rather than hardcoding the value
    /// (M2.1 reviewer S3).
    #[doc(hidden)]
    pub fn rendered_height(self, wide: bool) -> u16 {
        CardDensity::from(self).card_height(wide)
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
#[doc(hidden)]
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

/// PRD #80 M2: render the persistent global button bar into a one-row
/// `Buffer` for L1 tests, mirroring [`render_card_to_buffer`]. Drives the
/// production bottom-bar renderer (`render_bottom_bar`) through a
/// `TestBackend` so a test can assert on the rendered cells without a PTY.
/// `width` is the terminal width — vary it to exercise the comfortable-width
/// full-label layout and the narrow-terminal shortcut-only fallback. The bar
/// occupies a single row, so the returned buffer is `width × 1`.
pub fn render_button_bar_to_buffer(width: u16, palette: ColorPalette) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let mut ui = UiState::new(DashboardConfig::default(), palette);
    terminal
        .draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 1,
            };
            // has_pane_control = true → the richest legacy legend today;
            // after M2 this site renders the always-visible global button bar.
            render_bottom_bar(frame, &mut ui, area, true, &[]);
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #80 M6 L1 seam: render the filter-mode bottom row (the inline filter
/// input carrying `filter_text`) into a one-row `Buffer`. After M6 this row
/// also renders the inline `[Apply]` / `[Cancel]` buttons at its right edge.
/// Mirrors [`render_button_bar_to_buffer`].
pub fn render_filter_bar_to_buffer(
    filter_text: &str,
    width: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let mut ui = UiState::new(DashboardConfig::default(), palette);
    ui.mode = UiMode::Filter;
    ui.filter_text = filter_text.to_string();
    terminal
        .draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 1,
            };
            render_bottom_bar(frame, &mut ui, area, false, &[]);
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #80 M6 L1 seam: render the rename-mode bottom row (the inline rename
/// input carrying `rename_text`) into a one-row `Buffer`. After M6 this row
/// also renders the inline `[Save]` / `[Cancel]` buttons at its right edge.
/// Mirrors [`render_button_bar_to_buffer`].
pub fn render_rename_bar_to_buffer(
    rename_text: &str,
    width: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let mut ui = UiState::new(DashboardConfig::default(), palette);
    ui.mode = UiMode::Rename;
    ui.rename_text = rename_text.to_string();
    terminal
        .draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 1,
            };
            render_bottom_bar(frame, &mut ui, area, false, &[]);
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #80 M3: render the top tab strip into a one-row `Buffer` for L1
/// tests, mirroring [`render_button_bar_to_buffer`]. Drives the production
/// [`render_tab_strip`] through a `TestBackend` so a test can assert on the
/// rendered cells (e.g. the presence of a `[×]` close glyph on Mode /
/// Orchestration tabs and its absence on the Dashboard tab) without a PTY.
/// `closeable[i]` marks whether tab `i` carries a close affordance.
pub fn render_tab_bar_to_buffer(
    labels: &[&str],
    closeable: &[bool],
    active_index: usize,
    width: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let owned: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    terminal
        .draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 1,
            };
            render_tab_strip(frame, area, &owned, closeable, active_index, &palette);
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #80 M5: render a full-screen frame and run `draw_fn` inside it,
/// returning the resulting `Buffer`. Backs the per-modal L1 render seams
/// below so each one drives the *production* modal renderer through a
/// `TestBackend` — when M5 adds clickable buttons to a modal renderer, the
/// matching seam's buffer shows them automatically. Mirrors
/// [`render_button_bar_to_buffer`] / [`render_tab_bar_to_buffer`].
fn render_overlay_to_buffer(
    width: u16,
    height: u16,
    draw_fn: impl FnOnce(&mut Frame),
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    terminal
        .draw(|frame| draw_fn(frame))
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #80 M5 L1 seam: render the quit-confirm modal (option `selected`).
pub fn render_quit_confirm_to_buffer(
    selected: usize,
    width: u16,
    height: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    render_overlay_to_buffer(width, height, |frame| {
        render_quit_confirm(frame, selected, palette);
    })
}

/// PRD #80 M5 L1 seam: render the config-generation prompt (option `selected`).
pub fn render_config_gen_prompt_to_buffer(
    selected: usize,
    width: u16,
    height: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    render_overlay_to_buffer(width, height, |frame| {
        render_config_gen_prompt(frame, selected, palette);
    })
}

/// PRD #80 M5 L1 seam: render the star-prompt modal.
pub fn render_star_prompt_to_buffer(
    width: u16,
    height: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    render_overlay_to_buffer(width, height, |frame| {
        render_star_prompt(frame, palette);
    })
}

/// PRD #80 M5 L1 seam: render the help overlay.
pub fn render_help_overlay_to_buffer(
    width: u16,
    height: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    render_overlay_to_buffer(width, height, |frame| {
        render_help_overlay(frame, None, palette);
    })
}

/// PRD #80 M7 L1 seam: render the directory picker rooted at `start` into a
/// `Buffer`. Drives the production `render_dir_picker` through a
/// `TestBackend`; after M7 the picker chrome carries clickable `[Confirm]` /
/// `[Cancel]` / filter affordances, which this seam's buffer then shows.
/// Mirrors [`render_button_bar_to_buffer`].
pub fn render_dir_picker_to_buffer(
    start: std::path::PathBuf,
    width: u16,
    height: u16,
    palette: ColorPalette,
) -> ratatui::buffer::Buffer {
    let mut picker = DirPickerState::new(start);
    render_overlay_to_buffer(width, height, |frame| {
        render_dir_picker(frame, &mut picker, palette);
    })
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

    // PRD #76 M2.15: pin the layout-math helpers so a future change to the
    // mode-tab / dashboard render layout can't silently divorce spawn-time
    // dims from resize-time dims. The helpers are the single source of
    // truth for both `AgentSpawnOptions.rows/cols` (spawn) and
    // `resize_pane_pty` (resize); if one diverges from the render, the
    // agent draws into a mismatched buffer.

    #[test]
    fn mode_agent_pane_dims_uses_left_half_minus_chrome() {
        // 80×24 terminal: half_width = 80/2 - 2 = 38; rows = 24 - 3 = 21.
        // The -2 accounts for the agent pane's borders; the -3 covers tab
        // bar + hints bar chrome around the mode-tab content area.
        let (rows, cols) = mode_agent_pane_dims(Rect::new(0, 0, 80, 24));
        assert_eq!((rows, cols), (21, 38));
    }

    #[test]
    fn mode_agent_pane_dims_saturates_on_tiny_viewport() {
        // height < 3 must not underflow into u16::MAX rows — saturating
        // arithmetic keeps the inner dims at 0 so `resize_pane_pty`
        // skips the call (rows == 0) rather than handing the daemon a
        // pathological dimension.
        let (rows, cols) = mode_agent_pane_dims(Rect::new(0, 0, 2, 1));
        assert_eq!((rows, cols), (0, 0));
    }

    #[test]
    fn mode_side_pane_dims_divides_height_by_side_count() {
        // 100×30 terminal with 3 side panes: half_width = 100/2 - 2 = 48,
        // rows per side = 30/3 - 2 = 8.
        let (rows, cols) = mode_side_pane_dims(Rect::new(0, 0, 100, 30), 3);
        assert_eq!((rows, cols), (8, 48));
    }

    #[test]
    fn mode_side_pane_dims_clamps_zero_count_to_one() {
        // side_count == 0 must clamp to 1 to dodge a division-by-zero.
        let (rows, cols) = mode_side_pane_dims(Rect::new(0, 0, 100, 30), 0);
        assert_eq!((rows, cols), (28, 48));
    }

    #[test]
    fn dashboard_pane_dims_tiled_divides_main_height() {
        // 100×30 terminal, no tab bar, 2 panes, tiled. main_height = 30-1
        // (hints bar) = 29; chunk = 29/2 = 14; rows = 14 - 2 = 12.
        // right_width = 100 * 67 / 100 = 67; cols = 67 - 2 = 65.
        let (rows, cols) =
            dashboard_pane_dims(Rect::new(0, 0, 100, 30), 2, false, PaneLayout::Tiled, false);
        assert_eq!((rows, cols), (12, 65));
    }

    #[test]
    fn dashboard_pane_dims_tiled_with_tab_bar_subtracts_chrome() {
        // Same shape but show_tab_bar=true: chrome_rows = 1, main_height
        // = 30 - 2 = 28; chunk = 14; rows = 12.
        let (rows, cols) =
            dashboard_pane_dims(Rect::new(0, 0, 100, 30), 2, false, PaneLayout::Tiled, true);
        assert_eq!((rows, cols), (12, 65));
    }

    #[test]
    fn dashboard_pane_dims_stacked_focused_takes_remainder() {
        // 100×30, 3 panes, stacked, focused: unfocused = 2; main = 29;
        // chunk = 29 - 2 = 27; rows = 25. The other two panes collapse
        // to 1-row title bars (next assertion).
        let (rows, cols) = dashboard_pane_dims(
            Rect::new(0, 0, 100, 30),
            3,
            true,
            PaneLayout::Stacked,
            false,
        );
        assert_eq!((rows, cols), (25, 65));
    }

    #[test]
    fn dashboard_pane_dims_stacked_unfocused_collapses_to_title_bar() {
        // Unfocused in stacked mode: chunk = 1; rows = saturating_sub(2) = 0.
        // `resize_pane_pty` callers gate on rows > 0, so this just signals
        // "don't bother dispatching a resize for this pane right now."
        let (rows, _cols) = dashboard_pane_dims(
            Rect::new(0, 0, 100, 30),
            3,
            false,
            PaneLayout::Stacked,
            false,
        );
        assert_eq!(rows, 0);
    }

    #[test]
    fn dashboard_pane_dims_zero_pane_count_does_not_divide_by_zero() {
        // Edge case: pane_count = 0 (the very first pane is about to be
        // added). Clamp to 1 so the math produces a sane value instead
        // of a panic / overflow.
        let (rows, cols) =
            dashboard_pane_dims(Rect::new(0, 0, 100, 30), 0, false, PaneLayout::Tiled, false);
        assert_eq!((rows, cols), (27, 65));
    }

    // PRD #76 M2.15 fixup F3 — pin the orchestration helper's geometry to
    // the orchestration renderer's `[34%, 66%]` split. Before this helper
    // existed, every spawn / resize site for orchestration role panes
    // routed through `dashboard_pane_dims` (67%), so the daemon-side PTY
    // ran one column wider than the rendered area.

    #[test]
    fn orchestration_role_pane_dims_uses_right_66_percent_width() {
        // 100×30 terminal, no tab bar, 2 roles, tiled. main_height = 30-1
        // (hints bar) = 29; chunk = 29/2 = 14; rows = 12. right_width =
        // 100 * 66 / 100 = 66; cols = 64. Critical assertion: cols = 64,
        // NOT 65 (which is what `dashboard_pane_dims` would return for
        // the same input). The 1-col gap is exactly the F3 drift bug.
        let (rows, cols) = orchestration_role_pane_dims(
            Rect::new(0, 0, 100, 30),
            2,
            0,
            None,
            PaneLayout::Tiled,
            false,
        );
        assert_eq!((rows, cols), (12, 64));
    }

    #[test]
    fn orchestration_role_pane_dims_matches_renderer_constants() {
        // Drift guard: the helper's width and the renderer's
        // `Layout::horizontal([ORCHESTRATION_LEFT_PERCENT, ORCHESTRATION_PANES_PERCENT])`
        // must produce the same right-column width by construction.
        // Without this, a future tweak to one but not the other silently
        // re-introduces the F3 spawn-vs-render drift.
        let area = Rect::new(0, 0, 200, 50);
        let (_rows, helper_cols) =
            orchestration_role_pane_dims(area, 3, 0, None, PaneLayout::Tiled, false);
        // Inner cols = right-column width - 2 (pane borders).
        let renderer_cols = (area.width * ORCHESTRATION_PANES_PERCENT / 100).saturating_sub(2);
        assert_eq!(helper_cols, renderer_cols);
        // Sanity: dashboard helper differs because of the 67 vs 66 split.
        let dashboard_renderer_cols =
            (area.width * DASHBOARD_PANES_PERCENT / 100).saturating_sub(2);
        assert_ne!(renderer_cols, dashboard_renderer_cols);
    }

    #[test]
    fn orchestration_role_pane_dims_tiled_divides_height_equally() {
        // 4 roles, Tiled: every role_index returns the same dims.
        let area = Rect::new(0, 0, 100, 30);
        let r0 = orchestration_role_pane_dims(area, 4, 0, None, PaneLayout::Tiled, true);
        let r1 = orchestration_role_pane_dims(area, 4, 1, None, PaneLayout::Tiled, true);
        let r2 = orchestration_role_pane_dims(area, 4, 2, None, PaneLayout::Tiled, true);
        let r3 = orchestration_role_pane_dims(area, 4, 3, None, PaneLayout::Tiled, true);
        assert_eq!(r0, r1);
        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
    }

    #[test]
    fn orchestration_role_pane_dims_stacked_role_zero_expands() {
        // In Stacked mode with no focused role, role_index 0 mirrors
        // the renderer's "expand the first slot if nothing is focused"
        // fallback (see `render_terminal_panes` Stacked branch). Role
        // 0 gets the lion's share; others collapse to the 1-row
        // sentinel that resize callers gate on (`rows > 0` skips the
        // resize).
        let area = Rect::new(0, 0, 100, 30);
        let (rows_focused, _) =
            orchestration_role_pane_dims(area, 3, 0, None, PaneLayout::Stacked, false);
        let (rows_unfocused, _) =
            orchestration_role_pane_dims(area, 3, 1, None, PaneLayout::Stacked, false);
        assert!(rows_focused > rows_unfocused);
        assert_eq!(rows_unfocused, 0);
    }

    // PRD #76 M2.15 fixup pass 2 G2 — pin the focus-aware behavior so a
    // future refactor of `orchestration_role_pane_dims` can't silently
    // re-introduce the "role 0 hardcoded as expanded" bug.

    #[test]
    fn orchestration_role_pane_dims_stacked_expands_focused_non_zero_role() {
        // The reviewer R1 / fixup-pass-2 G2 bug: when the focused role
        // is non-zero, the resize sweep must expand THAT role's PTY,
        // not role 0. Pre-fix, the helper hardcoded role_index==0 as
        // expanded, so a focused non-zero role got the collapsed (1
        // row → 0 inner rows after border) height while role 0 got
        // the lion's share. With `focused_role_index=Some(2)`, the
        // helper must give role 2 the expanded rows and role 0 the
        // collapsed sentinel.
        let area = Rect::new(0, 0, 100, 30);
        let (r0_rows, _) =
            orchestration_role_pane_dims(area, 3, 0, Some(2), PaneLayout::Stacked, false);
        let (r1_rows, _) =
            orchestration_role_pane_dims(area, 3, 1, Some(2), PaneLayout::Stacked, false);
        let (r2_rows, _) =
            orchestration_role_pane_dims(area, 3, 2, Some(2), PaneLayout::Stacked, false);
        assert_eq!(r0_rows, 0, "role 0 must collapse when role 2 is focused");
        assert_eq!(r1_rows, 0, "role 1 must collapse when role 2 is focused");
        assert!(
            r2_rows > 0,
            "focused role must receive the expanded rows, got {r2_rows}"
        );
        // The focused row count must match what role 0 would have
        // received under the no-focus fallback — i.e. swapping which
        // slot is expanded, not changing the geometry.
        let (rows_when_role0_expanded, _) =
            orchestration_role_pane_dims(area, 3, 0, None, PaneLayout::Stacked, false);
        assert_eq!(
            r2_rows, rows_when_role0_expanded,
            "focused role's expanded rows must equal the no-focus role-0 expansion"
        );
    }

    #[test]
    fn orchestration_role_pane_dims_tiled_ignores_focused_role_index() {
        // Tiled layout divides height equally across all roles, so
        // `focused_role_index` must be a geometric no-op there.
        let area = Rect::new(0, 0, 100, 30);
        let none_dims = orchestration_role_pane_dims(area, 3, 1, None, PaneLayout::Tiled, false);
        let focused_self =
            orchestration_role_pane_dims(area, 3, 1, Some(1), PaneLayout::Tiled, false);
        let focused_other =
            orchestration_role_pane_dims(area, 3, 1, Some(2), PaneLayout::Tiled, false);
        assert_eq!(none_dims, focused_self);
        assert_eq!(none_dims, focused_other);
    }

    #[test]
    fn orchestration_role_pane_dims_matches_renderer_when_focused_role_nonzero() {
        // Extended drift guard (fixup pass 2 G2): the helper's
        // expanded-row height for a focused role must equal the
        // renderer's expanded height for the same slot. The renderer
        // gives the focused slot `Constraint::Fill(1)` after carving
        // 1-row title bars off the other (count-1) slots — i.e. the
        // expanded inner height = main_height - (count-1) - 2 (border).
        let area = Rect::new(0, 0, 200, 50);
        let role_count: u16 = 4;
        let chrome_rows: u16 = 1; // hints bar; no tab bar in this test
        let main_height = area.height.saturating_sub(chrome_rows);
        let expanded_outer = main_height.saturating_sub(role_count - 1);
        let expanded_inner = expanded_outer.saturating_sub(2);
        let (helper_rows, _) = orchestration_role_pane_dims(
            area,
            role_count as usize,
            2,
            Some(2),
            PaneLayout::Stacked,
            false,
        );
        assert_eq!(helper_rows, expanded_inner);
    }

    #[test]
    fn orchestration_role_pane_dims_zero_role_count_does_not_divide_by_zero() {
        // Defensive: role_count = 0 (transient state during a tab
        // teardown). Clamp to 1 so the helper returns a sane value.
        let (rows, cols) = orchestration_role_pane_dims(
            Rect::new(0, 0, 100, 30),
            0,
            0,
            None,
            PaneLayout::Tiled,
            false,
        );
        // main_height = 29, count = 1, chunk = 29, rows = 27.
        // right_width = 66, cols = 64.
        assert_eq!((rows, cols), (27, 64));
    }

    // PRD #76 M2.12: pin the hydration partition's bucket semantics so
    // a future tweak to `partition_hydrated_panes` can't silently strand
    // mode/orchestration panes on the dashboard or double-build a tab.

    fn hydrated(
        pane_id: &str,
        agent_id: &str,
        cwd: Option<&str>,
        membership: Option<TabMembership>,
    ) -> HydratedPane {
        HydratedPane {
            pane_id: pane_id.to_string(),
            agent_id: agent_id.to_string(),
            display_name: None,
            cwd: cwd.map(|s| s.to_string()),
            tab_membership: membership,
            agent_type: None,
        }
    }

    #[test]
    fn partition_routes_dashboard_panes_unchanged() {
        let panes = vec![
            hydrated("1", "a-1", Some("/work"), None),
            hydrated("2", "a-2", None, None),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.dashboard_pane_ids, vec!["1".to_string(), "2".to_string()]);
        assert!(p.mode_buckets.is_empty());
        assert!(p.orchestration_buckets.is_empty());
    }

    /// Round-10 reviewer #2: two saved panes that share
    /// `(dir, name, mode)` must NOT both be dropped when only one was
    /// hydrated. The pre-round-10 `HashSet` dedupe collapsed both;
    /// the count-based budget drops at most one per match.
    #[test]
    fn dedupe_drops_only_count_matched_saved_panes() {
        use config::SavedPane;
        let mut metadata: HashMap<String, SavedPane> = HashMap::new();
        // One hydrated dashboard pane with (dir=/w, name=foo, mode=None).
        metadata.insert(
            "pane-1".into(),
            SavedPane {
                dir: "/w".into(),
                name: "foo".into(),
                command: String::new(),
                mode: None,
            },
        );
        let mut budget = build_dedupe_budget(&metadata);

        let saved_a = SavedPane {
            dir: "/w".into(),
            name: "foo".into(),
            command: "vim".into(),
            mode: None,
        };
        let saved_b = SavedPane {
            dir: "/w".into(),
            name: "foo".into(),
            command: "tail -f log".into(),
            mode: None,
        };

        // First saved pane matches the hydrated → consumed.
        assert!(try_consume_dedupe_slot(&mut budget, &saved_a));
        // Second saved pane shares the key but the budget is exhausted → restored.
        assert!(!try_consume_dedupe_slot(&mut budget, &saved_b));
    }

    #[test]
    fn dedupe_distinguishes_mode_panes_from_dashboard() {
        use config::SavedPane;
        let mut metadata: HashMap<String, SavedPane> = HashMap::new();
        metadata.insert(
            "pane-1".into(),
            SavedPane {
                dir: "/w".into(),
                name: "foo".into(),
                command: String::new(),
                mode: Some("k8s-ops".into()),
            },
        );
        let mut budget = build_dedupe_budget(&metadata);

        // Same (dir, name) but different mode — must NOT dedupe.
        let saved_dashboard = SavedPane {
            dir: "/w".into(),
            name: "foo".into(),
            command: "vim".into(),
            mode: None,
        };
        assert!(!try_consume_dedupe_slot(&mut budget, &saved_dashboard));

        // Matching mode → DEDUPED.
        let saved_mode = SavedPane {
            dir: "/w".into(),
            name: "foo".into(),
            command: String::new(),
            mode: Some("k8s-ops".into()),
        };
        assert!(try_consume_dedupe_slot(&mut budget, &saved_mode));
    }

    #[test]
    fn dedupe_restores_saved_panes_when_daemon_empty() {
        use config::SavedPane;
        // No hydrated metadata (daemon restarted, hydration returned
        // empty). The saved-session file is the only source of truth;
        // every saved pane must restore.
        let metadata: HashMap<String, SavedPane> = HashMap::new();
        let mut budget = build_dedupe_budget(&metadata);

        let saved = SavedPane {
            dir: "/w".into(),
            name: "foo".into(),
            command: "vim".into(),
            mode: None,
        };
        assert!(!try_consume_dedupe_slot(&mut budget, &saved));
    }

    #[test]
    fn partition_groups_mode_membership_by_cwd_and_name() {
        let panes = vec![
            hydrated(
                "1",
                "a-1",
                Some("/work"),
                Some(TabMembership::Mode {
                    name: "k8s-ops".into(),
                }),
            ),
            hydrated(
                "2",
                "a-2",
                Some("/work2"),
                Some(TabMembership::Mode {
                    name: "k8s-ops".into(),
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert!(p.dashboard_pane_ids.is_empty());
        assert_eq!(p.mode_buckets.len(), 2);
        assert_eq!(p.mode_buckets[0].cwd, "/work");
        assert_eq!(p.mode_buckets[0].mode_name, "k8s-ops");
        assert_eq!(p.mode_buckets[0].agent_pane_id, "1");
        assert_eq!(p.mode_buckets[1].cwd, "/work2");
        assert_eq!(p.mode_buckets[1].agent_pane_id, "2");
    }

    #[test]
    fn partition_duplicate_mode_bucket_drops_extras_to_dashboard() {
        // Two agents claim the same (cwd, mode_name) — that's a logic
        // error (mode tabs have one agent each), so the first wins and
        // the rest end up on the dashboard rather than getting a doubled
        // mode tab. M2.12 fixup reviewer #3: the helper is pure, so the
        // duplicate is surfaced via `rejections` for the caller to log.
        let panes = vec![
            hydrated(
                "1",
                "a-1",
                Some("/work"),
                Some(TabMembership::Mode {
                    name: "k8s-ops".into(),
                }),
            ),
            hydrated(
                "2",
                "a-2",
                Some("/work"),
                Some(TabMembership::Mode {
                    name: "k8s-ops".into(),
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.mode_buckets.len(), 1);
        assert_eq!(p.mode_buckets[0].agent_pane_id, "1");
        assert_eq!(p.dashboard_pane_ids, vec!["2".to_string()]);
        assert_eq!(
            p.rejections,
            vec![HydrationRejection::DuplicateMode {
                cwd: "/work".into(),
                mode_name: "k8s-ops".into(),
                agent_id: "a-2".into(),
                pane_id: "2".into(),
            }]
        );
    }

    /// Round-12 reviewer #1: a 3-role orchestration whose workers
    /// have distinct per-pane cwds (round-9 #2 allows this) must
    /// hydrate into ONE orchestration bucket, not three. The bucket
    /// key is `(orchestration_cwd, name)` — shared across roles.
    #[test]
    fn partition_buckets_orchestration_by_orchestration_cwd_not_per_pane_cwd() {
        let orch_cwd = "/proj".to_string();
        let panes = vec![
            hydrated(
                "1",
                "a-1",
                Some("/proj"),
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 0,
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                    orchestration_cwd: Some(orch_cwd.clone()),
                }),
            ),
            hydrated(
                "2",
                "a-2",
                Some("/proj/sub-a"), // worker's own cwd diverges
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 1,
                    role_name: "coder".into(),
                    is_start_role: false,
                    orchestration_cwd: Some(orch_cwd.clone()),
                }),
            ),
            hydrated(
                "3",
                "a-3",
                Some("/proj/sub-b"),
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 2,
                    role_name: "reviewer".into(),
                    is_start_role: false,
                    orchestration_cwd: Some(orch_cwd.clone()),
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(
            p.orchestration_buckets.len(),
            1,
            "all three roles share the same orchestration_cwd → one bucket"
        );
        let bucket = &p.orchestration_buckets[0];
        assert_eq!(bucket.cwd, orch_cwd);
        assert_eq!(bucket.orchestration_name, "tdd-cycle");
        assert_eq!(bucket.role_slots.len(), 3);
    }

    /// Negative-case mirror of the above: two panes with the same
    /// orchestration name but distinct orchestration_cwds must end
    /// up in DIFFERENT buckets — the round-11 #C collision-fix
    /// invariant carried through the hydration partition.
    #[test]
    fn partition_separates_orchestrations_by_orchestration_cwd_not_pane_cwd() {
        let panes = vec![
            hydrated(
                "1",
                "a-1",
                Some("/shared"), // same per-pane cwd
                Some(TabMembership::Orchestration {
                    name: "foo".into(),
                    role_index: 0,
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                    orchestration_cwd: Some("/home/u/project-a".into()),
                }),
            ),
            hydrated(
                "2",
                "a-2",
                Some("/shared"), // same per-pane cwd — would collide on old key
                Some(TabMembership::Orchestration {
                    name: "foo".into(),
                    role_index: 0,
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                    orchestration_cwd: Some("/home/u/project-b".into()),
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(
            p.orchestration_buckets.len(),
            2,
            "distinct orchestration_cwds must split into distinct buckets"
        );
    }

    /// Legacy data path: a pre-round-11 daemon emits orchestration
    /// panes with no `orchestration_cwd`. Partition must still
    /// produce reasonable buckets (falls back to per-pane cwd) so
    /// reattach against an older daemon doesn't strand panes.
    #[test]
    fn partition_falls_back_to_per_pane_cwd_when_orchestration_cwd_missing() {
        let panes = vec![hydrated(
            "1",
            "a-1",
            Some("/legacy-work"),
            Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 0,
                role_name: "orchestrator".into(),
                is_start_role: true,
                orchestration_cwd: None,
            }),
        )];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.orchestration_buckets.len(), 1);
        assert_eq!(p.orchestration_buckets[0].cwd, "/legacy-work");
    }

    #[test]
    fn partition_collects_orchestration_role_slots() {
        let panes = vec![
            hydrated(
                "10",
                "a-10",
                Some("/work"),
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 0,
                    role_name: String::new(),
                    is_start_role: false,
                    orchestration_cwd: None,
                }),
            ),
            hydrated(
                "11",
                "a-11",
                Some("/work"),
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 2,
                    role_name: String::new(),
                    is_start_role: false,
                    orchestration_cwd: None,
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.orchestration_buckets.len(), 1);
        let bucket = &p.orchestration_buckets[0];
        assert_eq!(bucket.cwd, "/work");
        assert_eq!(bucket.orchestration_name, "tdd-cycle");
        assert_eq!(bucket.role_slots.len(), 2);
        assert!(bucket.role_slots.iter().any(|s| s.role_index == 0
            && s.pane_id == "10"
            && s.role_name.is_empty()
            && !s.is_start_role));
        assert!(bucket.role_slots.iter().any(|s| s.role_index == 2
            && s.pane_id == "11"
            && s.role_name.is_empty()
            && !s.is_start_role));
    }

    // Symptom 2 (`.dot-agent-deck/agent-card-lifecycle-bugs.md`):
    // when an orchestration role's daemon agent dies (e.g., a
    // `clear = false` release agent that finishes its workflow and
    // exits cleanly), the hydration bucket loses that slot. Pre-fix
    // the role just disappeared from the rebuilt orchestration tab.
    // `fill_dead_slots_with_placeholders` is the bridge: it stamps a
    // synthetic id onto every `None` slot and seeds a placeholder
    // session so every role in the config keeps its dashboard card.
    #[test]
    fn fill_dead_slots_replaces_none_with_synthetic_id_and_seeds_placeholder() {
        use crate::state::AppState;

        let mut state = AppState::default();
        // 5 roles, only 4 hydrated agents — the LAST role (role_index 4,
        // the `release`-style slot) is the dead one. This mirrors the
        // production scenario described in the bug task: the `release`
        // role is the last in `.dot-agent-deck.toml` and the only one
        // with `clear = false`, so it's the one most likely to be
        // missing on reconnect.
        let mut slots: Vec<Option<String>> = vec![
            Some("p-orch".to_string()),
            Some("p-coder".to_string()),
            Some("p-reviewer".to_string()),
            Some("p-auditor".to_string()),
            None,
        ];
        fill_dead_slots_with_placeholders(&mut slots, "/work", "tdd-cycle", &mut state);

        assert!(
            slots.iter().all(Option::is_some),
            "every slot must be filled"
        );
        let dead_id = slots[4].as_deref().unwrap();
        assert!(
            is_dead_slot_pane_id(dead_id),
            "dead slot must carry a synthetic id; got {dead_id:?}"
        );
        // Live slots untouched.
        assert_eq!(slots[0].as_deref(), Some("p-orch"));
        assert_eq!(slots[1].as_deref(), Some("p-coder"));
        // Placeholder session exists for the dead slot, with
        // agent_type=None so it renders as "No agent" in the card grid.
        let dead_session = state
            .sessions
            .values()
            .find(|s| s.pane_id.as_deref() == Some(dead_id))
            .expect("dead-slot placeholder session must be seeded");
        assert_eq!(
            dead_session.agent_type,
            AgentType::None,
            "dead-slot placeholder must render as 'No agent'"
        );
        assert_eq!(dead_session.agent_id, None);
    }

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

    #[test]
    fn fill_dead_slots_is_idempotent_across_repeat_calls() {
        // Reconnect runs hydration again — calling
        // `fill_dead_slots_with_placeholders` on already-filled slots
        // must not change them and must not double-seed the
        // placeholder session.
        //
        // Follow-up to 0d5e651 (reviewer finding #7): the original
        // comment claimed the helper "overwrites the existing entry
        // under the same session id" on the second pass. That's
        // wrong — the helper's body SKIPS slots where `slot.is_some()`,
        // so `insert_placeholder_session` is never re-invoked. The
        // count stays at 1 because the synthetic slot is already
        // filled on the second pass.
        use crate::state::AppState;

        let mut state = AppState::default();
        let mut slots: Vec<Option<String>> = vec![Some("p-orch".to_string()), None];
        fill_dead_slots_with_placeholders(&mut slots, "/work", "tdd-cycle", &mut state);
        let first_dead = slots[1].clone().unwrap();
        let placeholder_count_first = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref().is_some_and(is_dead_slot_pane_id))
            .count();
        // Second pass — same input shape (slots are already filled,
        // so the helper short-circuits on each iteration).
        fill_dead_slots_with_placeholders(&mut slots, "/work", "tdd-cycle", &mut state);
        let placeholder_count_second = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref().is_some_and(is_dead_slot_pane_id))
            .count();
        assert_eq!(slots[1].as_deref(), Some(first_dead.as_str()));
        assert_eq!(placeholder_count_first, 1);
        assert_eq!(placeholder_count_second, 1);
    }

    // Follow-up to 0d5e651 (reviewer finding #7): a more realistic
    // idempotency scenario than the one above. A real
    // disconnect/reconnect loop rebuilds `role_pane_ids` from scratch
    // every time `hydrate_from_daemon` runs — the slot for a dead
    // role is `None` again, not pre-filled. Verify that running the
    // helper twice in a row (with the slot reset between calls)
    // produces the SAME synthetic id (because `dead_slot_pane_id` is
    // deterministic in `(cwd, name, role_index)`) and that
    // `insert_placeholder_session` reuses the same `pane-{synthetic}`
    // session key so only one placeholder ever exists for the slot.
    #[test]
    fn fill_dead_slots_produces_stable_id_across_reconnect_rebuilds() {
        use crate::state::AppState;

        let mut state = AppState::default();
        let cwd = "/work";
        let orchestration_name = "tdd-cycle";

        // First reconnect: dead role at index 1.
        let mut slots: Vec<Option<String>> = vec![Some("p-orch".to_string()), None];
        fill_dead_slots_with_placeholders(&mut slots, cwd, orchestration_name, &mut state);
        let first_dead = slots[1].clone().unwrap();

        // Second reconnect: hydration rebuilt `role_pane_ids` from
        // scratch — the dead role's slot is `None` again. Run the
        // helper a second time.
        let mut slots: Vec<Option<String>> = vec![Some("p-orch".to_string()), None];
        fill_dead_slots_with_placeholders(&mut slots, cwd, orchestration_name, &mut state);
        let second_dead = slots[1].clone().unwrap();

        // The synthetic id is deterministic, so both reconnect
        // attempts produce the same id …
        assert_eq!(first_dead, second_dead);
        // … and the placeholder session is keyed on
        // `pane-{synthetic}`, so a second `insert_placeholder_session`
        // call reuses the existing entry rather than spawning a
        // sibling. Exactly one dead-slot session must exist.
        let dead_session_count = state
            .sessions
            .values()
            .filter(|s| s.pane_id.as_deref().is_some_and(is_dead_slot_pane_id))
            .count();
        assert_eq!(
            dead_session_count, 1,
            "repeated reconnect loops must not accumulate dead-slot \
             placeholder sessions; got {dead_session_count}"
        );
    }

    // CodeRabbit PR #118 finding #3: the production hydration loop
    // splits dead-slot pane-id assignment from placeholder-session
    // seeding, so `open_orchestration_tab_with_existing_role_panes`'s
    // Err arm doesn't orphan placeholder sessions in `AppState`. Pin
    // the contract that `assign_synthetic_dead_slot_ids` is pure (no
    // state mutation) — that's the prerequisite for the deferred-seed
    // pattern at `src/ui.rs::hydrate_from_daemon`.
    #[test]
    fn assign_synthetic_dead_slot_ids_does_not_mutate_state() {
        use crate::state::AppState;

        let state = AppState::default();
        let mut slots: Vec<Option<String>> = vec![
            Some("p-live".to_string()),
            None,
            Some("p-other".to_string()),
            None,
        ];
        let assigned = assign_synthetic_dead_slot_ids(&mut slots, "/work", "tdd-cycle");
        assert_eq!(
            assigned.len(),
            2,
            "the two `None` slots must each yield a synthetic id"
        );
        assert!(assigned.iter().all(|id| is_dead_slot_pane_id(id)));
        // Slots reflect the assignment …
        assert_eq!(slots[1].as_deref(), Some(assigned[0].as_str()));
        assert_eq!(slots[3].as_deref(), Some(assigned[1].as_str()));
        // … but `AppState` is untouched. The split-phase pattern relies
        // on this so a tab-open `Err` cannot leak placeholder sessions.
        assert!(
            state.sessions.is_empty(),
            "assign_synthetic_dead_slot_ids must be pure; sessions={:?}",
            state.sessions.keys().collect::<Vec<_>>()
        );
    }

    // CodeRabbit PR #118 finding #3: when
    // `open_orchestration_tab_with_existing_role_panes` returns Err
    // (e.g., `MismatchedRoleCount` from a malformed daemon record),
    // the hydration loop must not leave orphaned placeholder sessions
    // behind for the synthetic dead-slot ids it just minted. The fix
    // defers `insert_placeholder_session` calls into the `Ok` arm
    // only; the `Err` arm is a no-op for `AppState`. This test pins
    // that contract by replaying the production sequence end-to-end.
    #[test]
    fn dead_slot_placeholders_not_seeded_when_tab_open_fails() {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
        use crate::state::AppState;

        fn mk_role(name: &str, start: bool) -> OrchestrationRoleConfig {
            OrchestrationRoleConfig {
                name: name.to_string(),
                command: String::new(),
                start,
                description: None,
                prompt_template: None,
                clear: true,
            }
        }

        let state = AppState::default();
        let cfg = OrchestrationConfig {
            name: "tdd-cycle".into(),
            roles: vec![
                mk_role("orch", true),
                mk_role("coder", false),
                mk_role("release", false),
            ],
        };

        // Bucket-driven shape: roles 0 + 1 alive, role 2 dead.
        let mut role_pane_ids: Vec<Option<String>> =
            vec![Some("p-orch".into()), Some("p-coder".into()), None];

        // Phase 1 (production hydration): mint synthetic ids for the
        // dead slot. State must remain untouched.
        let synthetic_ids =
            assign_synthetic_dead_slot_ids(&mut role_pane_ids, "/work", "tdd-cycle");
        assert_eq!(synthetic_ids.len(), 1, "exactly the role 2 slot is dead");
        assert!(state.sessions.is_empty(), "phase 1 must not seed sessions");

        // Force the tab-open Err by passing a length-mismatched vec.
        // In production this is unreachable (the vec is built from
        // `cfg.roles.len()`); the Err arm exists defensively for
        // malformed daemon records and future-caller bugs. This is
        // what we want to pin: even on Err, no orphan sessions.
        struct NoopPC;
        impl crate::pane::PaneController for NoopPC {
            fn create_pane(
                &self,
                _cmd: Option<&str>,
                _cwd: Option<&str>,
            ) -> Result<String, crate::pane::PaneError> {
                Ok(String::new())
            }
            fn write_to_pane(&self, _id: &str, _text: &str) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn close_pane(&self, _id: &str) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn rename_pane(
                &self,
                _id: &str,
                name: &str,
            ) -> Result<crate::pane::RenameOutcome, crate::pane::PaneError> {
                Ok(crate::pane::RenameOutcome::applied(name))
            }
            fn focus_pane(&self, _id: &str) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn list_panes(&self) -> Result<Vec<crate::pane::PaneInfo>, crate::pane::PaneError> {
                Ok(Vec::new())
            }
            fn resize_pane(
                &self,
                _id: &str,
                _direction: crate::pane::PaneDirection,
                _amount: u16,
            ) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn toggle_layout(&self) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn name(&self) -> &str {
                "noop"
            }
            fn is_available(&self) -> bool {
                true
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        let mut tm = crate::tab::TabManager::new(Arc::new(NoopPC));
        let mut bad_vec = role_pane_ids.clone();
        bad_vec.truncate(role_pane_ids.len() - 1);
        let result = tm.open_orchestration_tab_with_existing_role_panes(&cfg, "/work", bad_vec);
        assert!(
            result.is_err(),
            "test precondition: mismatched-length role_pane_ids must Err"
        );

        // Phase 2 (production hydration `Ok` arm) was deliberately
        // skipped because of the Err. The `Err` arm is a no-op for
        // `AppState`, so no synthetic placeholder must remain.
        let leaked: Vec<String> = state
            .sessions
            .keys()
            .filter(|k| is_dead_slot_pane_id(k))
            .cloned()
            .collect();
        assert!(
            leaked.is_empty(),
            "tab-open Err must not leave orphan placeholder sessions; got {leaked:?}"
        );
        // And the synthetic ids we minted should be gone from state
        // entirely — `assign_synthetic_dead_slot_ids` never touched
        // it, and the Err arm never seeded them.
        for synthetic in &synthetic_ids {
            assert!(
                !state.sessions.contains_key(synthetic),
                "synthetic id {synthetic} must not have an `AppState` entry"
            );
        }
    }

    // Follow-up to 0d5e651 (auditor finding #2): dead-slot placeholder
    // sessions belong to the orchestration tab only. The dashboard
    // scoping uses `all_managed_pane_ids` as an EXCLUDE filter, but
    // that set now skips synthetic ids (otherwise `close_tab` would
    // try to `close_pane` them). Without an explicit `is_dead_slot`
    // filter on the dashboard side, the placeholder "No agent" card
    // leaks onto the Dashboard tab as a ghost. Pin the exclusion.
    #[test]
    fn dashboard_filter_excludes_dead_slot_placeholder_sessions() {
        use crate::state::AppState;

        let mut state = AppState::default();

        // A real session on a real pane that DOES belong to an
        // orchestration tab (so it's in `exclude`) — this models the
        // existing behaviour that orchestration-tab sessions are
        // hidden from the dashboard.
        let real_pane = "p-orchestrator".to_string();
        state.register_pane(real_pane.clone());
        state.insert_placeholder_session(
            real_pane.clone(),
            Some("/work".to_string()),
            Some(AgentType::ClaudeCode),
            Some("agent-A".to_string()),
        );

        // A dead-slot placeholder seeded by `fill_dead_slots_with_placeholders`.
        let mut slots: Vec<Option<String>> = vec![Some(real_pane.clone()), None];
        fill_dead_slots_with_placeholders(&mut slots, "/work", "tdd-cycle", &mut state);
        let dead_pane = slots[1].clone().unwrap();

        // A separate session that lives only on the dashboard (not in
        // any orchestration tab) — this is what should survive the
        // filter.
        let dashboard_pane = "p-dashboard-only".to_string();
        state.register_pane(dashboard_pane.clone());
        state.insert_placeholder_session(
            dashboard_pane.clone(),
            Some("/work".to_string()),
            Some(AgentType::ClaudeCode),
            Some("agent-B".to_string()),
        );

        // Build the exclude set the way `TabManager::all_managed_pane_ids`
        // does for an orchestration tab: include the real pane, skip
        // the synthetic dead-slot id.
        let exclude: Vec<String> = slots
            .iter()
            .filter_map(|s| s.as_ref())
            .filter(|id| !is_dead_slot_pane_id(id))
            .cloned()
            .collect();
        assert!(
            !exclude.contains(&dead_pane),
            "test precondition: synthetic id must NOT be in the exclude set"
        );

        // Replicate the dashboard scoping filter at
        // `render_frame`'s Tab::Dashboard branch.
        let visible_on_dashboard: Vec<String> = state
            .sessions
            .values()
            .filter(|s| {
                s.pane_id
                    .as_ref()
                    .is_none_or(|pid| !exclude.contains(pid) && !is_dead_slot_pane_id(pid))
            })
            .filter_map(|s| s.pane_id.clone())
            .collect();

        assert!(
            !visible_on_dashboard.contains(&dead_pane),
            "dead-slot placeholder must NOT appear on the Dashboard; \
             got {visible_on_dashboard:?}"
        );
        assert!(
            !visible_on_dashboard.contains(&real_pane),
            "real orchestration-tab pane must continue to be excluded \
             from the Dashboard via the existing path"
        );
        assert!(
            visible_on_dashboard.contains(&dashboard_pane),
            "dashboard-only session must remain visible; got {visible_on_dashboard:?}"
        );
    }

    #[test]
    fn partition_separates_orchestrations_by_cwd() {
        // Same orchestration name, different cwds — two separate tabs.
        let panes = vec![
            hydrated(
                "1",
                "a-1",
                Some("/work-a"),
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 0,
                    role_name: String::new(),
                    is_start_role: false,
                    orchestration_cwd: None,
                }),
            ),
            hydrated(
                "2",
                "a-2",
                Some("/work-b"),
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 0,
                    role_name: String::new(),
                    is_start_role: false,
                    orchestration_cwd: None,
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.orchestration_buckets.len(), 2);
    }

    #[test]
    fn partition_mixed_input_preserves_order() {
        // dashboard pane, then mode, then orchestration in input order.
        let panes = vec![
            hydrated("1", "a-1", Some("/w"), None),
            hydrated(
                "2",
                "a-2",
                Some("/w"),
                Some(TabMembership::Mode { name: "m".into() }),
            ),
            hydrated(
                "3",
                "a-3",
                Some("/w"),
                Some(TabMembership::Orchestration {
                    name: "o".into(),
                    role_index: 0,
                    role_name: String::new(),
                    is_start_role: false,
                    orchestration_cwd: None,
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.dashboard_pane_ids, vec!["1".to_string()]);
        assert_eq!(p.mode_buckets.len(), 1);
        assert_eq!(p.mode_buckets[0].agent_pane_id, "2");
        assert_eq!(p.orchestration_buckets.len(), 1);
        assert_eq!(p.orchestration_buckets[0].role_slots.len(), 1);
        let slot = &p.orchestration_buckets[0].role_slots[0];
        assert_eq!(slot.role_index, 0);
        assert_eq!(slot.pane_id, "3");
    }

    // ------------------------------------------------------------
    // PRD #111: orchestration tab hydration with synthesised config
    // (remote-reconnect path: laptop TUI connects to VM daemon whose
    // bucket.cwd doesn't exist locally → no local project config).
    // ------------------------------------------------------------

    #[test]
    fn partition_propagates_role_name_and_is_start_role() {
        // PRD #111: the partition must echo the daemon's role_name +
        // is_start_role into the bucket so the hydration site can
        // synthesise a config when the local TOML is absent.
        let panes = vec![
            hydrated(
                "p0",
                "a-0",
                Some("/remote/proj"),
                Some(TabMembership::Orchestration {
                    name: "review".into(),
                    role_index: 0,
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                    orchestration_cwd: Some("/remote/proj".into()),
                }),
            ),
            hydrated(
                "p1",
                "a-1",
                Some("/remote/proj"),
                Some(TabMembership::Orchestration {
                    name: "review".into(),
                    role_index: 1,
                    role_name: "reviewer".into(),
                    is_start_role: false,
                    orchestration_cwd: Some("/remote/proj".into()),
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.orchestration_buckets.len(), 1);
        let bucket = &p.orchestration_buckets[0];
        let slot_0 = bucket
            .role_slots
            .iter()
            .find(|s| s.role_index == 0)
            .expect("slot 0");
        assert_eq!(slot_0.role_name, "orchestrator");
        assert!(slot_0.is_start_role);
        let slot_1 = bucket
            .role_slots
            .iter()
            .find(|s| s.role_index == 1)
            .expect("slot 1");
        assert_eq!(slot_1.role_name, "reviewer");
        assert!(!slot_1.is_start_role);
    }

    /// PRD #111 happy path: daemon reports orchestration panes whose
    /// `cwd` doesn't resolve to a local project config file (laptop TUI
    /// → VM daemon). The hydration logic synthesises an
    /// `OrchestrationConfig` from the bucket metadata; opening the
    /// tab with that config must succeed and produce a `Tab::Orchestration`
    /// with the right name, role count, and `start_role_index`.
    /// Crucially, no role pane lands on the dashboard.
    #[test]
    fn synthesised_orchestration_tab_rebuilds_without_local_config() {
        use crate::project_config::{OrchestrationConfig, SynthesisRoleSlot};

        let panes = vec![
            hydrated(
                "p0",
                "a-0",
                Some("/remote/proj"),
                Some(TabMembership::Orchestration {
                    name: "review".into(),
                    role_index: 0,
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                    orchestration_cwd: Some("/remote/proj".into()),
                }),
            ),
            hydrated(
                "p1",
                "a-1",
                Some("/remote/proj"),
                Some(TabMembership::Orchestration {
                    name: "review".into(),
                    role_index: 2,
                    role_name: "reviewer".into(),
                    is_start_role: false,
                    orchestration_cwd: Some("/remote/proj".into()),
                }),
            ),
        ];
        let partition = partition_hydrated_panes(&panes);
        assert_eq!(partition.dashboard_pane_ids.len(), 0);
        let bucket = &partition.orchestration_buckets[0];

        let synthesis_slots: Vec<SynthesisRoleSlot> = bucket
            .role_slots
            .iter()
            .map(|s| SynthesisRoleSlot {
                role_index: s.role_index,
                role_name: s.role_name.clone(),
                is_start_role: s.is_start_role,
            })
            .collect();
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata(
            &bucket.orchestration_name,
            &synthesis_slots,
        );
        assert_eq!(cfg.name, "review");
        assert_eq!(cfg.roles.len(), 3);
        assert_eq!(cfg.roles[0].name, "orchestrator");
        assert_eq!(cfg.roles[1].name, "role-1"); // dead slot placeholder
        assert_eq!(cfg.roles[2].name, "reviewer");
        assert!(cfg.roles[0].start);
        assert!(!cfg.roles[1].start);
        assert!(!cfg.roles[2].start);

        // Build the role_pane_ids vec the way the hydration site does.
        let mut role_pane_ids: Vec<Option<String>> = vec![None; cfg.roles.len()];
        for slot in &bucket.role_slots {
            role_pane_ids[slot.role_index] = Some(slot.pane_id.clone());
        }
        assert_eq!(role_pane_ids[0], Some("p0".into()));
        assert_eq!(role_pane_ids[1], None);
        assert_eq!(role_pane_ids[2], Some("p1".into()));

        // Finally drive the tab open through the real TabManager+mock
        // pane controller. This is the same call the hydration loop
        // makes; if it succeeds with a synthesised config we're done.
        struct NoopPC;
        impl crate::pane::PaneController for NoopPC {
            fn create_pane(
                &self,
                _cmd: Option<&str>,
                _cwd: Option<&str>,
            ) -> Result<String, crate::pane::PaneError> {
                Ok(String::new())
            }
            fn write_to_pane(&self, _id: &str, _text: &str) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn close_pane(&self, _id: &str) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn rename_pane(
                &self,
                _id: &str,
                name: &str,
            ) -> Result<crate::pane::RenameOutcome, crate::pane::PaneError> {
                Ok(crate::pane::RenameOutcome::applied(name))
            }
            fn focus_pane(&self, _id: &str) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn list_panes(&self) -> Result<Vec<crate::pane::PaneInfo>, crate::pane::PaneError> {
                Ok(Vec::new())
            }
            fn resize_pane(
                &self,
                _id: &str,
                _direction: crate::pane::PaneDirection,
                _amount: u16,
            ) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn toggle_layout(&self) -> Result<(), crate::pane::PaneError> {
                Ok(())
            }
            fn name(&self) -> &str {
                "noop"
            }
            fn is_available(&self) -> bool {
                true
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        let mut tm = crate::tab::TabManager::new(Arc::new(NoopPC));
        let (tab_index, _flat) = tm
            .open_orchestration_tab_with_existing_role_panes(&cfg, &bucket.cwd, role_pane_ids)
            .expect("synthesised-config hydration must succeed");
        assert_eq!(tab_index, 1, "first non-dashboard tab is at index 1");
        let labels = tm.tab_labels();
        assert_eq!(labels[0], "Dashboard");
        assert_eq!(labels[1], "review");
    }

    /// PRD #111 reviewer S2: with a duplicate `role_index` in the
    /// hydrated bucket the two consumers must agree on which slot
    /// wins. The hydration loop keeps the *first* pane_id at that
    /// index (`if role_pane_ids[role_index].is_some() { continue; }`)
    /// and the synthesis path keeps the *first* role_name / start
    /// flag (project_config first-wins guard). Without that
    /// alignment the rendered tab carries the second slot's role
    /// label but the first slot's live pane — a confusing mismatch.
    #[test]
    fn duplicate_role_index_first_wins_through_full_hydration_path() {
        use crate::project_config::SynthesisRoleSlot;

        let panes = vec![
            hydrated(
                "first-pane",
                "a-0",
                Some("/remote/proj"),
                Some(TabMembership::Orchestration {
                    name: "review".into(),
                    role_index: 0,
                    role_name: "first".into(),
                    is_start_role: true,
                    orchestration_cwd: Some("/remote/proj".into()),
                }),
            ),
            hydrated(
                "second-pane",
                "a-1",
                Some("/remote/proj"),
                Some(TabMembership::Orchestration {
                    name: "review".into(),
                    role_index: 0,
                    role_name: "second".into(),
                    is_start_role: false,
                    orchestration_cwd: Some("/remote/proj".into()),
                }),
            ),
        ];
        let partition = partition_hydrated_panes(&panes);
        assert_eq!(partition.orchestration_buckets.len(), 1);
        let bucket = &partition.orchestration_buckets[0];
        assert_eq!(
            bucket.role_slots.len(),
            2,
            "partition must preserve both duplicates so the hydration loop can decide"
        );

        // Synthesis path: first slot's role_name and start flag win.
        let synthesis_slots: Vec<SynthesisRoleSlot> = bucket
            .role_slots
            .iter()
            .map(|s| SynthesisRoleSlot {
                role_index: s.role_index,
                role_name: s.role_name.clone(),
                is_start_role: s.is_start_role,
            })
            .collect();
        let cfg = crate::project_config::OrchestrationConfig::synthesize_from_bucket_metadata(
            &bucket.orchestration_name,
            &synthesis_slots,
        );
        assert_eq!(cfg.roles.len(), 1);
        assert_eq!(
            cfg.roles[0].name, "first",
            "synthesis must keep the first slot's role_name on duplicate role_index"
        );
        assert!(
            cfg.roles[0].start,
            "synthesis must keep the first slot's is_start_role on duplicate role_index"
        );

        // Hydration de-dup loop: first slot's pane_id wins. Replicates
        // the `if role_pane_ids[role_index].is_some() { continue; }`
        // guard at the hydration call site.
        let mut role_pane_ids: Vec<Option<String>> = vec![None; cfg.roles.len()];
        for slot in &bucket.role_slots {
            if role_pane_ids[slot.role_index].is_some() {
                continue;
            }
            role_pane_ids[slot.role_index] = Some(slot.pane_id.clone());
        }
        assert_eq!(
            role_pane_ids[0].as_deref(),
            Some("first-pane"),
            "hydration loop must keep the first slot's pane_id on duplicate role_index"
        );
    }

    /// PRD #111 regression: local config path still wins when the
    /// project config file is found AND carries this orchestration
    /// name. The synthesis fallback must NOT shadow display-only
    /// enrichment (description, prompt_template) that only the local
    /// TOML carries.
    ///
    /// Reviewer S1: this test drives `resolve_orch_config_for_hydration`
    /// — the exact helper the hydration loop calls — so a future
    /// refactor that accidentally flipped the precedence (or always
    /// synthesised, dropping `local`) would fail here. The prior
    /// version only asserted properties of two independently-built
    /// configs without driving the selection seam.
    #[test]
    fn local_config_enrichment_preserved_when_available() {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
        let local = OrchestrationConfig {
            name: "review".into(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "orchestrator".into(),
                    command: "claude".into(),
                    start: true,
                    description: Some("Coordinates".into()),
                    prompt_template: Some("You coordinate.".into()),
                    clear: true,
                },
                OrchestrationRoleConfig {
                    name: "reviewer".into(),
                    command: "claude --model sonnet".into(),
                    start: false,
                    description: Some("Reviews code".into()),
                    prompt_template: Some("Run tests.".into()),
                    clear: false,
                },
            ],
        };
        // Build a bucket that *would* synthesise to a config with no
        // description / prompt_template / clear=true defaults if the
        // helper picked the synthesis branch. The test asserts the
        // local config's enrichment fields round-trip through the
        // helper instead.
        let bucket = OrchestrationHydrationBucket {
            cwd: "/remote/proj".into(),
            orchestration_name: "review".into(),
            role_slots: vec![
                OrchestrationRoleSlot {
                    role_index: 0,
                    pane_id: "p0".into(),
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                },
                OrchestrationRoleSlot {
                    role_index: 1,
                    pane_id: "p1".into(),
                    role_name: "reviewer".into(),
                    is_start_role: false,
                },
            ],
        };
        let chosen = resolve_orch_config_for_hydration(Some(local.clone()), &bucket);
        assert_eq!(
            chosen.roles[0].description.as_deref(),
            Some("Coordinates"),
            "local config's description must survive selection"
        );
        assert_eq!(
            chosen.roles[1].prompt_template.as_deref(),
            Some("Run tests."),
            "local config's prompt_template must survive selection"
        );
        assert!(
            !chosen.roles[1].clear,
            "local config's non-default `clear=false` must survive selection"
        );
        assert_eq!(chosen.roles[0].command, "claude");
        assert_eq!(chosen.roles[1].command, "claude --model sonnet");

        // And the sibling case: when local is None, the helper falls
        // back to synthesis whose enrichment fields are defaults.
        let synthesised = resolve_orch_config_for_hydration(None, &bucket);
        assert_eq!(synthesised.name, "review");
        assert!(synthesised.roles[0].description.is_none());
        assert!(synthesised.roles[1].prompt_template.is_none());
        assert!(
            synthesised.roles[1].clear,
            "synthesis defaults `clear` to true (mirrors loader default)"
        );
        // Synthesis carries the role identity, just not the enrichment.
        assert_eq!(synthesised.roles[0].name, "orchestrator");
        assert!(synthesised.roles[0].start);
        assert_eq!(synthesised.roles[1].name, "reviewer");
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
                let noop = crate::embedded_pane::EmbeddedPaneController::for_render_only_tests();
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
            agent_id: None,
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
            agent_id: None,
        };
        state.apply_event(event2);

        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::for_render_only_tests();
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
                agent_id: None,
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
            agent_id: None,
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
            agent_id: None,
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
                let noop = crate::embedded_pane::EmbeddedPaneController::for_render_only_tests();
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
        // Instructs orchestrator to wait then delegate.
        assert!(content.contains("Wait for the user to tell you what to work on"));
        assert!(content.contains("delegate immediately"));
        // Enforces one-way delegation (workers never delegate to other workers).
        assert!(content.contains("Delegation is one-way"));
        assert!(content.contains("Workers NEVER delegate to other workers"));
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
    fn prepare_orchestrator_prompt_returns_full_content() {
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

        let dir = tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let prompt = prepare_orchestrator_prompt(&config, cwd);
        assert!(prompt.is_some());
        let prompt = prompt.unwrap();
        // One-liner referencing the file.
        assert!(prompt.contains("orchestrator-context.md"));
        assert!(!prompt.contains('\n'));
        // File was written with full content.
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
                agent_id: None,
            });
        }

        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::for_render_only_tests();
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

    /// Concatenate every cell symbol on a row into a single string and join
    /// rows with newlines. Used by the render-decision tests below to grep
    /// the rendered buffer for the placeholder lines.
    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    // -----------------------------------------------------------------------
    // PRD #76 M2.13: dashboard placeholder render-decision tests.
    //
    // `render_session_card` flips a single gate on `session.agent_type ==
    // AgentType::None`: when set, the card shows the "Launch an agent to
    // get started" empty state and the "No agent" status badge; when
    // populated (e.g. by the hydration path threading
    // `AgentRecord.agent_type` through `insert_placeholder_session`), the
    // card renders as a real session.
    //
    // Wire-format and AppState-side coverage live in `tests/rehydration.rs`
    // (`hydrate_preserves_agent_type_end_to_end` + its OpenCode counterpart).
    // These two tests pin the render side specifically so a future change to
    // the gate, the placeholder copy, or the agent-type field can't silently
    // re-introduce the "reconnect shows Launch an agent" bug.
    // -----------------------------------------------------------------------

    #[test]
    fn dashboard_placeholder_with_agent_type_does_not_show_launch_an_agent() {
        // Simulate the post-hydration state: a placeholder session whose
        // `agent_type` was populated from the daemon's registry via
        // `insert_placeholder_session(.., Some(ClaudeCode))`. The dashboard
        // card must render the agent (no "Launch an agent" empty state,
        // no "No agent" status badge).
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::default();
        state.register_pane("1".to_string());
        state.insert_placeholder_session(
            "1".to_string(),
            Some("/tmp".to_string()),
            Some(AgentType::ClaudeCode),
            None,
        );

        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::for_render_only_tests();
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

        let rendered = buffer_to_string(terminal.backend().buffer());
        assert!(
            !rendered.contains("Launch an agent to get started"),
            "hydrated placeholder (agent_type=ClaudeCode) must not render the \
             empty-state line; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("No agent"),
            "hydrated placeholder (agent_type=ClaudeCode) must not show the \
             'No agent' status badge; got:\n{rendered}"
        );
    }

    #[test]
    fn dashboard_placeholder_without_agent_type_shows_launch_an_agent() {
        // Negative case: a placeholder created without an `agent_type` (the
        // legacy local-mode path, or hydration from a pre-M2.13 daemon that
        // doesn't echo the field) still renders the empty state. This pins
        // the gate in the other direction so a refactor that drops the
        // distinction can't silently change the empty-state UX either.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState::default();
        state.register_pane("1".to_string());
        state.insert_placeholder_session("1".to_string(), Some("/tmp".to_string()), None, None);

        let mut ui = default_ui();
        let filtered = filter_sessions(&state, &ui);
        terminal
            .draw(|frame| {
                let noop = crate::embedded_pane::EmbeddedPaneController::for_render_only_tests();
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

        let rendered = buffer_to_string(terminal.backend().buffer());
        assert!(
            rendered.contains("Launch an agent to get started"),
            "unhydrated placeholder (agent_type=None) must render the \
             empty-state line; got:\n{rendered}"
        );
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
        // Use a fresh tempdir for current_dir so load_project_config can't
        // pick up a stray .dot-agent-deck.toml from /tmp on the host.
        let tmp = tempfile::tempdir().unwrap();
        let mut ui = default_ui();
        ui.mode = UiMode::DirPicker;
        let mut picker = make_dir_picker(&[".."]);
        picker.current_dir = tmp.path().to_path_buf();
        ui.dir_picker = Some(picker);

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
                agent_id: None,
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
                agent_id: None,
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
            agent_id: None,
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
            agent_id: None,
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
            agent_id: None,
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
            agent_id: None,
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
            agent_id: None,
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

        // Normal -> Filter: PRD #80 M4 routes `/` through `dispatch_action`
        // (shared by the `[Filter /]` button), so the handler returns
        // `Action::EnterFilter` rather than flipping the mode inline.
        let filter_action = handle_normal_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(filter_action, Action::EnterFilter));
        ui.mode = UiMode::Filter;
        assert_eq!(ui.mode, UiMode::Filter);

        // Filter -> Normal (Esc)
        handle_filter_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.mode, UiMode::Normal);

        // Normal -> Help: PRD #80 M2 routes `?` through `dispatch_action`
        // (shared by the `[Help ?]` button), so the handler now returns
        // `Action::ToggleHelp` rather than flipping the mode inline. Apply the
        // transition the way `dispatch_action` would so the test continues from
        // Help mode.
        let help_action = handle_normal_key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(help_action, Action::ToggleHelp));
        ui.mode = UiMode::Help;
        assert_eq!(ui.mode, UiMode::Help);

        // Help -> Normal
        handle_help_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.mode, UiMode::Normal);

        // Normal -> Rename: PRD #80 M4 routes `r` through `dispatch_action`
        // (shared by the `[Rename r]` button), so the handler returns
        // `Action::EnterRename` rather than flipping the mode inline.
        let rename_action = handle_normal_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(rename_action, Action::EnterRename));
        ui.mode = UiMode::Rename;
        assert_eq!(ui.mode, UiMode::Rename);

        // Rename -> Normal (Esc)
        handle_rename_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut ui,
            Some("s1"),
        );
        assert_eq!(ui.mode, UiMode::Normal);
    }

    // M2.11 fixup 5 — handle_rename_key no longer writes to
    // ui.display_names on Enter; the dispatch loop now calls
    // `pane.rename_pane` first and mirrors the controller-returned
    // RenameOutcome into both display-name maps. These tests pin
    // the residual handler responsibilities: clearing rename_text
    // and returning to Normal mode. The full commit pathway (handler
    // → controller → UI maps) is covered by
    // `tests/agent_metadata.rs::rename_outcome_*` and
    // `rename_pane_*_on_local_backend` in this crate.
    #[test]
    fn test_rename_handler_clears_buffer_and_exits_on_enter() {
        let mut ui = default_ui();
        ui.mode = UiMode::Rename;
        ui.rename_text = "my-agent".to_string();

        handle_rename_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            Some("session-123"),
        );

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(
            ui.rename_text.is_empty(),
            "rename_text must be cleared after Enter"
        );
        // Crucially: handler does NOT touch display_names. The
        // dispatch loop mirrors the controller's RenameOutcome
        // into the maps so the raw input never reaches them.
        assert!(
            !ui.display_names.contains_key("session-123"),
            "handler must not insert raw rename_text into display_names"
        );
    }

    #[test]
    fn test_rename_handler_preserves_existing_label_on_enter() {
        // Even when the handler runs with non-empty rename_text, it
        // must not mutate display_names — the dispatch loop owns
        // both maps now, gated by the RenameOutcome.
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
        assert_eq!(
            ui.display_names.get("s1"),
            Some(&"old-name".to_string()),
            "handler must NOT remove existing display_names entry — \
             the dispatch loop owns that based on RenameOutcome::Cleared"
        );
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

        handle_normal_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut ui,
            5,
            None,
        );
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
            None,
        );
        assert_eq!(ui.mode, UiMode::Normal);
    }

    // -----------------------------------------------------------------------
    // PRD #92 F2 — y / n permission key tests
    //
    // Behavior: pressing `y` on a dashboard card whose selected session is in
    // `WaitingForInput` returns `Action::SendPermissionResponse(true)` so
    // the dispatcher forwards `y` to the agent's PTY. Pressing `n` returns
    // `SendPermissionResponse(false)`. Any other status, or `total == 0`,
    // falls through to `Action::Continue`. PRD #18 documented these keys
    // on the help overlay since baseline but no handler existed until now.
    // -----------------------------------------------------------------------

    #[test]
    fn permission_y_on_waiting_for_input_returns_approve() {
        let mut ui = default_ui();
        let result = handle_normal_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut ui,
            1,
            Some(SessionStatus::WaitingForInput),
        );
        assert!(
            matches!(result, Action::SendPermissionResponse(true)),
            "y on a WaitingForInput card must approve, got {result:?}"
        );
    }

    #[test]
    fn permission_n_on_waiting_for_input_returns_deny() {
        let mut ui = default_ui();
        let result = handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut ui,
            1,
            Some(SessionStatus::WaitingForInput),
        );
        assert!(
            matches!(result, Action::SendPermissionResponse(false)),
            "n on a WaitingForInput card must deny, got {result:?}"
        );
    }

    #[test]
    fn permission_y_n_on_non_waiting_status_is_no_op() {
        // Any non-WaitingForInput status: keys must NOT trigger the permission
        // response. They fall through to `Continue` so future keybindings or
        // ignored input remain unaffected. Cover a representative sample of
        // statuses (Working, Idle, Thinking, Error) to pin the gate.
        for status in [
            SessionStatus::Working,
            SessionStatus::Idle,
            SessionStatus::Thinking,
            SessionStatus::Error,
            SessionStatus::Compacting,
        ] {
            let mut ui = default_ui();
            let result_y = handle_normal_key(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &mut ui,
                1,
                Some(status.clone()),
            );
            assert!(
                matches!(result_y, Action::Continue),
                "y on status {status:?} must no-op, got {result_y:?}"
            );
            let result_n = handle_normal_key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                &mut ui,
                1,
                Some(status.clone()),
            );
            assert!(
                matches!(result_n, Action::Continue),
                "n on status {status:?} must no-op, got {result_n:?}"
            );
        }
    }

    #[test]
    fn permission_y_n_with_no_card_selected_is_no_op() {
        // No card selected (total == 0, status None): both keys must
        // no-op. Belt-and-braces — the handler gates on `total > 0` AND on
        // status, so either guard alone is sufficient, but tests pin both.
        let mut ui = default_ui();
        let result_y = handle_normal_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut ui,
            0,
            None,
        );
        assert!(matches!(result_y, Action::Continue));
        let result_n = handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut ui,
            0,
            None,
        );
        assert!(matches!(result_n, Action::Continue));
        // Also exercise the case where `total > 0` but `selected_status` is
        // None (the selected session is missing from the snapshot — a race we
        // tolerate gracefully).
        let result_y_no_status = handle_normal_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut ui,
            1,
            None,
        );
        assert!(matches!(result_y_no_status, Action::Continue));
    }

    // -----------------------------------------------------------------------
    // PRD #68 — dashboard card navigation (j/k/Up/Down)
    //
    // Two layers are pinned here:
    //
    // 1. `handle_normal_key` updates `ui.selected_index` with wrap-around
    //    for j/k/Up/Down. This is the documented contract of the key
    //    handler (help overlay + docs/keyboard-shortcuts.md).
    //
    // 2. `focus_deck` (the Ctrl+d → 1-9 path) still flips the embedded
    //    controller's focus AND `ui.selected_index` — this is the
    //    risk-mitigation regression test the PRD asks for. The fix in
    //    the dispatch loop mirrors `focus_deck`'s "selection + focus"
    //    pattern after `handle_normal_key`, so the per-frame
    //    "selected_index ← focused pane" sync no longer rolls back the
    //    user's j/k move.
    // -----------------------------------------------------------------------

    #[test]
    fn handle_normal_key_j_and_down_advance_with_wrap() {
        let mut ui = default_ui();
        ui.selected_index = 0;

        // j advances 0 → 1
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, 1);

        // Down advances 1 → 2
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, 2);

        // wraps 2 → 0
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, 0);
    }

    #[test]
    fn handle_normal_key_k_and_up_retreat_with_wrap() {
        let mut ui = default_ui();
        ui.selected_index = 0;

        // k from 0 wraps to total - 1
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, 2);

        // Up retreats 2 → 1
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut ui,
            3,
            None,
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, 1);
    }

    #[test]
    fn handle_normal_key_jk_no_op_when_no_cards() {
        // `total == 0` (empty dashboard): j/k/Up/Down must not panic
        // and must not move `selected_index` off zero.
        let mut ui = default_ui();
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Down,
            KeyCode::Up,
        ] {
            let r = handle_normal_key(KeyEvent::new(code, KeyModifiers::NONE), &mut ui, 0, None);
            assert!(matches!(r, Action::Continue));
            assert_eq!(ui.selected_index, 0);
        }
    }

    /// Pane controller that records every `focus_pane` call. Used by the
    /// `focus_deck` regression test below — the production embedded
    /// controller is too heavy to spin up in a unit test, but the
    /// contract `focus_deck` cares about is "the controller's
    /// `focus_pane` got called with the right id".
    struct RecordingFocusPC {
        focused: std::sync::Mutex<Vec<String>>,
    }
    impl crate::pane::PaneController for RecordingFocusPC {
        fn create_pane(
            &self,
            _cmd: Option<&str>,
            _cwd: Option<&str>,
        ) -> Result<String, crate::pane::PaneError> {
            Ok(String::new())
        }
        fn write_to_pane(&self, _id: &str, _text: &str) -> Result<(), crate::pane::PaneError> {
            Ok(())
        }
        fn close_pane(&self, _id: &str) -> Result<(), crate::pane::PaneError> {
            Ok(())
        }
        fn rename_pane(
            &self,
            _id: &str,
            name: &str,
        ) -> Result<crate::pane::RenameOutcome, crate::pane::PaneError> {
            Ok(crate::pane::RenameOutcome::applied(name))
        }
        fn focus_pane(&self, id: &str) -> Result<(), crate::pane::PaneError> {
            self.focused.lock().unwrap().push(id.to_string());
            Ok(())
        }
        fn list_panes(&self) -> Result<Vec<crate::pane::PaneInfo>, crate::pane::PaneError> {
            Ok(Vec::new())
        }
        fn resize_pane(
            &self,
            _id: &str,
            _direction: crate::pane::PaneDirection,
            _amount: u16,
        ) -> Result<(), crate::pane::PaneError> {
            Ok(())
        }
        fn toggle_layout(&self) -> Result<(), crate::pane::PaneError> {
            Ok(())
        }
        fn name(&self) -> &str {
            "recording"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// PRD #68 risk-mitigation: pin that the digit-jump path
    /// (`Ctrl+d` → `1`-`9` → `focus_deck`) still focuses the right
    /// pane AND sets `ui.selected_index` after the j/k fix lands in
    /// the dispatch loop. The fix mirrors `focus_deck`'s pattern, so
    /// breaking the digit path would mean both paths regressed.
    #[test]
    fn focus_deck_focuses_target_pane_and_updates_selected_index() {
        use tokio::sync::RwLock;
        let mut snapshot = AppState::default();
        // Three sessions with concrete pane_ids so `focus_deck` has
        // something to focus on.
        for (sid, pid) in [("s0", "p0"), ("s1", "p1"), ("s2", "p2")] {
            let mut sess = make_session(SessionStatus::Idle);
            sess.session_id = sid.to_string();
            sess.pane_id = Some(pid.to_string());
            snapshot.sessions.insert(sid.to_string(), sess);
        }
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let pc = RecordingFocusPC {
            focused: std::sync::Mutex::new(Vec::new()),
        };

        // Build the `filtered` view focus_deck expects.
        let ids: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        // Sort for stable order across HashMap iteration.
        let mut ids = ids;
        ids.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        let ok = focus_deck(1, &mut ui, &ids, &snapshot, &state, &pc);
        assert!(ok, "focus_deck must accept an in-range idx");
        assert_eq!(ui.selected_index, 1);
        // PaneInput transition is also part of the Ctrl+d → 1-9 contract.
        assert_eq!(ui.mode, UiMode::PaneInput);
        let calls = pc.focused.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &["p1".to_string()],
            "focus_deck must call focus_pane on the targeted card's pane"
        );
    }

    /// Companion to the focus_deck test: pin the dispatch-loop's
    /// Normal-mode step end-to-end by driving the SAME helper that
    /// `run_tui` calls — `dispatch_normal_mode_key`. Deleting the
    /// `mirror_selection_into_focus` line from that helper would
    /// regress PRD #68 in production AND fail this test, closing the
    /// Greptile-flagged gap where calling `mirror_selection_into_focus`
    /// directly from the test would silently pass when someone
    /// removed the production call site.
    #[test]
    fn jk_navigation_mirrors_selection_into_focus() {
        let mut snapshot = AppState::default();
        for (sid, pid) in [("s0", "p0"), ("s1", "p1"), ("s2", "p2")] {
            let mut sess = make_session(SessionStatus::Idle);
            sess.session_id = sid.to_string();
            sess.pane_id = Some(pid.to_string());
            snapshot.sessions.insert(sid.to_string(), sess);
        }
        let pc = RecordingFocusPC {
            focused: std::sync::Mutex::new(Vec::new()),
        };
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));
        let total = filtered.len();

        let mut ui = default_ui();
        ui.selected_index = 0;

        let press = |ui: &mut UiState, key: KeyEvent| {
            dispatch_normal_mode_key(key, ui, total, None, &filtered, &pc);
        };

        // j: 0 → 1, focus mirrored to p1.
        press(
            &mut ui,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        assert_eq!(ui.selected_index, 1);
        // Down: 1 → 2, focus mirrored to p2.
        press(&mut ui, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(ui.selected_index, 2);
        // k: 2 → 1, focus mirrored to p1.
        press(
            &mut ui,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        );
        assert_eq!(ui.selected_index, 1);

        let calls = pc.focused.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &["p1".to_string(), "p2".to_string(), "p1".to_string()],
            "every j/k/Up/Down move must mirror the new selection into focus"
        );
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
            agent_id: None,
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
                agent_id: None,
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
            agent_id: None,
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
            agent_id: None,
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
            agent_id: None,
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
            Action::ForwardToPane(bytes) => assert_eq!(bytes, vec![b'l']),
            other => panic!("Expected ForwardToPane, got {:?}", other),
        }
    }

    // PRD #76 M2.20 — submit-debounce policy tests.

    #[test]
    fn enter_following_recent_keystroke_sleeps_at_least_debounce_minus_elapsed() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_millis(30);
        let sleep = submit_debounce_duration(now, Some(last), b"\r");
        // Expected ~120ms (150 - 30). Allow a few ms tolerance for arithmetic.
        let expected = std::time::Duration::from_millis(120);
        let lower = expected.saturating_sub(std::time::Duration::from_millis(5));
        let upper = expected + std::time::Duration::from_millis(5);
        assert!(
            sleep >= lower && sleep <= upper,
            "expected ~{expected:?}, got {sleep:?}"
        );
    }

    #[test]
    fn enter_with_stale_last_keystroke_does_not_sleep() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_millis(500);
        let sleep = submit_debounce_duration(now, Some(last), b"\r");
        assert_eq!(sleep, std::time::Duration::ZERO);
    }

    #[test]
    fn enter_with_no_prior_keystroke_does_not_sleep() {
        let now = std::time::Instant::now();
        let sleep = submit_debounce_duration(now, None, b"\r");
        assert_eq!(sleep, std::time::Duration::ZERO);
    }

    #[test]
    fn non_enter_bytes_never_sleep_even_when_recent() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_millis(10);
        let sleep = submit_debounce_duration(now, Some(last), b"hello");
        assert_eq!(sleep, std::time::Duration::ZERO);
    }

    // PRD #128 Direction B-1 — spawn-time readiness buffer policy tests.

    #[test]
    fn spawn_time_no_ready_since_fires_immediately() {
        // None signals "no SessionStart yet observed", but the caller
        // gates with the timeout-ready path; this helper should not
        // double-gate it.
        let now = std::time::Instant::now();
        assert!(should_inject_spawn_time_prompt(None, now));
    }

    #[test]
    fn spawn_time_within_buffer_window_holds() {
        let now = std::time::Instant::now();
        // 100 ms after SessionStart — far inside the 500 ms buffer.
        let ready_since = now - std::time::Duration::from_millis(100);
        assert!(!should_inject_spawn_time_prompt(Some(ready_since), now));
    }

    #[test]
    fn spawn_time_after_buffer_window_fires() {
        let now = std::time::Instant::now();
        // 600 ms after SessionStart — past the 500 ms buffer.
        let ready_since = now - std::time::Duration::from_millis(600);
        assert!(should_inject_spawn_time_prompt(Some(ready_since), now));
    }

    #[test]
    fn spawn_time_exactly_at_buffer_boundary_fires() {
        let now = std::time::Instant::now();
        let ready_since = now - SPAWN_TIME_READINESS_BUFFER;
        // `>=` boundary: exactly at the buffer should fire.
        assert!(should_inject_spawn_time_prompt(Some(ready_since), now));
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
        assert!(matches!(result, Action::SpawnPane(_)));
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
            Action::SpawnPane(req) => {
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
            Action::SpawnPane(req) => {
                assert!(req.mode_config.is_none());
            }
            other => panic!("Expected NewPane, got {:?}", other),
        }
    }

    // PRD #107 regression: user-entered name in the new-pane form should
    // override the orchestration config name so the tab title matches.
    #[test]
    fn orchestration_form_user_name_overrides_config_name() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp/proj"),
            String::new(),
            String::new(),
            vec![],
            vec![make_orchestration("config-name")],
        ));

        // Select the orchestration (Right from "No mode" → first orchestration slot)
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_new_pane_form_key(right, &mut ui);

        // Move focus to the Name field
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui); // Mode → Name

        // Type a custom name
        for c in "user-typed-name".chars() {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            handle_new_pane_form_key(key, &mut ui);
        }

        // PRD #106: with an orchestration selected, Command is hidden — so
        // pressing Enter on Name submits directly. No second navigation step.
        let result = handle_new_pane_form_key(enter, &mut ui); // submit

        let req = match result {
            Action::SpawnPane(r) => r,
            other => panic!("Expected NewPane, got {:?}", other),
        };

        // The form must carry the user's input in req.name.
        assert_eq!(req.name, "user-typed-name");

        // Simulate the handler fix (ui.rs Action::NewPane branch):
        // when req.name is non-empty, orch_config.name is overridden before
        // open_orchestration_tab is called, so the tab label matches user input.
        let mut orch = req.orchestration_config.unwrap();
        if !req.name.is_empty() {
            orch.name = req.name.clone();
        }
        assert_eq!(
            orch.name, "user-typed-name",
            "tab should show the user-entered name, not the TOML config name"
        );
    }

    // PRD #107: empty name in the form must leave the config name untouched.
    #[test]
    fn orchestration_form_empty_name_keeps_config_name() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp/proj"),
            String::new(),
            String::new(),
            vec![],
            vec![make_orchestration("config-name")],
        ));

        // Select orchestration, skip Name field (leave it empty), submit.
        // PRD #106: Command is hidden, so Enter on Name submits.
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_new_pane_form_key(right, &mut ui);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui); // Mode → Name
        let result = handle_new_pane_form_key(enter, &mut ui); // submit

        let req = match result {
            Action::SpawnPane(r) => r,
            other => panic!("Expected NewPane, got {:?}", other),
        };

        assert!(req.name.is_empty());

        // Simulate handler: empty name → no override → config name preserved.
        let mut orch = req.orchestration_config.unwrap();
        if !req.name.is_empty() {
            orch.name = req.name.clone();
        }
        assert_eq!(
            orch.name, "config-name",
            "config name must be kept when the user left the Name field empty"
        );
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

    // -----------------------------------------------------------------------
    // PRD #106: Command field visibility when orchestration is selected
    // -----------------------------------------------------------------------

    #[test]
    fn command_hidden_only_when_orchestration_selected() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("dev")],
            vec![make_orchestration("tdd")],
        );

        // 0 = No mode → command visible
        assert!(f.command_visible());

        // 1 = workspace mode → command visible
        f.selection_index = 1;
        assert!(f.command_visible());

        // 2 = orchestration → command hidden
        f.selection_index = 2;
        assert!(!f.command_visible());

        // Toggling back restores it.
        f.selection_index = 0;
        assert!(f.command_visible());
    }

    #[test]
    fn tab_skips_hidden_command_field_with_orchestration() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![],
            vec![make_orchestration("tdd")],
        );
        // Select the orchestration (index 1).
        f.selection_index = 1;
        assert!(!f.command_visible());

        assert_eq!(f.focused, FormField::Mode);

        // Mode → Name (skipping nothing yet).
        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Name);

        // Name should wrap back to Mode rather than visiting hidden Command.
        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Mode);

        // Shift+Tab from Mode should land on Name (skip Command).
        f.focused = f.prev_field();
        assert_eq!(f.focused, FormField::Name);

        // Shift+Tab from Name → Mode.
        f.focused = f.prev_field();
        assert_eq!(f.focused, FormField::Mode);
    }

    #[test]
    fn tab_visits_command_when_workspace_mode_selected() {
        // Regression guard: with a workspace mode (not an orchestration),
        // Tab cycling must still pass through the Command field.
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("dev")],
            vec![make_orchestration("tdd")],
        );
        f.selection_index = 1; // workspace mode
        assert!(f.command_visible());

        f.focused = FormField::Mode;
        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Name);
        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Command);
        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Mode);
    }

    #[test]
    fn enter_on_name_submits_when_orchestration_selected() {
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp/proj"),
            String::new(),
            String::new(),
            vec![],
            vec![make_orchestration("tdd")],
        ));

        // Select the orchestration.
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_new_pane_form_key(right, &mut ui);

        // Enter from Mode → Name.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().focused, FormField::Name);

        // Enter on Name should submit directly, not advance to a hidden field.
        let result = handle_new_pane_form_key(enter, &mut ui);
        assert!(matches!(result, Action::SpawnPane(_)));
        assert!(ui.new_pane_form.is_none());
        assert_eq!(ui.mode, UiMode::Normal);
        if let Action::SpawnPane(req) = result {
            assert!(req.orchestration_config.is_some());
            assert!(req.mode_config.is_none());
        }
    }

    #[test]
    fn tab_through_hidden_command_after_cycling_to_orchestration() {
        // End-to-end: user starts on Mode (No mode selected), navigates to
        // Command, comes back, then cycles forward to an orchestration. From
        // there, Tab must skip the now-hidden Command field cleanly.
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("dev")],
            vec![make_orchestration("tdd")],
        ));

        // Walk Mode → Name → Command → Name → Mode (all visible at this
        // point because "No mode" is selected).
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        handle_new_pane_form_key(tab, &mut ui);
        handle_new_pane_form_key(tab, &mut ui);
        assert_eq!(
            ui.new_pane_form.as_ref().unwrap().focused,
            FormField::Command
        );
        handle_new_pane_form_key(backtab, &mut ui);
        handle_new_pane_form_key(backtab, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().focused, FormField::Mode);

        // Right-arrow twice to land on the orchestration (0 → 1 → 2).
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_new_pane_form_key(right, &mut ui);
        handle_new_pane_form_key(right, &mut ui);
        assert!(
            ui.new_pane_form
                .as_ref()
                .unwrap()
                .selected_orchestration()
                .is_some()
        );

        // Now Tab forward: Mode → Name → Mode (skipping hidden Command).
        handle_new_pane_form_key(tab, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().focused, FormField::Name);
        handle_new_pane_form_key(tab, &mut ui);
        assert_eq!(ui.new_pane_form.as_ref().unwrap().focused, FormField::Mode);
    }

    #[test]
    fn footer_hint_switches_to_submit_when_name_focused_with_orchestration() {
        // PRD #106 follow-up: when the Command field is hidden and focus is
        // on Name, Enter submits — the footer must say so.
        let submit_hint = new_pane_form_footer_hint(true, true);
        assert!(
            submit_hint.contains("Enter: submit"),
            "expected submit hint, got {submit_hint:?}"
        );

        // Sanity checks: every other focus/visibility combination keeps the
        // legacy 'Enter: next' wording.
        let next_hint = new_pane_form_footer_hint(true, false);
        assert!(
            next_hint.contains("Enter: next") && !next_hint.contains("submit"),
            "expected next hint, got {next_hint:?}"
        );
        let no_mode_hint = new_pane_form_footer_hint(false, false);
        assert!(
            no_mode_hint.contains("Enter: next/confirm"),
            "expected next/confirm hint when there's no mode field, got {no_mode_hint:?}"
        );
    }

    #[test]
    fn enter_on_name_still_advances_to_command_without_orchestration() {
        // Regression guard for the non-orchestration flow: with a workspace
        // mode (or "No mode") selected, Enter on Name must still advance to
        // the Command field rather than submit.
        let mut ui = default_ui();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            "agent".to_string(),
            "claude".to_string(),
            vec![make_mode("dev")],
            vec![],
        ));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui); // Mode → Name
        let result = handle_new_pane_form_key(enter, &mut ui); // Name → Command
        assert!(matches!(result, Action::Continue));
        assert_eq!(
            ui.new_pane_form.as_ref().unwrap().focused,
            FormField::Command
        );
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
            matches!(result, Action::SendConfigGenPrompt { ref pane_id, ref cwd }
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
            agent_id: None,
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
        let frame_for = |tick: u64| (tick / 120) as usize % num_frames;
        assert_eq!(frame_for(0), 0);
        assert_eq!(frame_for(119), 0);
        assert_eq!(frame_for(120), 1);
        assert_eq!(frame_for(239), 1);
        assert_eq!(frame_for(240), 2);
        assert_eq!(frame_for(360), 0); // wraps
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
        assert!(matches!(result, Action::Continue));
    }

    #[test]
    fn config_gen_prompt_enter_on_never_suppresses_dir() {
        // Picking "Never" calls suppress_dir() → save(), which reads
        // DOT_AGENT_DECK_CONFIG_GEN_STATE. Hold the shared lock and point at
        // a temp path so we don't race against other tests touching the same
        // env var, and don't pollute the real home dir.
        let _guard = crate::config::CONFIG_GEN_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config-gen-state.json");
        // Drop guard restores the env var even if an assertion below panics.
        let _env_restore = crate::config::ConfigGenStateEnvGuard::set(path.to_str().unwrap());

        let mut ui = default_ui();
        ui.mode = UiMode::ConfigGenPrompt;
        ui.config_gen_selected = 2; // Never
        ui.config_gen_target = Some(("pane-1".to_string(), "/my/project".to_string()));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = handle_config_gen_prompt_key(enter, &mut ui);

        assert_eq!(ui.mode, UiMode::Normal);
        assert!(ui.config_gen_target.is_none());
        assert!(ui.config_gen_state.is_suppressed("/my/project"));
        assert!(matches!(result, Action::Continue));
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
        assert!(matches!(result, Action::Continue));
    }

    #[test]
    fn pending_dispatch_timeout() {
        let pane_ctrl =
            Arc::new(crate::embedded_pane::EmbeddedPaneController::for_render_only_tests());
        let pane: Arc<dyn PaneController> = pane_ctrl;
        let snapshot = AppState::default();
        let mut ui = default_ui();

        // Add a pending dispatch with an expired timeout.
        ui.pending_dispatches.push(PendingDispatch {
            pane_id: "999".to_string(),
            prompt: "Do work".to_string(),
            created_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
        });

        process_pending_dispatches(&mut ui, &pane, &snapshot);

        // Should be removed due to timeout.
        assert!(ui.pending_dispatches.is_empty());
    }

    #[test]
    fn pending_dispatch_waits_for_agent_ready() {
        let pane_ctrl =
            Arc::new(crate::embedded_pane::EmbeddedPaneController::for_render_only_tests());
        let pane: Arc<dyn PaneController> = pane_ctrl;
        let snapshot = AppState::default(); // No sessions → agent not ready
        let mut ui = default_ui();

        ui.pending_dispatches.push(PendingDispatch {
            pane_id: "1".to_string(),
            prompt: "Do work".to_string(),
            created_at: std::time::Instant::now(),
        });

        process_pending_dispatches(&mut ui, &pane, &snapshot);

        // Should still be pending — agent not ready and not timed out.
        assert_eq!(ui.pending_dispatches.len(), 1);
    }

    // PRD #93 Phase 2 / M4.2: the quit dialog collapsed from a
    // mode-dependent (Quit-or-Detach) action to a single Detach
    // confirmation. Pin that Enter on index 0 always returns
    // `DetachAndQuit` so a future refactor can't silently reintroduce
    // a local-mode "kill agents on exit" path.
    #[test]
    fn quit_confirm_enter_returns_detach() {
        let mut ui = default_ui();
        ui.mode = UiMode::QuitConfirm;
        ui.quit_confirm_selected = 0;

        let result = handle_quit_confirm_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            0,
        );
        assert!(matches!(result, Action::DetachAndQuit));
    }

    #[test]
    fn quit_confirm_cancel_returns_to_normal_mode() {
        let mut ui = default_ui();
        ui.mode = UiMode::QuitConfirm;
        // PRD #92 F1: Cancel moved from index 1 to index 2 to make room
        // for Stop in the middle.
        ui.quit_confirm_selected = 2;

        let result = handle_quit_confirm_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            0,
        );
        assert!(matches!(result, Action::Continue));
        assert_eq!(ui.mode, UiMode::Normal);
    }

    #[test]
    fn quit_confirm_down_clamps_to_three_options() {
        let mut ui = default_ui();
        ui.mode = UiMode::QuitConfirm;
        ui.quit_confirm_selected = 0;

        // Three presses on Down — selection must stop at index 2 (Cancel),
        // proving QUIT_OPTION_COUNT == 3 after the PRD #92 F1 expansion.
        handle_quit_confirm_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut ui, 0);
        assert_eq!(ui.quit_confirm_selected, 1);
        handle_quit_confirm_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut ui, 0);
        assert_eq!(ui.quit_confirm_selected, 2);
        handle_quit_confirm_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut ui, 0);
        assert_eq!(ui.quit_confirm_selected, 2);
    }

    // -----------------------------------------------------------------------
    // PRD #92 F1 — Stop option in the Ctrl+C dialog
    //
    // Primary dialog gains a Stop option at index 1 (Detach default / Stop
    // / Cancel). Picking Stop with no agents proceeds directly; with
    // agents alive the secondary y/n dialog confirms first. The secondary
    // dialog defaults to No; No returns to the primary dialog; Yes is
    // Action::StopAndQuit. KIND_SHUTDOWN side effects and registry
    // teardown are exercised by tests/stop_dialog.rs.
    // -----------------------------------------------------------------------

    #[test]
    fn quit_confirm_stop_with_no_agents_returns_stop_and_quit() {
        let mut ui = default_ui();
        ui.mode = UiMode::QuitConfirm;
        ui.quit_confirm_selected = 1; // Stop

        let result = handle_quit_confirm_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            0, // no agents
        );
        assert!(
            matches!(result, Action::StopAndQuit),
            "Stop with 0 agents must skip the secondary dialog, got {result:?}"
        );
    }

    #[test]
    fn quit_confirm_stop_with_agents_prompts_secondary_dialog() {
        let mut ui = default_ui();
        ui.mode = UiMode::QuitConfirm;
        ui.quit_confirm_selected = 1; // Stop

        let result = handle_quit_confirm_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            3, // three agents alive
        );
        assert!(
            matches!(result, Action::StopConfirmPrompt { agent_count: 3 }),
            "Stop with N>0 agents must request secondary confirmation with N rendered, got {result:?}"
        );
    }

    #[test]
    fn stop_confirm_defaults_to_no() {
        // Default selection on entry must be No (index 0). The dispatcher
        // sets this when transitioning into StopConfirm; the test
        // re-asserts the contract from the handler's perspective.
        let ui = default_ui();
        assert_eq!(
            ui.stop_confirm_selected, 0,
            "secondary dialog must default to No (index 0)"
        );
    }

    #[test]
    fn stop_confirm_yes_returns_stop_and_quit() {
        let mut ui = default_ui();
        ui.mode = UiMode::StopConfirm;
        ui.stop_confirm_selected = 1; // Yes
        ui.stop_confirm_agent_count = 2;

        let result =
            handle_stop_confirm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut ui);
        assert!(
            matches!(result, Action::StopAndQuit),
            "Enter on Yes must confirm Stop, got {result:?}"
        );
    }

    #[test]
    fn stop_confirm_y_shortcut_returns_stop_and_quit() {
        // `y` is a shortcut for Yes regardless of which option is
        // currently highlighted — matches conventional y/n dialogs.
        let mut ui = default_ui();
        ui.mode = UiMode::StopConfirm;
        ui.stop_confirm_selected = 0; // No is highlighted
        ui.stop_confirm_agent_count = 2;

        let result = handle_stop_confirm_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut ui,
        );
        assert!(
            matches!(result, Action::StopAndQuit),
            "y must confirm Stop even when No is highlighted, got {result:?}"
        );
    }

    #[test]
    fn stop_confirm_no_returns_to_primary_dialog() {
        // Selecting No (or pressing Esc / n) must return to the primary
        // QuitConfirm dialog with Stop still selected, so the user can
        // pick Detach or Cancel without re-opening the Ctrl+C dialog.
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        ] {
            let mut ui = default_ui();
            ui.mode = UiMode::StopConfirm;
            ui.stop_confirm_selected = 0; // No
            ui.stop_confirm_agent_count = 1;

            let result = handle_stop_confirm_key(key, &mut ui);
            assert!(
                matches!(result, Action::Continue),
                "{key:?} on No path must Continue, got {result:?}"
            );
            assert_eq!(
                ui.mode,
                UiMode::QuitConfirm,
                "{key:?} on No path must return to QuitConfirm"
            );
            assert_eq!(
                ui.stop_confirm_selected, 0,
                "{key:?} on No path must reset stop_confirm_selected to default (No)"
            );
        }
    }

    #[test]
    fn stop_confirm_down_clamps_to_two_options() {
        let mut ui = default_ui();
        ui.mode = UiMode::StopConfirm;
        ui.stop_confirm_selected = 0;

        // Two presses on Down — selection must stop at index 1 (Yes),
        // proving STOP_OPTION_COUNT == 2.
        handle_stop_confirm_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.stop_confirm_selected, 1);
        handle_stop_confirm_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut ui);
        assert_eq!(ui.stop_confirm_selected, 1);
    }
}
