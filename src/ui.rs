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
use crate::event::{AgentType, EventType, OrchestrationSurface};
use crate::features::Features;
// PRD #80 introduced a UI-dispatch `Action` enum in this module (the renamed
// `KeyResult`), which collides with the keybinding-action enum. Import the
// latter under an alias so both coexist: `Action` = the UI dispatch action,
// `KbAction` = a remappable keybinding action (MoveDown, Help, …).
use crate::keybindings::{Action as KbAction, KeybindingConfig};
use crate::palette;
use crate::pane::{AgentSpawnOptions, PaneController, PaneError, RenameOutcome};
use crate::project_config::{ModeConfig, OrchestrationConfig, load_project_config};
use crate::state::{AppState, DashboardStats, SessionState, SessionStatus, SharedState};
use crate::tab::{OrchestrationRoleStatus, OrchestrationStatus, Tab, TabId, TabManager};
use crate::tab_layout::fit_tab_labels;
use crate::terminal_widget::TerminalWidget;

// ---------------------------------------------------------------------------
// Terminal-relative text styles (PRD #13)
// ---------------------------------------------------------------------------
//
// Every neutral color the dashboard emits is expressed in the terminal's own
// frame of reference, so it stays legible on both light and dark terminals —
// no absolute `Color::Rgb(..)` is painted on text or contrast-critical
// surfaces. Backgrounds are left as the terminal default (`Color::Reset`),
// primary text uses the terminal's default foreground, secondary/muted text
// dims that same foreground (rather than hardcoding a gray), and selection /
// active-tab highlights invert in place via `Modifier::REVERSED`. Semantic
// accent/status colors stay as named ANSI (Cyan/Yellow/Red/Green/Blue/
// Magenta), which terminal themes already remap.

/// Primary / body text: the terminal's default foreground.
fn text_primary() -> Style {
    Style::default().fg(Color::Reset)
}

/// Secondary / muted text: the terminal foreground, dimmed *relative* to it
/// (not a hardcoded gray) so it reads as de-emphasized on any background.
fn text_dim() -> Style {
    Style::default()
        .fg(Color::Reset)
        .add_modifier(Modifier::DIM)
}

impl fmt::Display for crate::event::AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            crate::event::AgentType::ClaudeCode => write!(f, "ClaudeCode"),
            crate::event::AgentType::OpenCode => write!(f, "OpenCode"),
            crate::event::AgentType::Pi => write!(f, "Pi"),
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
    Compact,  // wide 5 / narrow 6 rows: 1 prompt, 1 tool
    Normal,   // wide 8 / narrow 9 rows: 1 prompt, 3 tools
    Spacious, // wide 10 / narrow 11 rows: 3 prompts, 3 tools
}

impl CardDensity {
    /// Card height in rows, derived from the exact lines `render_session_card`
    /// emits so reserved height never drifts from rendered content:
    ///   Dir (1) + prompts + [narrow: inline stats line] + [non-compact: blank
    ///   separator] + tools, plus 2 rows for the top/bottom border.
    ///
    /// Resulting heights — wide: Compact 5, Normal 8, Spacious 10;
    /// narrow: 6 / 9 / 11.
    fn card_height(self, wide: bool) -> u16 {
        let prompts = self.max_prompts() as u16;
        let tools = self.max_tools() as u16;
        let stats_line = if wide { 0 } else { 1 };
        let separator = if matches!(self, CardDensity::Compact) {
            0
        } else {
            1
        };
        (1 + prompts + stats_line + separator + tools) + 2 // +2 top/bottom border
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

/// Clamp a (possibly stale) vertical scroll offset to the largest value that
/// still fills the visible window: `scroll_offset.min(total_rows - visible_rows)`
/// (saturating at 0).
///
/// When a terminal resize *grows* `visible_rows` so that more — or all — card
/// rows now fit, an offset left over from the previous overflow state would
/// otherwise scroll the top rows off and leave blank space below the last card
/// until the next navigation keystroke. This only ever *reduces* an over-large
/// offset; while content genuinely overflows (`total_rows > visible_rows`) a
/// legitimate offset is returned unchanged, so normal scrolling is untouched.
///
/// Crate-private: the only consumers are the render call site and the in-crate
/// unit test, so it needs no crate-external visibility.
pub(crate) fn clamp_scroll_offset(
    scroll_offset: usize,
    total_rows: usize,
    visible_rows: usize,
) -> usize {
    scroll_offset.min(total_rows.saturating_sub(visible_rows))
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
    /// PRD #127 M3.3: the "Scheduled Tasks" manager dialog — a
    /// read-only-plus-actions list of the configured schedules (status +
    /// next-fire) with add/edit (seeded authoring agent), delete-with-confirm
    /// (definition only), and run-now actions.
    ScheduledTasks,
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
        /// PRD #83: which pane has visual focus, keyed by stable pane id
        /// (`None` = agent pane). Mirrors `Tab::Mode::focused_pane_id`.
        focused_pane_id: Option<String>,
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

/// PRD #127 M3.2: display name of the built-in "schedule" authoring option in
/// the new-deck dialog's Mode cycler. It is NOT a per-project `[[modes]]`
/// entry — it is appended to the end of the cycle and spawns a throwaway
/// authoring agent pre-seeded with [`SCHEDULE_AUTHORING_SEED_PROMPT`].
const SCHEDULE_MODE_NAME: &str = "schedule";

/// PRD #120: display name of the flag-gated issue-dispatch authoring option in
/// the new-pane Mode cycler — distinct from the plain `schedule` option. It is
/// appended AFTER `schedule` and shown only when
/// [`crate::features::show_issue_dispatch_authoring`] is true. Selecting it
/// spawns a throwaway authoring agent seeded with
/// [`ISSUE_DISPATCH_AUTHORING_SEED_PROMPT`], which calls `schedule add --repo …`.
const ISSUE_DISPATCH_MODE_NAME: &str = "schedule: issues";

/// PRD #170 round 2 (reviewer finding 1): the fallback agent command for a
/// scheduled-task authoring session when no `default_command` is configured. The
/// Command field is free-text (the clickable preset picker is gone), so only this
/// blank-case default remains — `claude`, the simple default that launches a real
/// conversational agent.
const DEFAULT_AUTHORING_COMMAND: &str = "claude";

/// PRD #170 round 2 (reviewer findings 1 & 3): resolve the authoring command
/// for a scheduled-task authoring session. The authoring agent MUST be a real
/// conversational agent that can act on the seed prompt and call the `schedule
/// add` CLI — never a bare `$SHELL`. So a blank/whitespace `default_command`
/// (the unconfigured-user case: `config.rs` defaults it to `String::new`) falls
/// back to [`DEFAULT_AUTHORING_COMMAND`] (`claude`); a configured value is used
/// as-is (trimmed). Shared by both authoring spawn sites so the fallback is
/// applied uniformly.
fn resolve_authoring_command(default_command: &str) -> String {
    let trimmed = default_command.trim();
    if trimmed.is_empty() {
        DEFAULT_AUTHORING_COMMAND.to_string()
    } else {
        trimmed.to_string()
    }
}

/// PRD #127 M3.2: the crisp seed prompt delivered (gated, like orchestrations)
/// to the "schedule" authoring agent. It instructs the agent to converse with
/// the user, then call the validated `dot-agent-deck schedule add` CLI — it
/// NEVER freehand-edits the TOML. Carries the field list, the exact invocation,
/// the validation rules, the test-in-session affordance, and the
/// confirm-before-write requirement.
const SCHEDULE_AUTHORING_SEED_PROMPT: &str = "\
You are helping the user create a cron-scheduled prompt for dot-agent-deck. \
This is a throwaway authoring session: converse to build ONE schedule entry, write it, then you are done.

Collect these fields:
- name: unique id for the schedule (also the reuse-tab key; renaming is forbidden — to rename, remove + add).
- cron: a cron expression (5-field POSIX, e.g. \"0 9 * * MON-FRI\", evaluated in local time).
- working_dir: directory the prompt runs in (the CLI expands ~ and $VAR — pass them literally).
- command: REQUIRED — the command that launches the single-agent card. It must RESULT IN a \"claude\" or \"opencode\" process: either run one directly (\"claude\", \"claude --model opus\", \"opencode --model gpt-4o\") OR use a project wrapper that ends up launching one (e.g. \"devbox run agent-new\", \"npm run agent\", \"task agent\"). Those are the two CLIs the deck integrates with for live status tracking — a command that does NOT result in claude/opencode still runs but gets no status tracking, so prefer one of them and don't suggest unrelated CLIs (e.g. gemini). ALWAYS ask the user what launches their agent (bare \"claude\"/\"opencode\" is the simple default) and ALWAYS pass --command; a scheduled task needs an agent to act on its prompt (there is no $SHELL fallback). Ignored only when working_dir has an [[orchestrations]] block (the orchestration's role commands win).
- prompt: the prompt text to deliver on each fire.
- new_tab_per_fire: true to open a fresh tab every fire, false (default) to reuse one tab.
- enabled: true (default) or false.

Rules:
- NEVER edit the TOML file directly. ALWAYS write via the validated CLI, which checks the cron, expands paths, and writes the global config atomically:
  dot-agent-deck schedule add --name <name> --cron <cron> --working-dir <dir> --command <cmd> --prompt <text> [--new-tab-per-fire <true|false>] [--enabled <true|false>]
- The user can TEST the prompt in THIS session before committing — offer to run it now and show them the result (\"run it now, show me\").
- CONFIRM the full entry (every field) with the user before you call `schedule add`.
- AFTER `schedule add` succeeds, tell the user this authoring pane existed ONLY to create the schedule and can be closed now — when the schedule fires, a single-agent run surfaces live in its own pane on the deck, while an orchestration-targeted run appears in its tab when the deck is (re)opened.";

/// PRD #120: the seed prompt for the flag-gated `schedule: issues` authoring
/// option. DISTINCT from [`SCHEDULE_AUTHORING_SEED_PROMPT`]: it authors an
/// ISSUE-DISPATCH task — on each fire the daemon enumerates a repo's open issues
/// and dispatches one agent per issue into a per-issue worktree — so it gathers
/// the GitHub knobs (`repo`, `max_per_run`, optional `label`/`query`) and calls
/// `dot-agent-deck schedule add --repo …` (NOT the plain `schedule add --name`
/// single-spawn form). The `{{issue_number}}` placeholder in the prompt template
/// is substituted per issue at fire time.
const ISSUE_DISPATCH_AUTHORING_SEED_PROMPT: &str = "\
You are helping the user create a SCHEDULED GITHUB ISSUE-DISPATCH task for dot-agent-deck. \
This is a throwaway authoring session: converse to build ONE issue-dispatch schedule, write it, then you are done.

On each fire this task enumerates the OPEN ISSUES of a single GitHub repo and dispatches one agent per issue, \
each in its own per-issue git worktree (branch `agent/issue-<n>`), reusing the prompt as a per-issue template.

Collect these fields:
- name: unique id for the schedule (also the reuse-tab key; renaming is forbidden — to rename, remove + add).
- repo: the target GitHub repo as an `owner/name` slug (e.g. \"vfarcic/dot-ai\"). EXACTLY ONE repo per task — for several repos, create several schedules.
- cron: a cron expression (5-field POSIX, e.g. \"0 9 * * MON-FRI\", evaluated in local time).
- working_dir: the workspace ROOT the repo is cloned under on each fire (the CLI expands ~ and $VAR — pass them literally).
- max_per_run: the per-fire cap on how many open issues are dispatched (default 3). Keep it small so a backlog doesn't fan out into dozens of agents at once.
- label (optional): only dispatch issues carrying this label (e.g. \"agent-eligible\").
- query (optional): an advanced raw `gh` search-query override; leave it off to use the default \"all open issues up to max_per_run\" listing.
- prompt: the per-issue prompt template delivered to each dispatched agent. Use the `{{issue_number}}` placeholder — it is substituted with each issue's number at fire time (e.g. \"fix issue {{issue_number}}\").

Rules:
- NEVER edit the TOML file directly. ALWAYS write via the validated CLI, which checks the cron, validates the repo slug, expands paths, and writes the global config atomically:
  dot-agent-deck schedule add --repo <owner/name> --max-per-run <N> --name <name> --cron <cron> --working-dir <dir> --prompt <template> [--label <label>] [--query <query>]
- Do NOT pass --command: an issue-dispatch task needs none (the per-issue agent command comes from each cloned repo's config / the deck's default_command).
- CONFIRM the full entry (every field, especially repo and max_per_run) with the user before you call `schedule add`.
- AFTER `schedule add` succeeds, tell the user this authoring pane existed ONLY to create the schedule and can be closed now — when the schedule fires, each dispatched issue surfaces live as its own tab on the deck.";

// ---------------------------------------------------------------------------
// "Scheduled Tasks" management dialog (PRD #127 M3.3)
// ---------------------------------------------------------------------------

/// Status of a schedule as shown in the manager dialog. "disabled" when
/// `enabled = false`; "live" when the task currently has a live reused
/// tab/agent; "idle" otherwise.
fn schedule_status_label(enabled: bool, is_live: bool) -> &'static str {
    if !enabled {
        "disabled"
    } else if is_live {
        "live"
    } else {
        "idle"
    }
}

/// Next-fire cell for the manager list: the `—` placeholder for a disabled
/// task, otherwise the next occurrence of the task's cron (local time) or `—`
/// when the cron yields no upcoming time / fails to parse (writer-validated
/// crons always parse, so this is defensive).
fn schedule_next_fire_display(task: &crate::config::ScheduledTask) -> String {
    if !task.enabled {
        return "\u{2014}".to_string(); // —
    }
    match crate::scheduler::parse_cron(&task.cron) {
        Ok(schedule) => match schedule.upcoming(chrono::Local).next() {
            Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
            None => "\u{2014}".to_string(),
        },
        Err(_) => "\u{2014}".to_string(),
    }
}

/// Build the "schedule" authoring `ModeConfig` for the manager's add/edit
/// actions (PRD #127 M3.3). Both reuse the 3B-i seeded authoring agent. For
/// **add**, the seed is the base [`SCHEDULE_AUTHORING_SEED_PROMPT`]. For
/// **edit**, the existing entry's current values are injected so the agent
/// starts from them and calls `schedule update` (NOT `add`); renaming is
/// forbidden (the `name` is the reuse-registry key — to rename, remove + add).
///
/// PRD #170 (unify): `working_dir` is the directory picked in the dir-picker
/// (the cwd the authoring session is launched in). It is appended as the
/// schedule's `working_dir` DEFAULT so the agent's `schedule add/update`
/// naturally targets the picked dir unless the user names another.
///
/// PRD #170 round 2 (reviewer finding 3): on **edit** the existing-values block
/// lists the PICKED `working_dir` (not the row's stale stored one) so it can
/// never conflict with the `working_dir DEFAULT:` line — re-picking a different
/// directory wins, and an unchanged pick (the picker opens at the row's dir)
/// reproduces the stored value.
fn build_schedule_authoring_mode(
    existing: Option<&crate::config::ScheduledTask>,
    working_dir: &std::path::Path,
) -> ModeConfig {
    let base = format!(
        "{seed}\n\n\
         working_dir DEFAULT: {dir} (the directory this authoring session was launched in) \
         — use it as the schedule's working_dir unless the user names another.",
        seed = SCHEDULE_AUTHORING_SEED_PROMPT,
        dir = working_dir.display(),
    );
    let seed = match existing {
        None => base,
        Some(t) => {
            let command = t.command.clone().unwrap_or_default();
            format!(
                "{base}\n\n\
                 You are EDITING the existing schedule {name:?}. Its current values are:\n\
                 - name: {name}\n\
                 - cron: {cron}\n\
                 - working_dir: {working_dir}\n\
                 - command: {command}\n\
                 - prompt: {prompt}\n\
                 - new_tab_per_fire: {ntpf}\n\
                 - enabled: {enabled}\n\
                 Start from these values and write changes with \
                 `dot-agent-deck schedule update --name {name} ...` (NOT `add`). \
                 RENAME IS FORBIDDEN — the name {name:?} is fixed (it is the reuse-tab key); \
                 to rename, remove this schedule and add a new one.",
                base = base,
                name = t.name,
                cron = t.cron,
                // PRD #170 finding 3: the PICKED dir (not the row's stale stored
                // one) so this current-value line agrees with `working_dir DEFAULT`.
                working_dir = working_dir.display(),
                command = command,
                prompt = t.prompt,
                ntpf = t.new_tab_per_fire,
                enabled = t.enabled,
            )
        }
    };
    ModeConfig {
        name: SCHEDULE_MODE_NAME.to_string(),
        init_command: None,
        seed_prompt: Some(seed),
        panes: Vec::new(),
        rules: Vec::new(),
        reactive_panes: 0,
    }
}

/// PRD #120: build the issue-dispatch authoring seed (base
/// [`ISSUE_DISPATCH_AUTHORING_SEED_PROMPT`] + the picked dir as the workspace
/// `working_dir` DEFAULT). Like the plain-schedule seed, the picked directory is
/// appended so the agent's `schedule add --repo …` naturally targets it unless
/// the user names another. There is no Edit variant — the manager's Add/Edit is
/// the plain-schedule door; issue-dispatch authoring is created fresh from the
/// new-pane cycler only.
fn build_issue_dispatch_authoring_seed(working_dir: &std::path::Path) -> String {
    format!(
        "{seed}\n\n\
         working_dir DEFAULT: {dir} (the directory this authoring session was launched in) \
         — use it as the schedule's working_dir unless the user names another.",
        seed = ISSUE_DISPATCH_AUTHORING_SEED_PROMPT,
        dir = working_dir.display(),
    )
}

/// Send a one-shot `AttachRequest` to the local daemon over the attach socket
/// and read the single `AttachResponse`, synchronously (no tokio runtime — the
/// TUI key path is sync). Used by the manager dialog's run-now and the
/// reload-after-delete. Returns `Err` on any transport problem; callers treat
/// a down daemon as best-effort.
fn send_daemon_request_blocking(
    req: &crate::daemon_protocol::AttachRequest,
) -> std::io::Result<crate::daemon_protocol::AttachResponse> {
    use crate::daemon_protocol::{KIND_REQ, KIND_RESP};
    use std::io::{Read, Write};

    let path = config::attach_socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;

    let payload = serde_json::to_vec(req).map_err(std::io::Error::other)?;
    let mut header = [0u8; 5];
    header[0] = KIND_REQ;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(&payload)?;
    stream.flush()?;

    let mut resp_header = [0u8; 5];
    stream.read_exact(&mut resp_header)?;
    if resp_header[0] != KIND_RESP {
        return Err(std::io::Error::other(format!(
            "expected RESP frame, got kind 0x{:02x}",
            resp_header[0]
        )));
    }
    let len = u32::from_be_bytes([
        resp_header[1],
        resp_header[2],
        resp_header[3],
        resp_header[4],
    ]) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

/// Names of schedules that currently have a live (non-exited) tab/agent —
/// derived from the daemon's `ListAgents`. One-shot socket query at dialog-open
/// time; a down daemon degrades to "no live tasks" (all idle).
///
/// PRD #127 N3: only agents whose `pane_id_env` carries the scheduler's
/// `sched-` prefix are counted, so a manually-spawned agent that happens to
/// share a schedule's display name can't be mistaken for a live fire. The task
/// name is then read from the agent's `display_name` (the scheduler sets it to
/// the task name at spawn).
fn live_schedule_names() -> HashSet<String> {
    match send_daemon_request_blocking(&crate::daemon_protocol::AttachRequest::ListAgents) {
        Ok(resp) => resp
            .agent_records
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                r.pane_id_env
                    .as_deref()
                    .is_some_and(|p| p.starts_with(crate::spawn::SCHEDULE_PANE_ID_PREFIX))
            })
            .filter_map(|r| r.display_name)
            .collect(),
        Err(_) => HashSet::new(),
    }
}

/// PRD #80 M8: which new-pane-form field is focused. Public because it rides
/// in [`Action::FormFocusField`] (a click on a field's row focuses it, the
/// same as Tab landing there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Mode,
    Name,
    Command,
}

/// PRD #170 (unify): why the directory picker is open — which form to build
/// once a directory is confirmed (in [`transition_after_dir_pick`]). `Ctrl+n`
/// opens it for an ordinary new pane; the Scheduled-Tasks manager's Add/Edit
/// reuse the SAME picker, then build the schedule-locked form (carrying the
/// edit row for `ScheduleEdit`). Always set at every picker-open site and reset
/// to `NewPane` once consumed, so a later `Ctrl+n` is never poisoned.
enum DirPickerIntent {
    /// `Ctrl+n` — build the ordinary new-pane form ([`NewPaneFormState::new`]).
    NewPane,
    /// Manager Add — build the schedule-locked form with no edit row.
    ScheduleAdd,
    /// Manager Edit — build the schedule-locked form pre-filled from this row
    /// (boxed: the variant is far larger than the others).
    ScheduleEdit(Box<config::ScheduledTask>),
}

struct NewPaneFormState {
    dir: PathBuf,
    name: String,
    command: String,
    // Mode/orchestration selection fields
    modes: Vec<ModeConfig>,
    orchestrations: Vec<OrchestrationConfig>,
    /// PRD #127 M3.2: the built-in "schedule" authoring mode, appended to the
    /// end of the Mode cycler (after the project's workload modes and
    /// orchestrations). Carries the authoring seed prompt; selecting it spawns
    /// a pre-seeded conversational agent via the same gated delivery path
    /// modes use (Phase 3A).
    schedule_authoring: ModeConfig,
    /// PRD #120: the flag-gated issue-dispatch authoring option, appended AFTER
    /// `schedule_authoring` in the cycler. Only the display `name` matters (the
    /// seed is derived at submit time by [`build_issue_dispatch_authoring_seed`]);
    /// it is offered only when `show_issue_dispatch` is true.
    issue_dispatch_authoring: ModeConfig,
    /// PRD #120: snapshot of [`crate::features::show_issue_dispatch_authoring`]
    /// taken at form-construction time (the input seam). When true the cycler
    /// offers the `schedule: issues` option after `schedule`; when false it is
    /// hidden and the cycler shape is byte-for-byte the pre-feature baseline.
    show_issue_dispatch: bool,
    selection_index: usize, // 0 = "No mode", 1..M = modes, M+1..M+O = orchestrations, then "schedule" [, "schedule: issues"]
    has_mode_field: bool,
    focused: FormField,
    /// PRD #170 (unify): when `true` the form is MODE-LOCKED to schedule
    /// authoring — the manager's Add/Edit reuses this same form but hides the
    /// Mode cycler and the Name field and retitles the modal ` New Schedule ` /
    /// ` Edit Schedule `. `false` is the ordinary `Ctrl+n` new-pane form (Mode +
    /// Name visible), left byte-for-byte unchanged. Built via
    /// [`NewPaneFormState::new_schedule_locked`]; `new` always sets it `false`.
    schedule_locked: bool,
    /// PRD #170 (unify): the existing schedule on a manager **Edit** (`None` on
    /// **Add**). Drives the authoring-seed pre-fill (the agent starts from the
    /// row's values and calls `schedule update`) and the ` Edit Schedule ` title.
    /// Only ever `Some` when `schedule_locked` is `true`.
    schedule_existing: Option<config::ScheduledTask>,
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
        // PRD #127 M3.2: the built-in "schedule" authoring option is always
        // available, so the Mode field always shows (at minimum "No mode" and
        // "schedule") and the form opens focused on it.
        let has_mode_field = true;
        // PRD #170 round 2 (reviewer finding 7): the synthetic mode only supplies
        // the cycler's display `name` — the authoring seed is derived at submit
        // time by `build_schedule_authoring_mode` (threaded with the picked dir),
        // so `seed_prompt` here is dead data; leave it `None`.
        let schedule_authoring = ModeConfig {
            name: SCHEDULE_MODE_NAME.to_string(),
            init_command: None,
            seed_prompt: None,
            panes: Vec::new(),
            rules: Vec::new(),
            reactive_panes: 0,
        };
        // PRD #120: synthetic issue-dispatch authoring option (name only; seed
        // derived at submit time). Whether it is offered is the flag snapshot.
        let issue_dispatch_authoring = ModeConfig {
            name: ISSUE_DISPATCH_MODE_NAME.to_string(),
            init_command: None,
            seed_prompt: None,
            panes: Vec::new(),
            rules: Vec::new(),
            reactive_panes: 0,
        };
        Self {
            dir,
            name,
            command,
            modes,
            orchestrations,
            schedule_authoring,
            issue_dispatch_authoring,
            // PRD #120: gate at the input seam (CLAUDE.md #9) — snapshot the
            // render wrapper once at construction so the count/name/cycler-cap
            // all observe one consistent value.
            show_issue_dispatch: crate::features::show_issue_dispatch_authoring(),
            selection_index: 0,
            has_mode_field,
            focused: FormField::Mode,
            // PRD #170: the ordinary `Ctrl+n` form is never locked.
            schedule_locked: false,
            schedule_existing: None,
        }
    }

    /// PRD #170 (unify): build the new-pane form MODE-LOCKED to schedule
    /// authoring — the shape the Scheduled-Tasks manager's Add/Edit opens after
    /// the directory picker (reusing the `Ctrl+n` form instead of a bespoke
    /// modal). `modes`/`orchestrations` are left empty so `schedule_index()` is
    /// `1` and the selection is fixed there (`is_schedule_selected()` is already
    /// true), the card name is fixed to `SCHEDULE_MODE_NAME` (the schedule's own
    /// name is authored conversationally), focus opens on the free-text Command
    /// field (pre-filled by the caller from the resolved `default_command`), and
    /// `schedule_locked` drives the render branches that hide the Mode cycler +
    /// Name field and retitle the modal. `existing` is `Some(row)` on Edit (for
    /// the authoring-seed pre-fill), `None` on Add.
    fn new_schedule_locked(
        dir: PathBuf,
        command: String,
        existing: Option<config::ScheduledTask>,
    ) -> Self {
        // PRD #170 round 2 (reviewer finding 7): seed is derived at submit time by
        // `build_schedule_authoring_mode`; the synthetic mode only carries `name`.
        let schedule_authoring = ModeConfig {
            name: SCHEDULE_MODE_NAME.to_string(),
            init_command: None,
            seed_prompt: None,
            panes: Vec::new(),
            rules: Vec::new(),
            reactive_panes: 0,
        };
        let issue_dispatch_authoring = ModeConfig {
            name: ISSUE_DISPATCH_MODE_NAME.to_string(),
            init_command: None,
            seed_prompt: None,
            panes: Vec::new(),
            rules: Vec::new(),
            reactive_panes: 0,
        };
        let mut form = Self {
            dir,
            name: SCHEDULE_MODE_NAME.to_string(),
            command,
            modes: Vec::new(),
            orchestrations: Vec::new(),
            schedule_authoring,
            issue_dispatch_authoring,
            // PRD #120: the manager's mode-locked form is plain-schedule authoring
            // only — the issue-dispatch option lives on the `Ctrl+n` cycler, and
            // the locked form hides the cycler entirely, so it never appears here.
            show_issue_dispatch: false,
            selection_index: 0,
            has_mode_field: true,
            focused: FormField::Command,
            schedule_locked: true,
            schedule_existing: existing,
        };
        // Lock the selection onto the built-in schedule option (index 1 with no
        // modes/orchestrations) so the existing schedule spawn branch fires.
        form.selection_index = form.schedule_index();
        form
    }

    /// Cycler index of the built-in "schedule" authoring option — after the
    /// project's workload modes and orchestrations (PRD #120: the optional
    /// `schedule: issues` option, when shown, follows it).
    fn schedule_index(&self) -> usize {
        1 + self.modes.len() + self.orchestrations.len()
    }

    /// PRD #120: cycler index of the flag-gated `schedule: issues` option —
    /// appended directly after `schedule`. Only meaningful when
    /// `show_issue_dispatch` is true.
    fn issue_dispatch_index(&self) -> usize {
        self.schedule_index() + 1
    }

    /// Whether the built-in "schedule" authoring option is currently selected.
    fn is_schedule_selected(&self) -> bool {
        self.selection_index == self.schedule_index()
    }

    /// PRD #120: whether the flag-gated `schedule: issues` option is selected.
    fn is_issue_dispatch_selected(&self) -> bool {
        self.show_issue_dispatch && self.selection_index == self.issue_dispatch_index()
    }

    /// PRD #120: whether the current selection is a throwaway authoring option
    /// (plain `schedule` OR `schedule: issues`). Drives the shared
    /// "↳ authoring (one-off)" hint + its reserved render row.
    fn is_authoring_selected(&self) -> bool {
        self.is_schedule_selected() || self.is_issue_dispatch_selected()
    }

    fn mode_option_count(&self) -> usize {
        // +1 for the built-in "schedule" authoring option appended at the end,
        // +1 more for the flag-gated "schedule: issues" option when shown.
        1 + self.modes.len()
            + self.orchestrations.len()
            + 1
            + if self.show_issue_dispatch { 1 } else { 0 }
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
        // PRD #120: the flag-gated issue-dispatch authoring option — its synthetic
        // mode supplies the cycler's title/chip ("schedule: issues mode"); the
        // spawned request swaps in the issue-dispatch seed (see
        // `build_new_pane_request`).
        if self.is_issue_dispatch_selected() {
            return Some(&self.issue_dispatch_authoring);
        }
        if self.is_schedule_selected() {
            // The built-in authoring mode — spawns a seeded agent like any
            // mode with a `seed_prompt`.
            return Some(&self.schedule_authoring);
        }
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

    /// PRD #80 M8: display name for the mode option at `idx` (0 = "No mode",
    /// 1..=modes = mode names, then orchestrations, then the built-in
    /// "schedule" authoring option at the end). Used to render one clickable
    /// chip per option.
    fn mode_option_name(&self, idx: usize) -> String {
        if idx == 0 {
            "No mode".to_string()
        } else if idx <= self.modes.len() {
            self.modes[idx - 1].name.clone()
        } else if idx <= self.modes.len() + self.orchestrations.len() {
            let orch_idx = idx - 1 - self.modes.len();
            let name = &self.orchestrations[orch_idx].name;
            if name.is_empty() {
                "Orchestration".to_string()
            } else {
                format!("Orch: {name}")
            }
        } else if self.show_issue_dispatch && idx == self.issue_dispatch_index() {
            // PRD #120: the flag-gated issue-dispatch authoring option.
            ISSUE_DISPATCH_MODE_NAME.to_string()
        } else {
            // PRD #127 M3.2: built-in "schedule" authoring option.
            SCHEDULE_MODE_NAME.to_string()
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
        // PRD #170: the locked schedule form has a single navigable field
        // (Command) — Mode + Name are hidden, so Tab is a no-op.
        if self.schedule_locked {
            return FormField::Command;
        }
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
        // PRD #170: symmetric to `next_field` — the locked schedule form's only
        // navigable field is Command.
        if self.schedule_locked {
            return FormField::Command;
        }
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

/// PRD #89 M1.2 — throttle window for continuous saved-session snapshot writes.
/// A meaningful state change marks the snapshot dirty; the main loop flushes it
/// at most once per this interval (plus one trailing write per quiet-down), so
/// a burst of changes coalesces to one or two disk writes instead of thrashing
/// the disk on every keystroke. Small enough that a fresh snapshot lands well
/// within a second of any change; large enough to absorb an orchestration
/// setup's pane-spawn burst into a single trailing write.
const SNAPSHOT_COALESCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(750);

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

/// PRD #127 M3.1: a mode's `seed_prompt` queued for delivery to its agent pane,
/// gated exactly like the orchestrator's spawn-time role prompt — agent-ready
/// (SessionStart, fast path) or a 10s timeout (slow path), plus the
/// `SPAWN_TIME_READINESS_BUFFER`, then an atomic `write_and_submit_to_pane`.
/// `ready_since` records when the agent first signalled ready so the buffer is
/// measured from that instant (mirrors `orchestration_ready_since`).
struct PendingSeedPrompt {
    pane_id: String,
    prompt: String,
    created_at: std::time::Instant,
    ready_since: Option<std::time::Instant>,
}

struct UiState {
    mode: UiMode,
    /// PRD #113: the Dashboard's selection is active/inactive. `Some(i)` paints
    /// the blue highlight on flat card index `i`; `None` (set when the user
    /// switches tabs away) means no card is highlighted, so the visual state
    /// can't go stale relative to what's actually armed. The Orchestration tab
    /// reuses this field but keeps its selection always active (`Some`).
    selected_index: Option<usize>,
    /// PRD #113 (PR #151 manual-test fix): the embedded controller's focused
    /// pane id as of the previous `reconcile_dashboard_selection` frame. The
    /// focused-pane sync (M4) reactivates the highlight only on a genuine focus
    /// *transition* — when the focused pane CHANGED to a visible dashboard card
    /// — not on a steady-state focus the tab-switch restore leaves in place
    /// (e.g. a Mode tab's agent pane, which is a dashboard card and stays
    /// focused on return). Without this, switching away and back re-armed the
    /// highlight a tab switch had cleared (violating SC1).
    last_focused_pane_id: Option<String>,
    /// PRD #113 design revision (2026-06-13): the last *active* selection of the
    /// CURRENTLY-ACTIVE deck, remembered across tab switches so Enter can RESTORE
    /// it when the deck is inactive (no live highlight) instead of jumping to
    /// card 0. This is a cache of `last_active_selection_by_deck`'s entry for the
    /// active deck, refreshed on every deck switch-in (`switch_tab_with_focus`)
    /// so the signature-frozen `dashboard_focus_target(&ui, total)` can read the
    /// active deck's value without the tab. Kept SEPARATE from the live
    /// `selected_index` / `selected_session_id` so the switch-in re-focus can't
    /// auto-reactivate it — it is consumed ONLY on an explicit Enter.
    last_active_selection: Option<usize>,
    /// PRD #113 revision (PR #151 review #2): the remembered Enter-restore
    /// selection PER DECK — the source of truth that `last_active_selection`
    /// above caches. Keyed by deck identity (the single Dashboard, or a specific
    /// Orchestration tab by its stable id, so multiple orchestrations each keep
    /// their own remembered role). Recorded into the OUTGOING deck's entry on
    /// leave and hoisted into the cache for the INCOMING deck on switch-in.
    /// Storing per-deck stops one deck's armed index from leaking into another
    /// deck's Enter-restore (the single shared field used to leak —
    /// orchestration_005). Only ever stores a `Some`: leaving a deck while
    /// inactive must not overwrite its entry (or a round-trip's return leg would
    /// clobber the remembered value).
    last_active_selection_by_deck: HashMap<DeckKey, usize>,
    filter_text: String,
    rename_text: String,
    display_names: HashMap<String, String>,
    columns: usize,
    scroll_offset: usize,
    status_message: Option<(String, std::time::Instant)>,
    dir_picker: Option<DirPickerState>,
    /// PRD #170 (unify): why `dir_picker` is open — selects which form
    /// [`transition_after_dir_pick`] builds when the directory is confirmed.
    /// Set at every picker-open site; reset to `NewPane` once consumed.
    dir_picker_intent: DirPickerIntent,
    new_pane_form: Option<NewPaneFormState>,
    pane_names: HashMap<String, String>,
    /// Maps pane_id → display name; survives session restarts (e.g. /clear).
    pane_display_names: HashMap<String, String>,
    /// Maps pane_id → launch metadata for auto-save/restore.
    pane_metadata: HashMap<String, config::SavedPane>,
    config: DashboardConfig,
    /// PRD #40: active keybinding config (resolved client-side at startup).
    /// Drives command-mode key dispatch and the dynamically-generated help
    /// overlay / hints bar. Defaults reproduce today's hardcoded bindings.
    keybindings: KeybindingConfig,
    /// Tracks last-seen status per session for bell transition detection.
    last_bell_status: HashMap<String, SessionStatus>,
    /// Populated by the background version-check task when a newer release is available.
    update_available: Option<String>,
    /// Layout mode for embedded terminal panes (stacked or tiled).
    pane_layout: PaneLayout,
    /// Warnings collected during session save/restore, flushed after terminal restore.
    session_warnings: Vec<String>,
    /// PRD #89 review-fix G1: tracks whether the most recent periodic snapshot
    /// write (in `flush_session_snapshot_if_due`) failed. F10 keeps the
    /// coalescer dirty on failure so the next loop retries; on a *persistent*
    /// failure that retry fires every ~750ms, so we push the warning only on
    /// the leading edge of a failure streak and suppress duplicates until a
    /// later write succeeds (which clears this flag, re-arming the warning for
    /// any subsequent new failure).
    session_snapshot_write_failed: bool,
    /// Mouse text selection state for copy support.
    selection: Option<TextSelection>,
    /// Screen rect of the focused pane (set during render, used for mouse mapping).
    focused_pane_rect: Option<Rect>,
    /// Screen rects of side panes in mode tabs (set during render, used for scroll hit-testing).
    side_pane_rects: Vec<(String, Rect)>,
    /// Screen rect of the agent pane in mode tabs (set during render, used for click-to-focus).
    agent_pane_rect: Option<Rect>,
    /// Tracks last click time and position for double/triple-click detection.
    last_click: Option<LastClick>, // PRD #80 review FIX 4: region-aware multi-click state
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
    /// PRD #127 M3.3: Scheduled-Tasks manager row rects, paired with the row's
    /// index into `scheduled_tasks` (matching `scheduled_selected`). A click
    /// selects the row — the keyboard parity for `j`/`k`. Populated by
    /// `render_overlays` while the manager dialog is shown (and empty while its
    /// delete confirmation is armed), cleared otherwise. Mirrors
    /// [`UiState::picker_row_rects`].
    scheduled_row_rects: Vec<(usize, Rect)>,
    /// PRD #80 M8: new-pane-form clickable field rows, paired with the
    /// [`FormField`] each focuses. Populated while the form is shown, cleared
    /// otherwise.
    form_field_rects: Vec<(FormField, Rect)>,
    /// PRD #80 M8: new-pane-form mode chips, paired with the mode-option index
    /// each selects (0 = "No mode", 1.. = modes/orchestrations).
    form_chip_rects: Vec<(usize, Rect)>,
    /// PRD #80 M8: new-pane-form `[Submit]`/`[Cancel]` button rects, paired
    /// with the [`Action`] each fires.
    form_button_rects: Vec<(Action, Rect)>,
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
    /// PRD #127 M3.1: mode `seed_prompt`s waiting for their agent pane to be
    /// ready before the gated atomic submit. Parallel to `pending_dispatches`
    /// but uses the spawn-time readiness buffer + `write_and_submit_to_pane`.
    pending_seed_prompts: Vec<PendingSeedPrompt>,
    /// PRD #127 M3.3: schedules listed in the "Scheduled Tasks" manager dialog,
    /// loaded from the global config when the dialog opens.
    scheduled_tasks: Vec<config::ScheduledTask>,
    /// Selected row in the manager dialog.
    scheduled_selected: usize,
    /// True while the manager dialog is showing the delete confirmation for the
    /// selected row (`d` pressed; `y` confirms, `n`/Esc cancels).
    scheduled_delete_confirm: bool,
    /// Schedule names with a live tab/agent, snapshotted when the dialog opens
    /// (drives the live/idle status indicator).
    scheduled_live_names: HashSet<String>,
    /// PRD #76 M2.20: timestamp of the most recent keystroke forwarded to a
    /// pane via `ForwardToPane`. Drives the submit-debounce in `PaneInput` mode
    /// so an Enter keystroke arriving fused to preceding typed bytes is
    /// delayed just enough that the agent TUI treats it as a standalone submit
    /// (matches `write_to_pane`'s SUBMIT_DELAY rationale at
    /// src/embedded_pane.rs:199).
    last_pane_keystroke_at: Option<std::time::Instant>,
    /// PRD #89 M1.2/M1.3 — coalesces saved-session snapshot writes so a burst
    /// of meaningful state changes (or detaches) produces one or two disk
    /// writes, not one per change. Marked dirty by [`UiState::mark_session_dirty`]
    /// at every state-change/detach call site; flushed by
    /// `flush_session_snapshot_if_due` once per main-loop iteration.
    session_coalescer: config::SnapshotCoalescer,
    /// PRD #89 M1.2 — monotonic reference the coalescer's `Duration` clock is
    /// measured from (`session_epoch.elapsed()`), set once at construction.
    session_epoch: std::time::Instant,
}

/// PRD #80 review FIX 4: which click region produced a [`LastClick`]. Multi-
/// click (double/triple) detection only fires when the current click is in the
/// SAME region as the previous one, so a cross-region click within the 400ms
/// window can't mis-classify — e.g. a dashboard card click (screen coords)
/// followed by an in-pane click (pane-relative coords) resets to a single
/// click instead of being read as a double.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickRegion {
    /// In-pane text selection (pane-relative coordinates).
    Pane,
    /// A dashboard card (screen coordinates).
    Card,
    /// A directory-picker row (screen coordinates).
    PickerRow,
}

/// PRD #80 review FIX 4: the previous left-button-down, used for region-aware
/// double/triple-click detection.
#[derive(Debug, Clone, Copy)]
struct LastClick {
    at: std::time::Instant,
    col: u16,
    row: u16,
    count: u8,
    region: ClickRegion,
}

impl LastClick {
    /// The multi-click count for a click at `(col, row)` in `region` given the
    /// previous click `prev`. Returns `(prev.count + 1).min(3)` only when the
    /// previous click was in the SAME region, within 400ms, on the same row,
    /// and within 3 columns; otherwise `1` (a fresh single click).
    fn next_count(
        prev: Option<LastClick>,
        now: std::time::Instant,
        col: u16,
        row: u16,
        region: ClickRegion,
    ) -> u8 {
        match prev {
            Some(p)
                if p.region == region
                    && now.duration_since(p.at).as_millis() < 400
                    && p.row == row
                    && col.abs_diff(p.col) <= 3 =>
            {
                (p.count + 1).min(3)
            }
            _ => 1,
        }
    }
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
    fn new(config: DashboardConfig, keybindings: KeybindingConfig) -> Self {
        Self {
            mode: UiMode::Normal,
            // PRD #113 M1: startup default is an ACTIVE selection on card 0 so
            // first-launch UX is unchanged.
            selected_index: Some(0),
            last_focused_pane_id: None,
            // PRD #113 revision: defaults to card 0 so first-launch Enter (before
            // anything has been selected) targets the first card, unchanged.
            last_active_selection: Some(0),
            // Per-deck remembered selections; empty until a deck is left with an
            // active highlight. The active deck's entry is hoisted into
            // `last_active_selection` on switch-in.
            last_active_selection_by_deck: HashMap::new(),
            filter_text: String::new(),
            rename_text: String::new(),
            display_names: HashMap::new(),
            columns: 1,
            scroll_offset: 0,
            status_message: None,
            dir_picker: None,
            dir_picker_intent: DirPickerIntent::NewPane,
            new_pane_form: None,
            pane_names: HashMap::new(),
            pane_display_names: HashMap::new(),
            pane_metadata: HashMap::new(),
            config,
            keybindings,
            last_bell_status: HashMap::new(),
            update_available: None,
            pane_layout: PaneLayout::Stacked,
            session_warnings: Vec::new(),
            session_snapshot_write_failed: false,
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
            pending_seed_prompts: Vec::new(),
            scheduled_tasks: Vec::new(),
            scheduled_selected: 0,
            scheduled_delete_confirm: false,
            scheduled_live_names: HashSet::new(),
            last_pane_keystroke_at: None,
            session_coalescer: config::SnapshotCoalescer::new(SNAPSHOT_COALESCE_INTERVAL),
            session_epoch: std::time::Instant::now(),
            button_rects: Vec::new(),
            tab_close_rects: Vec::new(),
            tab_header_rects: Vec::new(),
            card_rects: Vec::new(),
            modal_button_rects: Vec::new(),
            picker_button_rects: Vec::new(),
            picker_row_rects: Vec::new(),
            scheduled_row_rects: Vec::new(),
            form_field_rects: Vec::new(),
            form_chip_rects: Vec::new(),
            form_button_rects: Vec::new(),
        }
    }

    /// PRD #89 M1.2/M1.3 — the single trigger every meaningful state-change and
    /// detach call site invokes (new pane, rename, close, mode/orchestration tab
    /// open/close, agent restart, Ctrl+W close-pane). It only marks the
    /// saved-session snapshot dirty; the coalesced disk write happens in the
    /// main loop via `flush_session_snapshot_if_due`, so a burst of triggers
    /// collapses to one or two writes.
    fn mark_session_dirty(&mut self) {
        self.session_coalescer.mark_dirty();
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new(DashboardConfig::default(), KeybindingConfig::default())
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
// should open at. Before this, every spawn site hardcoded 24×80, so the
// agent's first frame painted at the wrong size. The spawn callsites call
// these so the PTY opens close to its eventual size; PRD #84 M4 then made
// the per-frame `resize_panes_to_layout` pass (driven by
// `compute_frame_layout`) the single owner of resize-time PTY sizing, so
// these helpers are now spawn-time-only. All return `(rows, cols)` (=
// `(height, width)` of the inner area inside the pane's border).

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
/// `role_index` matters only in `Stacked` mode, where role 0 is the
/// expanded slot and every other role collapses to a 1-row title bar —
/// mirroring the renderer's "expand the first slot if nothing is
/// focused" fallback (see `render_terminal_panes` Stacked branch). In
/// `Tiled` mode `role_index` is ignored (height divides equally).
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
/// PRD #84 M4 removed the resize sweep that once threaded a live
/// focused-role index through this helper (so role panes could re-expand
/// on focus). The per-frame `resize_panes_to_layout` pass now owns PTY
/// sizing from `compute_frame_layout`, so the sole remaining caller is
/// the spawn path, which spawns `Tiled` with no role focused yet — hence
/// the `focused_role_index` parameter is gone and role 0 is the
/// Stacked expanded slot.
pub(crate) fn orchestration_role_pane_dims(
    frame_area: Rect,
    role_count: usize,
    role_index: usize,
    layout: PaneLayout,
    show_tab_bar: bool,
) -> (u16, u16) {
    // Stacked: role 0 is the expanded slot, mirroring the renderer's
    // "expand the first slot if nothing is focused" fallback. Tiled
    // ignores `is_focused` (equal division).
    let is_focused = role_index == 0;
    right_column_pane_dims(
        frame_area,
        ORCHESTRATION_PANES_PERCENT,
        role_count as u16,
        is_focused,
        layout,
        show_tab_bar,
    )
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
/// original radar. PRD #84 M4: the per-frame `resize_panes_to_layout`
/// pass reconciles any rounding once the pane is in the layout, but
/// spawning at the right size removes the visible 24×80 hiccup.
///
/// `is_focused=false` is used because the restore loop doesn't focus
/// any pane until after restore; the helper is forgiving in Tiled mode
/// where `is_focused` is ignored.
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
    /// PRD #107 follow-up: the user-typed tab title echoed back by the
    /// daemon on each role pane's `TabMembership::Orchestration.display_title`
    /// (shared across roles, so the first non-`None` slot wins). Routed to
    /// the rebuilt tab's TITLE so detach/reattach preserves the entered name;
    /// `None` falls back to the canonical resolved name.
    pub display_title: Option<String>,
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
                display_title,
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
                                display_title: display_title.clone(),
                                role_slots: Vec::new(),
                            });
                        i
                    }
                };
                // All role panes of one orchestration carry the same
                // display_title, but guard against a leading legacy/dead slot
                // that omitted it: keep the first non-`None` value we see.
                if out.orchestration_buckets[idx].display_title.is_none() {
                    out.orchestration_buckets[idx].display_title = display_title.clone();
                }
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

/// PRD #127 M3.1: deliver a mode's `seed_prompt` to its agent pane once the
/// agent is ready, gated exactly like the orchestrator's spawn-time role prompt
/// (`SessionStart` fast path / 10s timeout slow path, then the
/// `SPAWN_TIME_READINESS_BUFFER`, then an atomic `write_and_submit_to_pane`).
/// A mode without a `seed_prompt` never enqueues one, so this is a no-op for
/// plain modes (no regression).
fn process_pending_seed_prompts(
    ui: &mut UiState,
    pane: &Arc<dyn PaneController>,
    snapshot: &AppState,
) {
    let now = std::time::Instant::now();
    ui.pending_seed_prompts.retain_mut(|sp| {
        // Fast path: agent fired SessionStart (agent_type resolved).
        let agent_ready = snapshot.sessions.values().any(|s| {
            s.pane_id.as_deref() == Some(sp.pane_id.as_str()) && s.agent_type != AgentType::None
        });
        // Slow path: no SessionStart after 10s (e.g. opencode) — proceed anyway.
        let timeout_ready =
            !agent_ready && sp.created_at.elapsed() > std::time::Duration::from_secs(10);
        if agent_ready {
            sp.ready_since.get_or_insert(now);
        }
        // Hold the write until the readiness buffer elapses (mirrors the
        // orchestrator path). The timeout path bypasses the buffer.
        let buffer_elapsed = if timeout_ready {
            true
        } else {
            should_inject_spawn_time_prompt(sp.ready_since, now)
        };
        if (agent_ready || timeout_ready) && buffer_elapsed {
            let _ = pane.write_and_submit_to_pane(&sp.pane_id, &sp.prompt);
            return false;
        }
        // Hard timeout after 60s — give up.
        if sp.created_at.elapsed() > std::time::Duration::from_secs(60) {
            tracing::warn!(pane_id = %sp.pane_id, "seed prompt: timed out waiting for agent");
            return false;
        }
        true
    });
}

// ---------------------------------------------------------------------------
// PRD #120: live orchestration surfacing
// ---------------------------------------------------------------------------

/// PRD #120: build live orchestration tabs for any orchestrations the daemon
/// spawned while this TUI is attached (the issue-dispatch path), queued by the
/// event subscriber into [`AppState::pending_orchestration_surfaces`].
///
/// Drains the queue, attaches each role's daemon PTY, and rebuilds the
/// orchestration tab through the SAME config-resolution + tab-build machinery
/// the reconnect-hydration path uses ([`resolve_orch_config_for_hydration`] →
/// [`TabManager::open_orchestration_tab_with_existing_role_panes`]). So a
/// dispatch opened mid-session appears as a real tab (e.g. `issue-work`) with no
/// reconnect, instead of only flat per-role dashboard cards.
///
/// A no-op unless the controller is the daemon-backed [`EmbeddedPaneController`]
/// — the only production controller, and the only one that can attach live
/// daemon PTYs (this path only ever fires for daemon-dispatched orchestrations).
fn process_pending_orchestration_surfaces(
    state: &SharedState,
    pane: &Arc<dyn PaneController>,
    tab_manager: &mut TabManager,
) {
    // S2: cheap READ-lock peek. The steady-state idle cost is one read lock +
    // is_empty — not the exclusive write lock the drain needs, taken every frame
    // just to find the queue empty.
    if state
        .blocking_read()
        .pending_orchestration_surfaces
        .is_empty()
    {
        return;
    }
    let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() else {
        // Non-embedded controller (tests / non-production): can't attach live
        // daemon PTYs. Drain and drop so the (capped) queue can't accumulate.
        let mut st = state.blocking_write();
        let count = st.pending_orchestration_surfaces.len();
        if count > 0 {
            st.pending_orchestration_surfaces.clear();
            tracing::debug!(
                count,
                "live orchestration surface: non-embedded controller; dropping queued surfaces"
            );
        }
        return;
    };
    // M2/S3: process at most ONE surface per frame. `surface_one_orchestration`
    // does a bounded `block_on(hydrate_pane)` per role (list 5s + attach 3s);
    // draining the whole queue in a single frame would compound that into a
    // multi-second UI freeze under a flood. One-per-frame caps the per-frame
    // cost at a single surface's bounded round-trips — the rest drain on the
    // following frames (the render loop ticks ~every 16ms while idle).
    let surface = {
        let mut st = state.blocking_write();
        if st.pending_orchestration_surfaces.is_empty() {
            return; // a concurrent path drained it between the peek and here
        }
        st.pending_orchestration_surfaces.remove(0)
    };
    surface_one_orchestration(state, embedded, tab_manager, surface);
}

/// Build one live orchestration tab from a daemon [`OrchestrationSurface`].
/// Idempotent on the role pane ids, so a duplicate broadcast (or a race with a
/// reconnect that already hydrated the tab) doesn't double-build.
fn surface_one_orchestration(
    state: &SharedState,
    embedded: &EmbeddedPaneController,
    tab_manager: &mut TabManager,
    surface: OrchestrationSurface,
) {
    // Idempotency: skip if an existing tab already owns any of this surface's
    // role panes (a duplicate broadcast, or reconnect-hydration that beat us to
    // it). Matching on the globally-unique role pane ids is more robust than a
    // (cwd, name) compare — an unnamed orchestration's surface `name` is the
    // resolved cwd-basename while the tab's `config.name` may be the raw
    // (possibly empty) config name, so they wouldn't compare equal.
    let already_built = surface
        .roles
        .iter()
        .any(|r| tab_manager.tab_index_for_pane(&r.pane_id).is_some());
    if already_built {
        tracing::debug!(
            cwd = %surface.cwd,
            orchestration = %surface.name,
            "live orchestration surface: tab already owns a role pane, skipping"
        );
        return;
    }

    // Reuse the hydration partition's config resolution: the local project
    // config when present, else a minimal config synthesised from the surface's
    // role metadata (same as a remote reconnect whose local config is absent).
    let bucket = OrchestrationHydrationBucket {
        cwd: surface.cwd.clone(),
        orchestration_name: surface.name.clone(),
        display_title: surface.display_title.clone(),
        role_slots: surface
            .roles
            .iter()
            .map(|r| OrchestrationRoleSlot {
                role_index: r.role_index,
                pane_id: r.pane_id.clone(),
                role_name: r.role_name.clone(),
                is_start_role: r.is_start_role,
            })
            .collect(),
    };
    let local = load_project_config(Path::new(&surface.cwd))
        .ok()
        .flatten()
        .and_then(|c| {
            c.orchestrations
                .into_iter()
                .find(|o| o.name == surface.name)
        });
    let orch_config = resolve_orch_config_for_hydration(local, &bucket);

    // Attach each role's live daemon PTY (by its DOT_AGENT_DECK_PANE_ID) and
    // place it in the role-slot vector, mirroring the reconnect partition.
    let mut role_pane_ids: Vec<Option<String>> = vec![None; orch_config.roles.len()];
    for role in &surface.roles {
        if role.role_index >= orch_config.roles.len() {
            tracing::error!(
                cwd = %surface.cwd,
                orchestration = %surface.name,
                role_index = role.role_index,
                role_count = orch_config.roles.len(),
                "live orchestration surface: role_index out of range; dropping role"
            );
            continue;
        }
        if role_pane_ids[role.role_index].is_some() {
            continue;
        }
        // `hydrate_pane` attaches the daemon agent carrying this pane id and
        // wires its PTY locally — the same attach + scrollback-replay path as
        // reconnect hydration, so the live agent renders in the tab.
        if embedded.hydrate_pane(&role.pane_id) {
            role_pane_ids[role.role_index] = Some(role.pane_id.clone());
        } else {
            tracing::debug!(
                cwd = %surface.cwd,
                orchestration = %surface.name,
                pane_id = %role.pane_id,
                "live orchestration surface: role pane attach failed; leaving slot as dead"
            );
        }
    }
    if role_pane_ids.iter().all(Option::is_none) {
        tracing::warn!(
            cwd = %surface.cwd,
            orchestration = %surface.name,
            "live orchestration surface: no role panes attached; not building tab"
        );
        return;
    }

    // Fill any missing slot with a synthetic dead-slot id so every role keeps a
    // card (parity with the reconnect path — a fresh spawn normally has none).
    let dead_slot_ids =
        assign_synthetic_dead_slot_ids(&mut role_pane_ids, &surface.cwd, &surface.name);

    // Build the tab WITHOUT yanking the user off their current tab: the open
    // call activates the new tab, so restore the prior active index afterward.
    // The tab bar shows whenever `tabs.len() > 1`, so the new label still paints
    // regardless of which tab is active.
    let prev_active = tab_manager.active_index();
    match tab_manager.open_orchestration_tab_with_existing_role_panes(
        &orch_config,
        &surface.cwd,
        role_pane_ids.clone(),
        bucket.display_title.as_deref(),
    ) {
        Ok(_) => {
            let mut st = state.blocking_write();
            // Seed placeholder cards for synthetic dead slots only (live roles
            // get their card from the agent's own SessionStart hook).
            for synthetic in &dead_slot_ids {
                st.insert_placeholder_session(
                    synthetic.clone(),
                    Some(surface.cwd.clone()),
                    None,
                    None,
                );
            }
            // Register live role panes + wire the per-pane maps exactly as the
            // reconnect-hydration path does (pane_role_map / pane_cwd_map /
            // orchestrator_pane_ids).
            for (i, role) in orch_config.roles.iter().enumerate() {
                if let Some(Some(pane_id)) = role_pane_ids.get(i)
                    && !is_dead_slot_pane_id(pane_id)
                {
                    st.register_pane(pane_id.clone());
                    st.pane_role_map.insert(pane_id.clone(), role.name.clone());
                    st.pane_cwd_map.insert(pane_id.clone(), surface.cwd.clone());
                    if role.start {
                        st.orchestrator_pane_ids.insert(pane_id.clone());
                    }
                }
            }
            drop(st);
            tab_manager.switch_to(prev_active);
            tracing::info!(
                cwd = %surface.cwd,
                orchestration = %surface.name,
                role_count = orch_config.roles.len(),
                "live orchestration surface: built tab for daemon-spawned orchestration"
            );
        }
        Err(e) => {
            tracing::error!(
                cwd = %surface.cwd,
                orchestration = %surface.name,
                error = %e,
                "live orchestration surface: failed to build tab; role panes stay on dashboard"
            );
        }
    }
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
    /// PRD #127: a seed prompt to deliver (gated, like a mode's `seed_prompt`)
    /// to the spawned single-agent CARD once its pane is ready. Set for the
    /// built-in "schedule" authoring session, which is a throwaway single-agent
    /// card (NOT a 50/50 mode tab), so its seed cannot ride on `mode_config` —
    /// that field routes the spawn through `render_mode_tab`. Carrying the seed
    /// here keeps the authoring session a dashboard card while still delivering
    /// the authoring prompt.
    seed_prompt: Option<String>,
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
    /// PRD #80 M8: click a new-pane-form field row — focus that field (== Tab /
    /// Shift+Tab landing on it). Typing stays keyboard.
    FormFocusField(FormField),
    /// PRD #80 M8: click a mode chip — select that mode option by index into
    /// the option list (0 = "No mode", 1.. = modes/orchestrations), == the
    /// Left/Right/h/l cycler landing on it.
    FormSelectMode(usize),
    /// PRD #80 M8: form `[Submit]` — spawn the pane from the form values
    /// (== Enter on the final field).
    FormSubmit,
    /// PRD #80 M8: form `[Cancel]` — close the form without spawning (== Esc).
    FormCancel,
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
    ///
    /// Boxed: `NewPaneRequest` is by far the largest payload of any `Action`
    /// variant (it owns a `ModeConfig`/`OrchestrationConfig`), so storing it
    /// inline would bloat every `Action` value (`clippy::large_enum_variant`).
    SpawnPane(Box<NewPaneRequest>),
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
    /// PRD #127 finding #4: open the "Scheduled Tasks" manager dialog. Shared by
    /// the dashboard `s`/`S` key and the `[Scheduled Tasks s]` button-bar button
    /// (PRD #80 parity), so both funnel through one dispatch path that loads the
    /// schedules and switches into [`UiMode::ScheduledTasks`].
    OpenScheduledTasks,
    /// PRD #127 finding #4: the manager dialog's `[Add]` button — mouse parity
    /// for the `a` key. Closes the dialog and spawns the seeded authoring agent
    /// with a blank context (same outcome as pressing `a`).
    ScheduleAdd,
    /// PRD #127 finding #4: the manager dialog's `[Edit]` button — mouse parity
    /// for the `e`/Enter key. Closes the dialog and spawns the authoring agent
    /// pre-filled with the selected row's values.
    ScheduleEdit,
    /// PRD #127 finding #4: the manager dialog's `[Delete]` button — mouse
    /// parity for the `d` key. Arms the definition-only delete confirmation for
    /// the selected row (the dialog stays open); confirming is then `y` / the
    /// confirmed [`Action::ScheduleDelete`].
    ScheduleArmDelete,
    /// PRD #127 M3.3: the manager dialog's `r` action — fire the named schedule
    /// immediately via the `RunNow` daemon control message. Dispatched in the
    /// main loop (socket I/O), which the sync key handler can't do directly.
    ScheduleRunNow(String),
    /// PRD #127 M3.3: the manager dialog's confirmed `d` action — remove the
    /// named schedule's DEFINITION (rewrite `schedules.toml` via the validated
    /// writer + daemon reload). Definition-only: it must NOT close an open tab
    /// for that schedule (the main-loop handler does no agent teardown).
    ScheduleDelete(String),
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
    // PRD #113 M3: a digit jump activates the highlight on the targeted card.
    ui.selected_index = Some(idx);
    if let Some((sid, _)) = filtered.get(idx)
        && let Some(session) = snapshot.sessions.get(*sid)
    {
        if let Some(ref pane_id) = session.pane_id {
            // PRD #127 finding #2: a card backed by a LIVE daemon agent but not
            // yet wired to a local pane (e.g. a scheduler-spawned agent that
            // surfaced via a `SessionStart` broadcast without going through
            // startup hydration) makes `focus_pane` report "pane not found".
            // That is NOT a stale card — attach the daemon's pane on demand and
            // retry before the delete arm below treats it as stale. Post-PRD #93
            // the daemon-backed `EmbeddedPaneController` is the only production
            // `PaneController` (the old in-process `LocalDeck` is gone), so the
            // downcast below always succeeds and the guard always operates on it.
            let mut focus_result = pane.focus_pane(pane_id);
            if let Err(PaneError::CommandFailed(_)) = focus_result
                && let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>()
                && embedded.hydrate_pane(pane_id)
            {
                focus_result = pane.focus_pane(pane_id);
            }
            match focus_result {
                Ok(()) => {
                    ui.mode = UiMode::PaneInput;
                    ui.status_message = Some((
                        format!(
                            "PaneInput mode — type to interact, {} for dashboard",
                            display_notation(&ui.keybindings, KbAction::Dashboard)
                        ),
                        std::time::Instant::now(),
                    ));
                    // PRD #84 M4: focusing a pane just updates focus state; the
                    // per-frame `resize_panes_to_layout` pass sizes the (now
                    // possibly expanded) pane to its inner area on the next
                    // frame. No inline resize here — the inline `crossterm::
                    // terminal::size()` + hardcoded `(term_w * 67 / 100)` /
                    // `saturating_sub(3)` math that used to live here diverged
                    // from the real layout and produced off-by-one-column PTYs.
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
    prev_selected_index: Option<usize>,
    ui: &UiState,
    filtered: &[(&String, &SessionState)],
    pane: &dyn PaneController,
) {
    if ui.selected_index != prev_selected_index
        && let Some(idx) = ui.selected_index
        && let Some((_, session)) = filtered.get(idx)
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
    kb: &KeybindingConfig,
) -> Action {
    let prev_selected_index = ui.selected_index;
    let result = handle_normal_key(key, ui, total, selected_status, kb);
    mirror_selection_into_focus(prev_selected_index, ui, filtered, pane);
    result
}

/// PRD #83 M3/M4 — reconcile the active tab's remembered card selection
/// with the currently focused pane and derive the card index to
/// highlight. Pure (no controller / no I/O) so it can be unit-tested and
/// reused by an L1 dashboard render test.
///
/// `filtered` is the tab's visible card list as `(session_id, pane_id)`
/// pairs, in render order. Behaviour by tab:
/// - **Dashboard**: if the focused pane maps to a visible card, adopt
///   that card's session id; then resolve `selected_session_id` to its
///   index (clearing it and returning `0` when it's no longer present).
/// - **Orchestration**: same, keyed by role pane id.
/// - **Mode**: returns `None` — mode tabs render via a separate path, so
///   `selected_index` must be left untouched.
///
/// The caller passes the active tab, so a pane focused while another tab
/// is active can never rewrite a different tab's selection — the gating
/// the PRD calls for.
pub fn sync_and_derive_selection(
    tab: &mut Tab,
    focused_pane_id: Option<&str>,
    filtered: &[(&str, Option<&str>)],
) -> Option<usize> {
    match tab {
        Tab::Dashboard {
            selected_session_id,
        } => {
            if let Some(fid) = focused_pane_id
                && let Some((sid, _)) = filtered.iter().find(|(_, pid)| *pid == Some(fid))
            {
                *selected_session_id = Some((*sid).to_string());
            }
            match selected_session_id
                .as_deref()
                .and_then(|sid| filtered.iter().position(|(id, _)| *id == sid))
            {
                Some(idx) => Some(idx),
                None => {
                    if selected_session_id.is_some() {
                        *selected_session_id = None;
                    }
                    Some(0)
                }
            }
        }
        Tab::Orchestration {
            role_pane_ids,
            focused_role_pane_id,
            ..
        } => {
            if let Some(fid) = focused_pane_id
                && role_pane_ids.iter().any(|p| p == fid)
            {
                *focused_role_pane_id = Some(fid.to_string());
            }
            match focused_role_pane_id
                .as_deref()
                .and_then(|pid| filtered.iter().position(|(_, p)| *p == Some(pid)))
            {
                Some(idx) => Some(idx),
                None => {
                    if focused_role_pane_id.is_some() {
                        *focused_role_pane_id = None;
                    }
                    Some(0)
                }
            }
        }
        Tab::Mode { .. } => None,
    }
}

/// PRD #113 revision (PR #151 review #2): identity of a *deck* — a tab that
/// carries a card selection (the Dashboard or an Orchestration). Used to key the
/// per-deck remembered Enter-restore selection so one deck can't leak its armed
/// index into another's restore. There is only ever one Dashboard, so it needs
/// no id; each Orchestration tab is keyed by its stable [`TabId`] so multiple
/// orchestrations each keep their own remembered role. `Mode` tabs are not decks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum DeckKey {
    Dashboard,
    Orchestration(TabId),
}

/// The [`DeckKey`] for a tab, or `None` for a non-deck (`Mode`) tab.
fn deck_key(tab: &Tab) -> Option<DeckKey> {
    match tab {
        Tab::Dashboard { .. } => Some(DeckKey::Dashboard),
        Tab::Orchestration { id, .. } => Some(DeckKey::Orchestration(*id)),
        Tab::Mode { .. } => None,
    }
}

/// PRD #113 M3 — resolve the card index a *focus* action (Enter / double-click)
/// should target on a deck (Dashboard or Orchestration — both route Enter here).
/// When the selection is active the highlighted card is used; with no cards
/// there is no target. Pure (no I/O), so it's unit-tested directly.
///
/// PRD #113 design revision (2026-06-13): when the deck is inactive (`None`,
/// e.g. just after returning from another tab) Enter RESTORES the last *active*
/// selection (`last_active_selection`) instead of jumping to card 0 — defaulting
/// to card 0 only before anything was ever selected. The index is clamped to the
/// current card count so a stale remembered index can't point past the list.
fn dashboard_focus_target(ui: &UiState, total: usize) -> Option<usize> {
    if total == 0 {
        return None;
    }
    Some(match ui.selected_index {
        Some(idx) => idx.min(total - 1),
        None => ui.last_active_selection.unwrap_or(0).min(total - 1),
    })
}

/// PRD #113 M2/M4 — reconcile the dashboard's *active* selection highlight with
/// the focused pane each frame, replacing the bare `sync_and_derive_selection`
/// call in the outer loop. The dashboard selection is now active/inactive
/// (`UiState.selected_index: Option<usize>`): an inactive selection (`None`,
/// set when the user switched tabs away) must stay inactive across frames so a
/// card never *looks* selected when nothing is armed — UNLESS a focused pane
/// *transitions* to a visible dashboard card, in which case the highlight
/// reactivates on that card (M4 focused-pane sync). When the selection is
/// already active, or for the Orchestration/Mode tabs (whose selection is
/// separate, always-on state), the pre-existing `sync_and_derive_selection`
/// derive is preserved.
///
/// PR #151 (manual-test fix): M4 reactivates only on a genuine focus
/// TRANSITION — the focused pane id must have *changed* since the previous
/// frame. The original code reactivated whenever the focused pane merely
/// *mapped* to a card, which re-armed a just-cleared selection on tab return:
/// a Mode tab's agent pane is a dashboard card (only its side panes are in
/// `all_managed_pane_ids`, so the agent pane isn't filtered out), and switching
/// to a Mode tab focuses that agent pane while the return to the Dashboard
/// restores nothing — so the agent pane stays focused. That steady-state focus
/// is not a transition, so it no longer reactivates the highlight. The cyan
/// focus border (driven by the controller's focus, not this function) is
/// unaffected. Orchestration role panes are already excluded from the card
/// list, which is why that path never exhibited the bug.
fn reconcile_dashboard_selection(
    ui: &mut UiState,
    tab: &mut Tab,
    focused_pane_id: Option<&str>,
    filtered: &[(&str, Option<&str>)],
) {
    // A genuine focus transition = the focused pane id changed since last frame.
    // Record the current focus first so every frame (any tab) keeps the
    // baseline fresh — otherwise a stale baseline would read a restored
    // steady-state focus as a transition on the next dashboard frame.
    let focus_changed = focused_pane_id != ui.last_focused_pane_id.as_deref();
    ui.last_focused_pane_id = focused_pane_id.map(str::to_string);

    // On a deck (Dashboard OR Orchestration — PRD #113 revision Change 1 makes
    // the clearing symmetric), only reactivate an inactive selection when a
    // focused pane TRANSITIONS to a visible card; otherwise leave it inactive so
    // a tab-switch deactivation isn't undone by the per-frame sync (the restored
    // steady-state focus is not a transition). The guard expression itself is
    // unchanged from PR #151.
    if let Tab::Dashboard { .. } | Tab::Orchestration { .. } = tab {
        let focus_maps_to_card = focused_pane_id
            .map(|fid| filtered.iter().any(|(_, pid)| *pid == Some(fid)))
            .unwrap_or(false);
        let focus_reactivates = focus_maps_to_card && focus_changed;
        if ui.selected_index.is_none() && !focus_reactivates {
            return;
        }
    }
    if let Some(idx) = sync_and_derive_selection(tab, focused_pane_id, filtered) {
        ui.selected_index = Some(idx);
    }
}

/// PRD #83 M2 — switch to `target_index` while preserving per-tab focus.
/// Captures the source tab's focused pane on the way out, switches, then
/// restores the destination tab's remembered focus on the way in. The
/// Dashboard's selection is keyed by session id (not a pane id), so its
/// restore is handled here using the live `snapshot` that `TabManager`
/// doesn't carry: the remembered card's pane is re-focused so the
/// per-frame focused-pane→selection sync can't snap the dashboard onto a
/// pane left focused by another tab. Returns whether the switch happened.
///
/// PRD #113 (UNIFIED deck behavior): leaving a deck (Dashboard OR Orchestration)
/// deactivates its selection highlight (`ui.selected_index = None`) but KEEPS the
/// deck's remembered selection, so on return the deck re-focuses its remembered
/// pane and shows the same region — the Dashboard via the `selected_session_id`
/// re-focus block below, the Orchestration via `restore_focus_on_switch_in`. The
/// highlight stays inactive because `reconcile_dashboard_selection` only
/// reactivates on a focus *transition* and the pre-seed below makes the restored
/// focus a steady state (no transition): a card never *looks* selected until the
/// user re-arms it with Enter. (A Mode tab's agent pane stays focused on return
/// and *is* a dashboard card, but the same transition guard keeps it from
/// re-arming — see PR #151.)
fn switch_tab_with_focus(
    tab_manager: &mut TabManager,
    target_index: usize,
    pane: &dyn PaneController,
    snapshot: &AppState,
    ui: &mut UiState,
) -> bool {
    // Only treat this as "leaving the Dashboard" when the switch will actually
    // move to a different tab (`switch_to` returns `true` even for the current
    // index), so a no-op cycle on a single-tab layout can't spuriously clear
    // the highlight.
    //
    // PR #151 (Greptile P2): the leave-clear below mutates the OUTGOING
    // Dashboard, so it must run while that tab is still active — i.e. before
    // `switch_to`. That is safe here because `will_move` already guarantees the
    // switch will succeed: `switch_to(i)` returns `true` *iff* `i < tabs.len()`
    // (its only guard — see `TabManager::switch_to`), and `will_move` requires
    // `target_index < tab_count()` (== `tabs.len()`) plus a real index change.
    // Nothing between here and `switch_to` adds/removes tabs (`active_tab_mut`
    // and `capture_focus_on_switch_out` only touch per-tab fields), so
    // `tabs.len()` is invariant. Hence `will_move == true` ⇒ `switched == true`:
    // the clear can never deactivate the selection without an actual transition.
    let will_move =
        target_index < tab_manager.tab_count() && target_index != tab_manager.active_index();
    // PRD #113 (UNIFIED deck behavior): leaving a deck (Dashboard OR
    // Orchestration) runs ONE shared path — deactivate the live highlight
    // (`selected_index = None`) but KEEP the deck's own remembered selection so
    // the return leg can re-focus it. The two decks remember different keys
    // (Dashboard: `selected_session_id`; Orchestration: `focused_role_pane_id`),
    // but both are now treated the SAME: left intact on leave. The former
    // Dashboard-only `selected_session_id = None` band-aid is gone — it existed
    // to stop the switch-in re-focus block from re-arming the highlight, but the
    // focus-transition guard (`reconcile_dashboard_selection`) + the focus-baseline
    // pre-seed below now keep the highlight inactive without forgetting the card,
    // so the Dashboard restores its remembered region on return exactly like
    // Orchestration restores its role pane. An ACTIVE highlight is recorded into
    // THIS deck's OWN per-deck slot (keyed by deck identity, not a shared field,
    // so it can't leak into another deck's restore — orchestration_005) so Enter
    // can restore it; only a `Some` is stored, since an inactive leave (e.g. a
    // round-trip's return leg) must NOT clobber the remembered value.
    if will_move && let Some(key) = deck_key(tab_manager.active_tab()) {
        if let Some(idx) = ui.selected_index {
            ui.last_active_selection_by_deck.insert(key, idx);
        }
        ui.selected_index = None;
    }
    tab_manager.capture_focus_on_switch_out();
    let switched = tab_manager.switch_to(target_index);
    if switched {
        // The pane focus is restored to on switch-in becomes the new focus
        // baseline (see the PR #151 pre-seed below).
        let mut restored_focus = tab_manager.restore_focus_on_switch_in();
        // PRD #113 finding 1 + revision Change 1: entering a deck (Dashboard OR
        // Orchestration) via a real tab move starts the selection INACTIVE —
        // symmetric to the leave-deactivation above and across both decks. The
        // shared `selected_index` can be re-armed by a deck's always-active
        // reconcile while in transit, so without this a deck→deck→deck round-trip
        // would leave a stale highlight (violating SC1). The per-frame
        // `reconcile_dashboard_selection` reactivates only on a genuine focus
        // transition (M4 / selection_009), so legitimate reactivation is preserved.
        if will_move && let Some(key) = deck_key(tab_manager.active_tab()) {
            ui.selected_index = None;
            // PR #151 review #2: hoist the INCOMING deck's own remembered
            // selection into the field `dashboard_focus_target` reads, so Enter
            // restores THIS deck's prior selection — not whatever another deck
            // last armed. `None` when this deck has nothing remembered yet
            // (Enter then defaults to card 0, unchanged).
            ui.last_active_selection = ui.last_active_selection_by_deck.get(&key).copied();
        }
        // Dashboard analog of the Orchestration role-pane restore: the Dashboard's
        // selection is keyed by SESSION id (not a pane id) and `TabManager` carries
        // no session→pane map, so `restore_focus_on_switch_in` can't resolve it —
        // this block does, using the live `snapshot`. It re-focuses the remembered
        // card's pane (kept on leave, like Orchestration keeps `focused_role_pane_id`)
        // so the Dashboard returns showing the same region, and becomes the focus
        // baseline below. Both decks thus run the SAME return shape: re-focus the
        // remembered pane → feed `restored_focus` → pre-seed the baseline.
        if let Tab::Dashboard {
            selected_session_id,
        } = tab_manager.active_tab()
            && let Some(sid) = selected_session_id
            && let Some(session) = snapshot.sessions.get(sid)
            && let Some(pane_id) = session.pane_id.as_ref()
        {
            let _ = pane.focus_pane(pane_id);
            // The re-focus block is the final word on what's focused after the
            // switch, so it (not `restore_focus_on_switch_in`) is the baseline.
            restored_focus = Some(pane_id.clone());
        }
        // PR #151 orch→orch fix: pre-seed the focus baseline to the pane the
        // switch RESTORED focus to, so the next `reconcile_dashboard_selection`
        // frame computes `focus_changed == false` and the inactive-stays-inactive
        // guard early-returns — keeping `selected_index` None. This generalizes
        // the steady-state fix (selection_013, where the same pane stays focused
        // on return) to a tab switch that restores a DIFFERENT pane (orch A →
        // orch B, where each orchestration tab restores its own role): without
        // it, the restored B role reads as a focus transition and re-arms the
        // highlight one frame after the switch cleared it. Only overwrite when
        // the switch actually restored a focus (`Some`); a None restore (e.g. a
        // Dashboard return with no remembered card, which re-focuses nothing)
        // leaves the prior baseline intact, matching the controller's still-focused
        // pane. This does NOT
        // suppress a genuine in-deck focus change: a later frame (no tab switch)
        // that focuses a different pane still differs from this baseline and
        // reactivates (M4 / selection_009 / selection_014).
        if will_move && let Some(restored) = restored_focus {
            ui.last_focused_pane_id = Some(restored);
        }
    }
    switched
}

fn handle_normal_key(
    key: KeyEvent,
    ui: &mut UiState,
    total: usize,
    selected_status: Option<SessionStatus>,
    kb: &KeybindingConfig,
) -> Action {
    // Ctrl+C from dashboard: show quit confirmation. PRD #40 safety net —
    // hardcoded and checked first so it can never be remapped or unbound.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
    }

    // PRD #40: dashboard (command-mode) shortcuts resolve from the active
    // keybinding config. `kb` is the single per-keypress snapshot taken by the
    // caller (passed by reference so we don't re-clone — it exists to avoid a
    // `&ui` borrow conflict with the `&mut ui` mutations below). PRD #80: the
    // mode-changing shortcuts return shared `Action`s (EnterFilter/ToggleHelp/
    // EnterRename/Focus) so a keypress and the button-bar funnel into the same
    // `dispatch_action` path. The non-configurable arrow-key aliases (Down/Up)
    // are kept alongside move_down/move_up so default nav is byte-for-byte
    // unchanged.
    if kb.matches(KbAction::MoveDown, &key) || key.code == KeyCode::Down {
        if total > 0 {
            // PRD #113 M3: from an inactive selection `j`/Down jumps to the
            // first card and activates; once active it advances with wrap.
            ui.selected_index = Some(match ui.selected_index {
                None => 0,
                Some(i) => (i + 1) % total,
            });
        }
        return Action::Continue;
    }
    if kb.matches(KbAction::MoveUp, &key) || key.code == KeyCode::Up {
        if total > 0 {
            // PRD #113 M3: from an inactive selection `k`/Up jumps to the last
            // card and activates; once active it retreats with wrap.
            ui.selected_index = Some(match ui.selected_index {
                None => total - 1,
                Some(i) => (i + total - 1) % total,
            });
        }
        return Action::Continue;
    }
    // move_left / move_right (defaults h / l) are handled in the main loop for
    // tab switching.
    if kb.matches(KbAction::Filter, &key) {
        return Action::EnterFilter;
    }
    if kb.matches(KbAction::Help, &key) {
        return Action::ToggleHelp;
    }
    if kb.matches(KbAction::Rename, &key) && total > 0 {
        return Action::EnterRename;
    }
    if kb.matches(KbAction::FocusPane, &key) && total > 0 {
        return Action::Focus;
    }
    // generate_config (default `g`) — a first-class remappable action so it
    // participates in conflict detection and renders via display_notation.
    if kb.matches(KbAction::GenerateConfig, &key) && total > 0 {
        return Action::RequestConfigGen;
    }
    // PRD #92 F2 (PRD #18 follow-through): approve / deny permission. Only
    // fires when the selected card is in `WaitingForInput` — any other status,
    // or no card selected, no-ops silently. The bindings (defaults y / n)
    // carry their own modifier requirement via `matches`, so Ctrl+n (new
    // pane, handled in the outer dispatch loop) still wins.
    if kb.matches(KbAction::ApprovePermission, &key)
        && total > 0
        && selected_status == Some(SessionStatus::WaitingForInput)
    {
        return Action::SendPermissionResponse(true);
    }
    if kb.matches(KbAction::DenyPermission, &key)
        && total > 0
        && selected_status == Some(SessionStatus::WaitingForInput)
    {
        return Action::SendPermissionResponse(false);
    }
    // PRD #127 finding #4: open the "Scheduled Tasks" manager dialog. Routed
    // through the keybinding registry (default `s`) so it is remappable and
    // shares one dispatch path with the `[Scheduled Tasks s]` button-bar button
    // (PRD #80 parity). The legacy uppercase `S` (Shift+s) stays a hardcoded
    // non-configurable alias — mirroring the Down/Up aliases kept beside j/k —
    // so existing muscle memory keeps working while lowercase `s` is added.
    if kb.matches(KbAction::OpenScheduledTasks, &key) || key.code == KeyCode::Char('S') {
        return Action::OpenScheduledTasks;
    }
    if kb.matches(KbAction::ClearFilter, &key) {
        if !ui.filter_text.is_empty() {
            ui.filter_text.clear();
        }
        return Action::Continue;
    }
    Action::Continue
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

/// PRD #127 M3.3: key handling for the "Scheduled Tasks" manager dialog.
/// Read-only-plus-actions: `j`/`k` move the selection, `a` adds, `Enter`/`e`
/// edits the selected row (both spawn the seeded authoring agent), `d` asks to
/// confirm a definition-only delete (`y` confirms, `n`/Esc cancels), `r`
/// run-now fires the selected row. NO inline enable/disable toggle and NO
/// in-place field editing (PRD decision).
fn handle_scheduled_tasks_key(key: KeyEvent, ui: &mut UiState) -> Action {
    // Delete-confirmation sub-state takes precedence.
    if ui.scheduled_delete_confirm {
        match key.code {
            KeyCode::Char('y') => {
                ui.scheduled_delete_confirm = false;
                if let Some(task) = ui.scheduled_tasks.get(ui.scheduled_selected) {
                    let name = task.name.clone();
                    // PRD #127 N2: keep the dialog OPEN after a confirmed
                    // delete — the main-loop handler removes the definition and
                    // refreshes the list in place so the user can act on more
                    // rows.
                    return Action::ScheduleDelete(name);
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                ui.scheduled_delete_confirm = false;
            }
            _ => {}
        }
        return Action::Continue;
    }

    let len = ui.scheduled_tasks.len();
    match key.code {
        // PRD #127 finding #4: `s`/`S` toggle the dialog closed, mirroring the
        // case-insensitive open shortcut, alongside the usual Esc / `q`.
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('S') | KeyCode::Char('s') => {
            ui.mode = UiMode::Normal;
            Action::Continue
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if len > 0 {
                ui.scheduled_selected = (ui.scheduled_selected + 1) % len;
            }
            Action::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if len > 0 {
                ui.scheduled_selected = (ui.scheduled_selected + len - 1) % len;
            }
            Action::Continue
        }
        // Add: PRD #170 (unify) — reuse the `Ctrl+n` flow. Open the directory
        // picker (at the cwd) marked `ScheduleAdd`; confirming a dir builds the
        // mode-locked ` New Schedule ` form, which spawns the seeded authoring
        // agent on submit (blank context).
        KeyCode::Char('a') => {
            open_schedule_dir_picker(ui, None);
            Action::Continue
        }
        // Edit the selected row: PRD #170 (unify) — open the directory picker
        // STARTING at the row's `working_dir`, marked `ScheduleEdit(row)` so the
        // mode-locked ` Edit Schedule ` form pre-fills the authoring seed from
        // the row (it calls `schedule update`). With no rows, behaves like add.
        KeyCode::Enter | KeyCode::Char('e') => {
            let existing = ui.scheduled_tasks.get(ui.scheduled_selected).cloned();
            open_schedule_dir_picker(ui, existing);
            Action::Continue
        }
        // Delete (definition only) — ask to confirm first.
        KeyCode::Char('d') => {
            if ui.scheduled_selected < len {
                ui.scheduled_delete_confirm = true;
            }
            Action::Continue
        }
        // Run-now the selected row. PRD #127 N2: the dialog stays OPEN so the
        // user can fire several rows; the main-loop handler refreshes status.
        KeyCode::Char('r') => {
            if let Some(task) = ui.scheduled_tasks.get(ui.scheduled_selected) {
                return Action::ScheduleRunNow(task.name.clone());
            }
            Action::Continue
        }
        _ => Action::Continue,
    }
}

/// PRD #170 (unify): open the directory picker for a manager Add (`existing =
/// None`) or Edit (`existing = Some(row)`), reusing the `Ctrl+n` flow instead
/// of a bespoke pick-agent modal. Add starts the picker at the cwd; Edit starts
/// it at the row's `working_dir` and carries the row so the mode-locked form
/// pre-fills the authoring seed. The picked directory then drives
/// [`transition_after_dir_pick`] (branching on the `dir_picker_intent` set
/// here) to build the schedule-locked form. Shared by the `a`/`e`/Enter keys
/// and the `[Add]`/`[Edit]` button actions so all four follow one path.
fn open_schedule_dir_picker(ui: &mut UiState, existing: Option<config::ScheduledTask>) {
    let (intent, start) = match existing {
        Some(row) => {
            let start = PathBuf::from(&row.working_dir);
            (DirPickerIntent::ScheduleEdit(Box::new(row)), start)
        }
        None => (
            DirPickerIntent::ScheduleAdd,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        ),
    };
    ui.dir_picker_intent = intent;
    ui.dir_picker = Some(DirPickerState::new(start));
    ui.mode = UiMode::DirPicker;
}

/// PRD #170 round 2 (reviewer finding 5): re-render the Scheduled-Tasks manager
/// after cancelling a manager-originated schedule flow. The `scheduled_tasks`
/// list is still loaded from the original `OpenScheduledTasks`, so this only
/// clamps the selection back into range (defensive — nothing removed a row) and
/// switches mode so the dialog (its `NEXT FIRE` header) re-renders.
fn return_to_scheduled_tasks_manager(ui: &mut UiState) {
    if ui.scheduled_selected >= ui.scheduled_tasks.len() {
        ui.scheduled_selected = ui.scheduled_tasks.len().saturating_sub(1);
    }
    ui.scheduled_delete_confirm = false;
    ui.mode = UiMode::ScheduledTasks;
}

/// PRD #170 round 2 (reviewer findings 4 & 5): cancel the directory picker.
/// Routes by the picker's `dir_picker_intent`: a manager-originated pick
/// (`ScheduleAdd`/`ScheduleEdit`) returns to the Scheduled-Tasks manager, while
/// an ordinary `Ctrl+n` pick (`NewPane`) drops to the dashboard (unchanged).
/// Either way the intent is CONSUMED (reset to `NewPane`) so a later `Ctrl+n` is
/// never poisoned by a stale schedule intent. Shared by the picker's Esc/`q`
/// keys and the `[Cancel]` button so all cancel doors behave identically.
fn cancel_dir_picker(ui: &mut UiState) {
    ui.dir_picker = None;
    let intent = std::mem::replace(&mut ui.dir_picker_intent, DirPickerIntent::NewPane);
    match intent {
        DirPickerIntent::ScheduleAdd | DirPickerIntent::ScheduleEdit(_) => {
            return_to_scheduled_tasks_manager(ui);
        }
        DirPickerIntent::NewPane => ui.mode = UiMode::Normal,
    }
}

/// PRD #170 round 2 (reviewer finding 5): cancel the new-pane form. By the time
/// the form is up, [`transition_after_dir_pick`] has already consumed the
/// `dir_picker_intent`, so route on the form's own `schedule_locked` flag: a
/// mode-locked schedule form (manager Add/Edit) returns to the Scheduled-Tasks
/// manager, while the ordinary `Ctrl+n` form drops to the dashboard (unchanged).
/// Shared by the form's Esc key and the `[Cancel]` button.
fn cancel_new_pane_form(ui: &mut UiState) {
    let locked = ui.new_pane_form.as_ref().is_some_and(|f| f.schedule_locked);
    ui.new_pane_form = None;
    if locked {
        return_to_scheduled_tasks_manager(ui);
    } else {
        ui.mode = UiMode::Normal;
    }
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
                    cancel_dir_picker(ui);
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
                cancel_dir_picker(ui);
            }
        }
        KeyCode::Char('q') => {
            cancel_dir_picker(ui);
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

/// Build the form that follows a confirmed directory pick. For an ordinary
/// `Ctrl+n` pick (`DirPickerIntent::NewPane`): check for `.dot-agent-deck.toml`
/// in the selected directory and open the unified new-pane form, with the Mode
/// field when modes are available. For a manager Add/Edit pick
/// (`ScheduleAdd`/`ScheduleEdit`, PRD #170 unify): open the SAME form
/// MODE-LOCKED to schedule authoring, with the Command pre-filled from the
/// resolved `default_command` and (on Edit) the row carried for the seed
/// pre-fill. The intent is consumed (reset to `NewPane`) so a later `Ctrl+n`
/// isn't poisoned.
fn transition_after_dir_pick(ui: &mut UiState) {
    let dir = ui
        .dir_picker
        .as_ref()
        .map(|p| p.current_dir.clone())
        .unwrap_or_default();

    ui.dir_picker = None;
    // Consume the intent so a later `Ctrl+n` (which only sets `NewPane`) is
    // never poisoned by a stale schedule intent.
    let intent = std::mem::replace(&mut ui.dir_picker_intent, DirPickerIntent::NewPane);

    let form = match intent {
        DirPickerIntent::NewPane => {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let command = ui.config.default_command.clone();
            let (modes, orchestrations) = match load_project_config(&dir) {
                Ok(Some(config)) => (config.modes, config.orchestrations),
                _ => (vec![], vec![]),
            };
            NewPaneFormState::new(dir, name, command, modes, orchestrations)
        }
        // PRD #170: the picked dir is pre-seeded as the schedule's working_dir;
        // the Command field pre-fills from the resolved authoring command so a
        // blank `default_command` shows the `claude` fallback rather than blank.
        DirPickerIntent::ScheduleAdd => {
            let command = resolve_authoring_command(&ui.config.default_command);
            NewPaneFormState::new_schedule_locked(dir, command, None)
        }
        DirPickerIntent::ScheduleEdit(existing) => {
            let command = resolve_authoring_command(&ui.config.default_command);
            NewPaneFormState::new_schedule_locked(dir, command, Some(*existing))
        }
    };

    ui.new_pane_form = Some(form);
    ui.mode = UiMode::NewPaneForm;
}

/// PRD #80 M8: build the [`NewPaneRequest`] from the current form values.
/// Shared by the Enter-submit key arm and the `[Submit]` button
/// ([`Action::FormSubmit`]) so click and key spawn an identical pane.
fn build_new_pane_request(form: &NewPaneFormState, default_command: &str) -> NewPaneRequest {
    // PRD #120: the flag-gated "schedule: issues" authoring option — like the
    // plain "schedule" option it is a throwaway single-agent authoring CARD, but
    // its seed authors an ISSUE-DISPATCH task (`schedule add --repo …`) instead
    // of a single-spawn one. Spawn it as a dashboard card carrying the
    // issue-dispatch seed; `selected_mode()` still returns its synthetic mode for
    // the cycler title ("schedule: issues mode").
    if form.is_issue_dispatch_selected() {
        let command = if form.command.trim().is_empty() {
            resolve_authoring_command(default_command)
        } else {
            form.command.clone()
        };
        return NewPaneRequest {
            dir: form.dir.clone(),
            name: form.name.clone(),
            command,
            mode_config: None,
            orchestration_config: None,
            seed_prompt: Some(build_issue_dispatch_authoring_seed(&form.dir)),
        };
    }
    // PRD #127: the built-in "schedule" authoring option is NOT a workload mode
    // tab — it is a throwaway single-agent authoring CARD that converses to
    // build a schedule entry. Spawn it as a dashboard card (`mode_config` None)
    // carrying the authoring seed prompt, so it routes to the dashboard like any
    // single-agent card instead of through `render_mode_tab`'s 50/50 split. The
    // form's `selected_mode()` still returns the synthetic mode for the Mode
    // cycler's title/separator rendering — only the spawned request differs.
    if form.is_schedule_selected() {
        // PRD #170 M2.1: the authoring agent must be a real conversational agent
        // (it has to act on the seed prompt and call the `schedule add` CLI), so
        // default a blank command to the configured `default_command` (the same
        // value the form opens pre-filled with) rather than spawning a bare
        // $SHELL that can't author anything — replacing the former hardcoded
        // `claude`. Applied HERE — not only in the Enter arm — so BOTH submit
        // doors (Enter on the final field AND the [Submit] button, which calls
        // this directly) apply the default.
        //
        // PRD #170 round 2 (reviewer finding 1): `default_command` itself can be
        // blank/whitespace for an unconfigured user, so a SECOND-level fallback
        // resolves it to `claude` — `resolve_authoring_command` never returns a
        // blank, so the schedule authoring agent is always a real agent.
        let command = if form.command.trim().is_empty() {
            resolve_authoring_command(default_command)
        } else {
            form.command.clone()
        };
        // PRD #170 (unify): derive the seed from `build_schedule_authoring_mode`
        // threaded with the picked dir (and, on a manager Edit, the existing
        // row) so the seed carries the working_dir DEFAULT and — for Edit — the
        // row's current values pre-fill. For an unlocked `Ctrl+n` schedule
        // selection `schedule_existing` is `None`, so this is the base seed plus
        // the working_dir line.
        return NewPaneRequest {
            dir: form.dir.clone(),
            name: form.name.clone(),
            command,
            mode_config: None,
            orchestration_config: None,
            seed_prompt: build_schedule_authoring_mode(form.schedule_existing.as_ref(), &form.dir)
                .seed_prompt,
        };
    }
    NewPaneRequest {
        dir: form.dir.clone(),
        name: form.name.clone(),
        command: form.command.clone(),
        mode_config: form.selected_mode().cloned(),
        orchestration_config: form.selected_orchestration().cloned(),
        seed_prompt: None,
    }
}

fn handle_new_pane_form_key(key: KeyEvent, ui: &mut UiState) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
    }
    // PRD #170 M2.1: snapshot the configured default command before the mutable
    // `form` borrow below so the blank-command authoring default resolves to it.
    let default_command = ui.config.default_command.clone();
    let form = match ui.new_pane_form.as_mut() {
        Some(f) => f,
        None => {
            ui.mode = UiMode::Normal;
            return Action::Continue;
        }
    };
    match key.code {
        // PRD #170 finding 5: a locked schedule form returns to the manager; the
        // ordinary `Ctrl+n` form drops to the dashboard (unchanged).
        KeyCode::Esc => {
            cancel_new_pane_form(ui);
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
                // The blank-command -> `default_command` authoring default now
                // lives in `build_new_pane_request`, so both this Enter door and
                // the [Submit] button door apply it identically.
                let req = build_new_pane_request(form, &default_command);
                ui.new_pane_form = None;
                ui.mode = UiMode::Normal;
                return Action::SpawnPane(Box::new(req));
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
pub fn global_action(kb: &KeybindingConfig, key: &KeyEvent) -> Option<Action> {
    // PRD #40: the four configurable global commands resolve from the active
    // keybinding config (any chord, any mode), defaulting to Ctrl+d/t/n/w. The
    // caller excludes Ctrl+C before calling this, so a binding to Ctrl+C can't
    // win here.
    if kb.matches(KbAction::Dashboard, key) {
        return Some(Action::DetachToNormal);
    }
    if kb.matches(KbAction::ToggleLayout, key) {
        return Some(Action::ToggleLayout);
    }
    if kb.matches(KbAction::NewPane, key) {
        return Some(Action::NewPane);
    }
    if kb.matches(KbAction::ClosePane, key) {
        return Some(Action::CloseSelected);
    }
    // Ctrl+PageDown / Ctrl+PageUp: non-configurable tab navigation.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::PageDown => return Some(Action::GlobalNextTab),
            KeyCode::PageUp => return Some(Action::GlobalPrevTab),
            _ => {}
        }
    }
    None
}

/// PRD #80 / #40: map a Normal-mode tab-cycling key to its [`Action`]. The
/// configurable move_right/move_left actions (defaults `l`/`h`) drive this,
/// alongside the non-configurable Tab / Shift+Tab / Left / Right aliases.
/// Returns `None` for anything else.
fn cycle_tab_action(kb: &KeybindingConfig, key: &KeyEvent) -> Option<Action> {
    if kb.matches(KbAction::MoveRight, key) || matches!(key.code, KeyCode::Tab | KeyCode::Right) {
        return Some(Action::CycleTabNext);
    }
    if kb.matches(KbAction::MoveLeft, key) || matches!(key.code, KeyCode::BackTab | KeyCode::Left) {
        return Some(Action::CycleTabPrev);
    }
    None
}

/// PRD #80 / #40: map an in-tab navigation key on a mode tab to its [`Action`].
/// The configurable move_down/move_up actions (defaults `j`/`k`) drive
/// selection, with the Down/Up arrows kept as non-configurable aliases;
/// Enter/Esc are mode-fixed. The caller only invokes this when the active tab
/// is a `Tab::Mode`, so the returned action is always meaningful there.
fn mode_tab_nav_action(kb: &KeybindingConfig, key: &KeyEvent) -> Option<Action> {
    if kb.matches(KbAction::MoveDown, key) || key.code == KeyCode::Down {
        return Some(Action::ModeTabSelectNext);
    }
    if kb.matches(KbAction::MoveUp, key) || key.code == KeyCode::Up {
        return Some(Action::ModeTabSelectPrev);
    }
    match key.code {
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
    /// dimmed and is INERT: the bar renderers do not record its rect, so a
    /// click on it is a no-op — matching the keyboard, where the bound key is
    /// a silent no-op (PRD #80 review FIX 3).
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
    /// a disabled one. PRD #13: terminal-relative — the bracketed `[Label
    /// Shortcut]` is self-identifying, so the button paints no absolute
    /// background; an enabled button uses the terminal's own foreground and a
    /// disabled one dims it.
    fn style(&self) -> Style {
        if self.enabled {
            text_primary()
        } else {
            text_dim()
        }
    }

    /// Render `text` (an already-chosen full or shortcut-only label) into `buf`
    /// at `area` and return the `(Action, Rect)` pair to record in
    /// `UiState::button_rects`. Keeping render and the recorded rect in one
    /// call is what stops the click target from drifting from what's drawn.
    fn render_text(&self, text: &str, area: Rect, buf: &mut Buffer) -> (Action, Rect) {
        let span = Span::styled(text.to_string(), self.style());
        buf.set_span(area.x, area.y, &span, area.width);
        self.pair(area)
    }

    /// Render the full `[Label Shortcut]` button into `buf` at `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> (Action, Rect) {
        self.render_text(&self.display_label(), area, buf)
    }

    /// Render the narrow-terminal `[Shortcut]` fallback into `buf` at `area`.
    pub fn render_compact(&self, area: Rect, buf: &mut Buffer) -> (Action, Rect) {
        self.render_text(&self.shortcut_only_label(), area, buf)
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
            // PRD #84 M4: the pre-draw `resize_panes_to_layout` sizes the
            // remaining panes on the next frame — no resize pushed here.
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
    if let Some(idx) = ui.selected_index
        && idx > 0
    {
        ui.selected_index = Some(idx - 1);
    }
    // PRD #89 M1.2 — closing a mode/orchestration tab (via `[×]` click or the
    // CloseTab action) is a meaningful state change; keep the snapshot fresh.
    ui.mark_session_dirty();
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
) {
    let prompt = crate::config_gen::config_gen_prompt(cwd);
    match pane.write_to_pane(pane_id, &prompt) {
        Ok(()) => {
            // Focus the pane so the user can press Enter to execute.
            if let Some(tab_idx) = tab_manager.tab_index_for_pane(pane_id) {
                // PRD #83: snapshot the SOURCE tab's focus before leaving it,
                // mirroring `switch_tab_with_focus` (capture-out → switch_to →
                // record). Capture is a no-op when the source is the Dashboard
                // (its selection is session-id keyed), but adding it keeps every
                // switch site uniform. `capture_focus_on_switch_out` only reads
                // the controller's focused pane id — no `focus_pane` — so the
                // destination focus below is unaffected.
                tab_manager.capture_focus_on_switch_out();
                tab_manager.switch_to(tab_idx);
                tab_manager.record_focus(pane_id);
                // PRD #84 M4: the destination tab's panes are sized by the
                // pre-draw `resize_panes_to_layout` on the next frame.
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
            // PRD #170: mark this pick as an ordinary new-pane open (not a
            // schedule Add/Edit) so a prior schedule intent can't leak.
            ui.dir_picker_intent = DirPickerIntent::NewPane;
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
            // PRD #84 M4: toggling the layout just flips `ui.pane_layout`; the
            // pre-draw `resize_panes_to_layout` re-sizes every pane to the new
            // split on the next frame (it reads `pane_layout`).
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
            // PR #151 (e2e layout_002 regression): branch on the ACTIVE tab.
            // Ctrl+W routes here for BOTH "close the selected dashboard card" and
            // "close the active Mode/Orchestration tab". The dashboard-card close
            // below no-ops on an inactive selection (finding 2 / selection_012),
            // but on a Mode/Orchestration tab the dashboard selection is `None`,
            // so that gate wrongly suppressed the tab-close. When the active tab
            // IS a closable tab, close it directly — regardless of
            // `ui.selected_index` — mirroring the mouse [×] path
            // (`close_tab_by_index`) so the keyboard closes the same tab the same
            // way. (selection_016)
            let active_is_closable_tab = matches!(
                tab_manager.active_tab(),
                Tab::Mode { .. } | Tab::Orchestration { .. }
            );
            if active_is_closable_tab {
                close_tab_by_index(tab_manager.active_index(), ui, state, tab_manager);
            }
            // PRD #113 finding 2: on the Dashboard the destructive close requires
            // a REAL active selection. When the selection is inactive (`None`)
            // this is a no-op — no card-0 fallback (that fallback is reserved for
            // Enter/Focus) — so an unarmed dashboard can never silently close
            // card 0.
            else if let Some(sid) = ui
                .selected_index
                .and_then(|i| filtered.get(i))
                .map(|(id, _)| (*id).clone())
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
                            // PRD #84 M4: remaining panes are sized by the
                            // pre-draw `resize_panes_to_layout` next frame.
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
                                    "Failed to close pane {closed_pane_id}: {e} — press {} to retry",
                                    display_notation(&ui.keybindings, KbAction::ClosePane)
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
                if let Some(idx) = ui.selected_index
                    && idx > 0
                {
                    ui.selected_index = Some(idx - 1);
                }
            }
            // PRD #89 M1.3 — Ctrl+W close-pane is a detach path; flush a fresh
            // snapshot reflecting the surviving workspace (every sub-path above:
            // closable-tab close, mode/orchestration tab close, plain pane).
            ui.mark_session_dirty();
        }
        // Ctrl+PageDown: next tab (clamped, gated on a visible tab bar).
        // PRD #83: route through `switch_tab_with_focus` so the source tab's
        // focused pane is captured and the destination tab's remembered focus
        // is restored.
        Action::GlobalNextTab => {
            if tab_manager.show_tab_bar() {
                let prev_idx = tab_manager.active_index();
                // PRD #84 M4: switching just updates tab state; the pre-draw
                // `resize_panes_to_layout` sizes the destination tab's panes on
                // the next frame. PRD #113: pass `ui` so the deck selection
                // highlight is deactivated on leave/enter.
                switch_tab_with_focus(tab_manager, prev_idx + 1, pane, snapshot, ui);
            }
        }
        // Ctrl+PageUp: previous tab (clamped, gated on a visible tab bar).
        Action::GlobalPrevTab => {
            if tab_manager.show_tab_bar() {
                let prev_idx = tab_manager.active_index();
                if prev_idx > 0 {
                    switch_tab_with_focus(tab_manager, prev_idx - 1, pane, snapshot, ui);
                }
            }
        }
        // Normal-mode Tab / Right / l: cycle to the next tab (wraps).
        Action::CycleTabNext => {
            let count = tab_manager.tab_count();
            if count > 0 {
                let prev_idx = tab_manager.active_index();
                let next = (prev_idx + 1) % count;
                switch_tab_with_focus(tab_manager, next, pane, snapshot, ui);
            }
        }
        // Normal-mode BackTab / Left / h: cycle to the previous tab (wraps).
        Action::CycleTabPrev => {
            let count = tab_manager.tab_count();
            if count > 0 {
                let prev_idx = tab_manager.active_index();
                let prev = (prev_idx + count - 1) % count;
                switch_tab_with_focus(tab_manager, prev, pane, snapshot, ui);
            }
        }
        // PRD #80 M3: click a tab header → switch to that tab. PRD #83: preserve
        // per-tab focus across the switch. PRD #84 M4: PTY sizing happens in the
        // pre-draw resize pass, not here.
        Action::SelectTab(idx) => {
            switch_tab_with_focus(tab_manager, idx, pane, snapshot, ui);
        }
        // PRD #80 M3: click a tab's [×] → close that tab, reusing Ctrl+W's
        // tab-teardown semantics for the clicked tab.
        Action::CloseTab(idx) => {
            close_tab_by_index(idx, ui, state, tab_manager);
        }
        // PRD #80 M4 / PRD #83: single-click a card → select exactly that card
        // (PRD #68). Under #83 the Dashboard's selection is keyed by session id
        // in the active tab (`selected_session_id`), and `selected_index` is
        // derived from it each frame by `sync_and_derive_selection`. Write the
        // clicked card's session id straight into that store so the selection
        // survives the per-frame sync even for cards with no embedded pane;
        // then mirror the move into the embedded focus (the same pattern
        // `dispatch_normal_mode_key` uses for j/k) so a focused pane follows.
        Action::SelectCard(idx) => {
            if idx < filtered.len() {
                let prev = ui.selected_index;
                // PRD #113: a click activates the highlight on the clicked card.
                ui.selected_index = Some(idx);
                if let Tab::Dashboard {
                    selected_session_id,
                } = tab_manager.active_tab_mut()
                {
                    *selected_session_id = Some(filtered[idx].0.clone());
                }
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
            // PRD #84 M4: focusing a card can change which Stacked pane expands;
            // the pre-draw `resize_panes_to_layout` re-sizes it next frame.
        }
        // Mode tab in-tab navigation (j/Down): move side-pane focus down.
        // PRD #83: focus is tracked by stable pane id (`focused_pane_id`), so
        // translate it to a positional slot for the j/k arithmetic, step, then
        // store the new id back.
        Action::ModeTabSelectNext => {
            if let Tab::Mode {
                focused_pane_id,
                mode_manager,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                let side_ids = mode_manager.managed_pane_ids();
                let side_count = side_ids.len();
                let cur: Option<usize> = focused_pane_id
                    .as_ref()
                    .and_then(|id| side_ids.iter().position(|s| s == id));
                let next = match cur {
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
                *focused_pane_id = next.map(|i| side_ids[i].clone());
                let focus_id = focused_pane_id
                    .clone()
                    .unwrap_or_else(|| agent_pane_id.clone());
                let _ = pane.focus_pane(&focus_id);
            }
        }
        // Mode tab in-tab navigation (k/Up): move side-pane focus up.
        Action::ModeTabSelectPrev => {
            if let Tab::Mode {
                focused_pane_id,
                mode_manager,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                let side_ids = mode_manager.managed_pane_ids();
                let side_count = side_ids.len();
                let cur: Option<usize> = focused_pane_id
                    .as_ref()
                    .and_then(|id| side_ids.iter().position(|s| s == id));
                let next = match cur {
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
                *focused_pane_id = next.map(|i| side_ids[i].clone());
                let focus_id = focused_pane_id
                    .clone()
                    .unwrap_or_else(|| agent_pane_id.clone());
                let _ = pane.focus_pane(&focus_id);
            }
        }
        // Mode tab in-tab navigation (Enter): focus the selected side/agent pane.
        Action::ModeTabFocus => {
            if let Tab::Mode {
                focused_pane_id,
                mode_manager,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                let target_pane_id = focused_pane_id
                    .clone()
                    .unwrap_or_else(|| agent_pane_id.clone());
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
                        format!(
                            "PaneInput mode — type to interact, {} for dashboard",
                            display_notation(&ui.keybindings, KbAction::Dashboard)
                        ),
                        std::time::Instant::now(),
                    ));
                }
            }
        }
        // Mode tab in-tab navigation (Esc): reset focus back to the agent pane.
        Action::ModeTabReset => {
            if let Tab::Mode {
                focused_pane_id,
                agent_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                *focused_pane_id = None;
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
            // PRD #113 (UNIFIED deck behavior, Issue 1): paint the selection
            // highlight on the card Enter is about to focus, mirroring
            // `focus_deck` (the digit-jump path, which sets `selected_index`
            // directly and already paints on BOTH decks). Resolve the SAME
            // target index the caller used to pick `selected_id`
            // (`dashboard_focus_target`, which restores the remembered selection
            // when the deck returned inactive). Without this, Enter on an
            // Orchestration deck whose role pane is already focused on return
            // never re-arms the highlight: the per-frame reconcile reactivates
            // only on a focus TRANSITION, and re-focusing the already-focused
            // pane isn't one (which is why the Dashboard happened to work and
            // the Orchestration deck didn't). Setting it here is one shared
            // change covering both decks.
            ui.selected_index = dashboard_focus_target(ui, filtered.len());
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
                        // PRD #83: snapshot the SOURCE tab's focus before
                        // leaving it, mirroring `switch_tab_with_focus`
                        // (capture-out → switch_to → record). This path is
                        // reachable from a focus-bearing Orchestration tab
                        // (Enter on its card grid), so the source's remembered
                        // focus must be captured. `capture_focus_on_switch_out`
                        // only reads the controller's focused pane id into the
                        // source tab's field — it issues no `focus_pane`, so the
                        // destination focus below is unaffected.
                        tab_manager.capture_focus_on_switch_out();
                        tab_manager.switch_to(tab_idx);
                        tab_manager.record_focus(pane_id);
                        // PRD #84 M4: the destination tab's panes are sized by
                        // the pre-draw `resize_panes_to_layout` next frame.
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
                                format!(
                                    "PaneInput mode — type to interact, {} for dashboard",
                                    display_notation(&ui.keybindings, KbAction::Dashboard)
                                ),
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
                if let Some(orch_config) = req.orchestration_config {
                    // PRD #107 regression fix: do NOT overwrite
                    // `orch_config.name` with the form name. That override
                    // corrupted the orchestration IDENTITY — the canonical
                    // config name is what the daemon's
                    // `lookup_orchestration_role` compares to honor a role's
                    // `clear`/`prompt_template`, and overwriting it with the
                    // dir basename broke the delegate respawn in every
                    // worktree (PRD #107 only worked in the main checkout
                    // where basename == config name). Instead route the form
                    // name to the tab TITLE only, via `display_title`; the
                    // identity stays the canonical `orch_config.name`.
                    let display_title = (!req.name.is_empty()).then(|| req.name.clone());
                    let prompt = prepare_orchestrator_prompt(&orch_config, &dir_str);
                    // PRD #89 M2b.2: keep a copy of the prepared prompt for the
                    // capture snapshot below — `prompt` itself is moved into
                    // `open_orchestration_tab`. Empty when the orchestration
                    // had no prompt.
                    let captured_prompt = prompt.clone().unwrap_or_default();
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
                    // layout); pass role_index=0 so the helper falls
                    // back to "role 0 expanded" — matching the
                    // renderer's "first slot expands" default when
                    // no role is focused yet (panes haven't been
                    // created at this point). `Tiled` makes role_index
                    // a geometric no-op anyway; the per-frame
                    // `resize_panes_to_layout` reconciles the exact
                    // rects once the tab is active.
                    let spawn_dims = orchestration_role_pane_dims(
                        frame_area,
                        orch_config.roles.len(),
                        0,
                        PaneLayout::Tiled,
                        true,
                    );
                    match tab_manager.open_orchestration_tab(
                        &orch_config,
                        &dir_str,
                        prompt,
                        display_title.as_deref(),
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
                            let start_idx =
                                orch_config.roles.iter().position(|r| r.start).unwrap_or(0);
                            // PRD #89 M2b.2 — capture the orchestration metadata
                            // onto the START (orchestrator) role pane so the
                            // daemon-empty restore path can rebuild the whole tab
                            // (orchestrator + role panes in display order, the
                            // prompt, the start cursor) by re-resolving the
                            // config from `project_path` + `config_name`. Only the
                            // start role carries the snapshot — a single
                            // `[panes.orchestration]` block — so restore rebuilds
                            // the others from the re-resolved config rather than
                            // duplicating them as plain panes. The non-start role
                            // panes deliberately get NO `pane_metadata` entry.
                            ui.pane_metadata.insert(
                                role_pane_ids[start_idx].clone(),
                                config::SavedPane {
                                    dir: dir_str.clone(),
                                    name: orch_config.roles[start_idx].name.clone(),
                                    command: orch_config.roles[start_idx].command.clone(),
                                    mode: None,
                                    orchestration: Some(config::OrchestrationSnapshot {
                                        version: 1,
                                        roles: orch_config
                                            .roles
                                            .iter()
                                            .map(|r| r.name.clone())
                                            .collect(),
                                        start_role_index: start_idx,
                                        orchestrator_prompt: captured_prompt.clone(),
                                        config_name: orch_config.name.clone(),
                                        project_path: dir_str.clone(),
                                        started_role_indices: vec![start_idx],
                                        // PRD #89 review-fix F4: capture the
                                        // user-typed tab title so the
                                        // daemon-empty restore rebuilds the tab
                                        // under the user's name, not the
                                        // canonical config/cwd name.
                                        display_title: display_title.clone(),
                                    }),
                                },
                            );
                            // PRD #89 M1.2 — opening an orchestration tab is a
                            // meaningful state change; keep the snapshot fresh.
                            ui.mark_session_dirty();

                            // Focus the start role's pane.
                            let _ = pane.focus_pane(&role_pane_ids[start_idx]);
                            ui.mode = UiMode::PaneInput;

                            // PRD #84 M4: role panes were spawned at the
                            // orchestration layout dims (above); the pre-draw
                            // `resize_panes_to_layout` reconciles them to the
                            // exact rect on the next frame.

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
                                // Show the same name as the tab title (PRD #107
                                // display intent): the user's form name when
                                // typed, else the canonical config name. This is
                                // display-only and does not affect identity.
                                format!(
                                    "Activated orchestration: {}",
                                    display_title.as_deref().unwrap_or(&orch_config.name)
                                ),
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
                                    // PRD #89 M2b.2: capture is a later step
                                    // (M2b.3); the schema field exists now so
                                    // older/newer snapshots round-trip.
                                    orchestration: None,
                                },
                            );
                            // PRD #89 M1.2 — a new dashboard pane / mode tab is a
                            // meaningful state change; keep the snapshot fresh.
                            ui.mark_session_dirty();

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
                                        // PRD #84 M4: the agent + side panes were
                                        // already spawned at the mode-tab dims
                                        // (above), so commands start at the right
                                        // PTY size; the pre-draw
                                        // `resize_panes_to_layout` reconciles the
                                        // exact rect next frame. No resize here.
                                        let _ = tab_manager.start_mode_commands();
                                        // Send the agent pane command after resize
                                        // so it starts at the correct PTY dimensions.
                                        if let Some(ref init_cmd) = mode_config.init_command {
                                            let _ = pane.write_to_pane(&new_id, init_cmd);
                                        }
                                        // PRD #127 M3.1: a mode carrying a
                                        // `seed_prompt` enqueues it for GATED
                                        // delivery to the agent pane — drained by
                                        // `process_pending_seed_prompts` once the
                                        // agent signals readiness (SessionStart)
                                        // plus the spawn-time buffer, unlike
                                        // `init_command` (written immediately
                                        // above). A mode without `seed_prompt`
                                        // enqueues nothing (no-op for plain modes).
                                        if let Some(ref seed) = mode_config.seed_prompt {
                                            ui.pending_seed_prompts.push(PendingSeedPrompt {
                                                pane_id: new_id.clone(),
                                                prompt: seed.clone(),
                                                created_at: std::time::Instant::now(),
                                                ready_since: None,
                                            });
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
                                // No mode — regular dashboard card. The card lives on the Dashboard
                                // (tab 0), so make the Dashboard active before focusing/selecting it —
                                // otherwise, when launched from an orchestration/mode tab, the new card
                                // lands on a tab the user isn't viewing. (Orchestration/mode creation
                                // already switch to their own new tab via open_*_tab.)
                                //
                                // Capture the leaving tab's live focus before the switch, mirroring
                                // the established switch-out invariant (every other production
                                // `switch_to` — src/ui.rs:3059, 4074, 4625, and the
                                // `switch_tab_with_focus` helper — is preceded by
                                // `capture_focus_on_switch_out`). Without it, a card created from a
                                // non-Dashboard tab never snapshots that tab's `focused_pane_id`, so
                                // its prior focus goes stale and fails to restore on return. Safe
                                // here: focus is still the leaving tab's pane at this point, and the
                                // `pane.focus_pane(&new_id)` below then moves focus to the new card.
                                tab_manager.capture_focus_on_switch_out();
                                tab_manager.switch_to(0);
                                let _ = pane.focus_pane(&new_id);
                                ui.mode = UiMode::PaneInput;
                                // PRD #113: the freshly-created card is active.
                                ui.selected_index = Some(filtered.len());
                                // PRD #84 M4: the pane was spawned at the
                                // dashboard layout dims (above); the pre-draw
                                // `resize_panes_to_layout` reconciles it to the
                                // exact rect next frame. No resize here.
                                // PRD #127: a single-agent card carrying a
                                // `seed_prompt` (the built-in "schedule"
                                // authoring session) enqueues it for the SAME
                                // gated delivery modes use — drained by
                                // `process_pending_seed_prompts` once the agent
                                // signals readiness (SessionStart) plus the
                                // spawn-time buffer. A card without a seed
                                // enqueues nothing (no-op for ordinary panes).
                                if let Some(seed) = req.seed_prompt {
                                    ui.pending_seed_prompts.push(PendingSeedPrompt {
                                        pane_id: new_id.clone(),
                                        prompt: seed,
                                        created_at: std::time::Instant::now(),
                                        ready_since: None,
                                    });
                                }
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
            send_config_gen_prompt(&pane_id, &cwd, ui, pane, tab_manager);
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
                send_config_gen_prompt(&pane_id, &cwd, ui, pane, tab_manager);
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
            // PRD #89 M1.2 — a rename changes the saved display name; keep the
            // snapshot fresh.
            ui.mark_session_dirty();
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
        // [Cancel] → close the picker (== q / Esc). PRD #170 finding 5: routes
        // intent-aware — a manager-originated pick returns to the manager.
        Action::PickerCancel => {
            cancel_dir_picker(ui);
        }
        // [Filter] → open the picker's filter input (== `/`).
        Action::PickerFilter => {
            if let Some(picker) = ui.dir_picker.as_mut() {
                picker.filtering = true;
            }
        }
        // ===== PRD #80 M8: new-pane-form click actions =====
        // click a field row → focus it (== Tab / Shift+Tab landing).
        Action::FormFocusField(field) => {
            if let Some(form) = ui.new_pane_form.as_mut() {
                form.focused = field;
            }
        }
        // click a mode chip → select that option (== Left/Right/h/l cycler).
        Action::FormSelectMode(idx) => {
            if let Some(form) = ui.new_pane_form.as_mut()
                && idx < form.mode_option_count()
            {
                form.selection_index = idx;
                form.focused = FormField::Mode;
            }
        }
        // [Submit] → spawn the pane from the form values (== Enter on the final
        // field). Reuses the SpawnPane arm so click and key spawn identically.
        Action::FormSubmit => {
            if let Some(form) = ui.new_pane_form.take() {
                ui.mode = UiMode::Normal;
                let req = build_new_pane_request(&form, &ui.config.default_command);
                return dispatch_action(
                    Action::SpawnPane(Box::new(req)),
                    ui,
                    pane,
                    state,
                    tab_manager,
                    snapshot,
                    filtered,
                    selected_id,
                    frame_area,
                );
            }
        }
        // [Cancel] → close the form without spawning (== Esc). PRD #170 finding 5:
        // a locked schedule form returns to the manager (intent-aware via
        // `schedule_locked`); the ordinary form drops to the dashboard.
        Action::FormCancel => {
            cancel_new_pane_form(ui);
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
        // PRD #127 finding #4: open the manager dialog. Shared by the dashboard
        // `s`/`S` key and the `[Scheduled Tasks s]` button-bar button. Loads the
        // schedules from the global config and snapshots which currently have a
        // live tab/agent (for the status indicator), then switches mode.
        Action::OpenScheduledTasks => {
            let tasks = config::LoadedSchedules::load().tasks;
            ui.scheduled_tasks = tasks;
            ui.scheduled_selected = 0;
            ui.scheduled_delete_confirm = false;
            ui.scheduled_live_names = live_schedule_names();
            ui.mode = UiMode::ScheduledTasks;
        }
        // PRD #127 finding #4: `[Add]` button parity for the `a` key. PRD #170
        // (unify): open the directory picker marked `ScheduleAdd` (reusing the
        // `Ctrl+n` flow) instead of a bespoke modal — confirming a dir builds
        // the mode-locked ` New Schedule ` form, which spawns on submit.
        Action::ScheduleAdd => {
            open_schedule_dir_picker(ui, None);
        }
        // PRD #127 finding #4: `[Edit]` button parity for the `e`/Enter key.
        // PRD #170 (unify): open the directory picker (at the row's working_dir)
        // marked `ScheduleEdit(row)` so the mode-locked ` Edit Schedule ` form
        // pre-fills the authoring seed from the row (blank context when empty).
        Action::ScheduleEdit => {
            let existing = ui.scheduled_tasks.get(ui.scheduled_selected).cloned();
            open_schedule_dir_picker(ui, existing);
        }
        // PRD #127 finding #4: `[Delete]` button parity for the `d` key — arm
        // the definition-only delete confirmation for the selected row (the
        // dialog stays open; `y` / the confirmed delete then removes it).
        Action::ScheduleArmDelete => {
            if ui.scheduled_selected < ui.scheduled_tasks.len() {
                ui.scheduled_delete_confirm = true;
            }
        }
        Action::ScheduleRunNow(name) => {
            // PRD #127 M3.3: fire the schedule now via the daemon's RunNow control message.
            match send_daemon_request_blocking(&crate::daemon_protocol::AttachRequest::RunNow {
                name: name.clone(),
            }) {
                Ok(resp) if resp.ok => {
                    let msg = match crate::daemon_client::run_now_outcome_from_agents(&resp.agents)
                    {
                        crate::daemon_client::RunNowOutcome::Started => {
                            format!("Ran schedule '{name}'")
                        }
                        crate::daemon_client::RunNowOutcome::SkippedStillRunning => {
                            format!("'{name}' already running — skipped")
                        }
                    };
                    ui.status_message = Some((msg, std::time::Instant::now()));
                }
                Ok(resp) => {
                    ui.status_message = Some((
                        format!(
                            "Run-now failed: {}",
                            resp.error.unwrap_or_else(|| "unknown error".to_string())
                        ),
                        std::time::Instant::now(),
                    ));
                }
                Err(e) => {
                    ui.status_message =
                        Some((format!("Run-now failed: {e}"), std::time::Instant::now()));
                }
            }
            // PRD #127 N2: dialog stays open after run-now; refresh live-status snapshot.
            if ui.mode == UiMode::ScheduledTasks {
                ui.scheduled_live_names = live_schedule_names();
            }
        }
        Action::ScheduleDelete(name) => {
            // PRD #127 M3.3: remove the DEFINITION only — rewrite the global
            // schedules.toml via the validated writer, then reload. Does NOT
            // touch any open tab/agent for the schedule.
            let mut loaded = config::LoadedSchedules::load();
            match crate::schedule_cli::remove(&mut loaded.tasks, &name) {
                Ok(()) => {
                    let path = config::schedules_path();
                    if let Err(e) = crate::schedule_cli::write_atomic(&path, &loaded.tasks) {
                        ui.status_message =
                            Some((format!("Delete failed: {e}"), std::time::Instant::now()));
                    } else {
                        let _ = send_daemon_request_blocking(
                            &crate::daemon_protocol::AttachRequest::ReloadSchedules,
                        );
                        ui.status_message = Some((
                            format!("Deleted schedule '{name}' (open tab kept)"),
                            std::time::Instant::now(),
                        ));
                    }
                }
                Err(e) => {
                    ui.status_message =
                        Some((format!("Delete failed: {e}"), std::time::Instant::now()));
                }
            }
            // PRD #127 N2: dialog stays open after delete — refresh list and clamp selection.
            if ui.mode == UiMode::ScheduledTasks {
                ui.scheduled_tasks = config::LoadedSchedules::load().tasks;
                ui.scheduled_live_names = live_schedule_names();
                if ui.scheduled_selected >= ui.scheduled_tasks.len() {
                    ui.scheduled_selected = ui.scheduled_tasks.len().saturating_sub(1);
                }
            }
        }
        Action::Continue => {}
    }
    Flow::Continue
}

/// PRD #89 M1.2 — flush the saved-session snapshot to disk when the coalescer
/// says a write is due (a state change is pending and the throttle interval has
/// elapsed since the last write). Called once per main-loop iteration, so the
/// snapshot stays continuously fresh on meaningful state changes and detaches
/// without writing on every keystroke. Mirrors the pre-teardown snapshot block
/// (build from the live panes, clear when empty), then records the write so the
/// throttle re-arms.
fn flush_session_snapshot_if_due(ui: &mut UiState, state: &SharedState) {
    let now = ui.session_epoch.elapsed();
    if !ui.session_coalescer.is_due(now) {
        return;
    }
    let live_panes = state.blocking_read().managed_pane_ids.clone();
    let session =
        config::SavedSession::snapshot(&mut ui.pane_metadata, &ui.pane_display_names, &live_panes);
    // PRD #89 review-fix F10: only mark the write as done (clearing the dirty
    // flag via `record_write`) when the persist actually SUCCEEDED. On a
    // transient disk error we leave the coalescer dirty so the next loop
    // iteration retries, rather than dropping the pending snapshot until the
    // next unrelated state change re-dirties it.
    let result = if session.panes.is_empty() {
        config::SavedSession::clear().map_err(|e| format!("Warning: failed to clear session: {e}"))
    } else {
        session
            .save()
            .map_err(|e| format!("Warning: failed to save session: {e}"))
    };
    match result {
        Ok(()) => {
            // PRD #89 review-fix G1: a successful write/clear ends any ongoing
            // failure streak, so re-arm the one-shot warning for the next time
            // a *new* failure occurs.
            ui.session_snapshot_write_failed = false;
            ui.session_coalescer.record_write(now);
        }
        Err(warning) => {
            // PRD #89 review-fix G1: F10 keeps the coalescer dirty so the next
            // loop retries, but on a PERSISTENT failure (disk full, bad perms)
            // that retry fires every ~750ms. Push the warning AT MOST ONCE per
            // ongoing failure streak so `session_warnings` cannot grow unbounded.
            if !ui.session_snapshot_write_failed {
                ui.session_warnings.push(warning);
                ui.session_snapshot_write_failed = true;
            }
        }
    }
}

/// PRD #89 M2.2 — daemon-vs-snapshot restore precedence seam.
///
/// Returns `true` when the on-disk saved-session snapshot should be applied on
/// startup, `false` when it must be skipped. On every startup the TUI hydrates
/// from the daemon first; if that produced *any* managed pane (any
/// `managed_pane_id` registered in `state`), the daemon already owns the live
/// workspace and wins — restoring the snapshot on top of it would double up the
/// panes. Only when hydration produced zero panes (a fresh daemon, or crash
/// recovery into an empty registry) do we fall back to the disk snapshot. When
/// both sources are empty, the snapshot load is attempted but yields nothing and
/// the dashboard lands empty.
///
/// The decision is purely *structural* — the presence or absence of hydrated
/// managed panes — never a flag, which keeps it a pure function that can be
/// unit-tested in isolation (see `session/restore/005`).
pub fn should_apply_snapshot(state: &AppState) -> bool {
    state.managed_pane_ids.is_empty()
}

/// PRD #89 M2b.3 — re-resolve the `OrchestrationConfig` for a snapshot's
/// orchestration metadata on the daemon-empty restore path.
///
/// Returns the resolved config on success, or a human-readable drift reason
/// (always NAMING the missing/renamed orchestration) when:
/// - the project config can't be loaded (file gone / unreadable),
/// - there is no project config at the saved `project_path`,
/// - the named orchestration was renamed/removed (no match by `config_name`),
/// - the saved roles no longer match the resolved config's roles (a role was
///   added/removed/renamed/reordered) — the tab the user saved no longer
///   exists as saved.
///
/// On `Err`, the caller surfaces the reason via `session_warnings` and falls
/// back to a PLAIN dashboard pane (never a half-broken orchestration tab),
/// mirroring the mode-tab drift Path D/E fallback (PRD #69).
///
/// On `Ok`, returns the resolved config AND the validated start-role index to
/// honor (the SAVED cursor, bounds-checked — PRD #89 review-fix F2/F3), so the
/// caller drives prompt delivery from the saved start role rather than
/// recomputing it from the live config's `start` flags.
fn resolve_orchestration_for_restore(
    snap: &config::OrchestrationSnapshot,
    saved_dir: &str,
) -> Result<(OrchestrationConfig, usize), String> {
    // PRD #89 review-fix F1 (security): the snapshot's `project_path` and the
    // saved pane's `dir` are written EQUAL on capture; a divergence only happens
    // via a tampered `session.toml`. Re-resolving — and auto-running — the
    // config at a divergent `project_path` would execute a `.dot-agent-deck.toml`
    // an attacker controls. Canonicalize both and refuse to re-resolve unless
    // they name the same directory; a divergence (or a `project_path` that no
    // longer resolves) is treated as drift -> plain-pane fallback, so the
    // planted config is never executed.
    let project_dir = std::path::Path::new(&snap.project_path);
    let canon_project = project_dir.canonicalize().map_err(|e| {
        format!(
            "orchestration '{}' project_path {} could not be resolved: {e}",
            snap.config_name, snap.project_path
        )
    })?;
    let canon_saved = std::path::Path::new(saved_dir)
        .canonicalize()
        .map_err(|e| {
            format!(
                "orchestration '{}' saved dir {saved_dir} could not be resolved: {e}",
                snap.config_name
            )
        })?;
    if canon_project != canon_saved {
        return Err(format!(
            "orchestration '{}' project_path {} does not match saved pane dir {saved_dir} — \
             refusing to re-resolve a divergent config",
            snap.config_name, snap.project_path
        ));
    }

    let cfg = match load_project_config(&canon_project) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            return Err(format!(
                "orchestration '{}' not found — no project config in {}",
                snap.config_name, snap.project_path
            ));
        }
        Err(e) => {
            return Err(format!(
                "orchestration '{}' could not be resolved — failed to load project config from {}: {e}",
                snap.config_name, snap.project_path
            ));
        }
    };
    let orch = cfg
        .orchestrations
        .into_iter()
        .find(|o| o.name == snap.config_name)
        .ok_or_else(|| {
            format!(
                "orchestration '{}' not found in {} (renamed or removed)",
                snap.config_name, snap.project_path
            )
        })?;
    // Drift guard: the saved role set must still match the resolved config's
    // roles (same names, same order). A renamed/removed/reordered/added role
    // means the saved tab no longer exists as saved — fall back rather than
    // rebuild a different tab.
    let current_roles: Vec<&str> = orch.roles.iter().map(|r| r.name.as_str()).collect();
    let saved_roles: Vec<&str> = snap.roles.iter().map(String::as_str).collect();
    if current_roles != saved_roles {
        return Err(format!(
            "orchestration '{}' role set changed (saved [{}], now [{}])",
            snap.config_name,
            saved_roles.join(", "),
            current_roles.join(", "),
        ));
    }
    // PRD #89 review-fix F2 (robustness): bounds-check the SAVED start cursor
    // against the re-resolved role set. `load_project_config` runs no
    // config_validation, so a whittled-down (or explicitly `roles = []`) config
    // is structurally valid yet role-less, and a tampered cursor could point
    // past the end. Either would index an empty/short `role_pane_ids` and panic
    // at startup (crash-loop). Treat an out-of-range cursor — which includes
    // every index of an empty role set — as drift, so the caller falls back to
    // a plain pane instead of panicking.
    if snap.start_role_index >= orch.roles.len() {
        return Err(format!(
            "orchestration '{}' has no role at saved start index {} (role count {}) — falling back",
            snap.config_name,
            snap.start_role_index,
            orch.roles.len()
        ));
    }
    // PRD #89 review-fix F3: honor the SAVED start cursor (now validated in
    // range) instead of recomputing it from the live config's `start` flags, so
    // a saved cursor that differs from the config default is preserved on
    // restore.
    Ok((orch, snap.start_role_index))
}

pub fn run_tui(
    state: SharedState,
    pane: Arc<dyn PaneController>,
    config: DashboardConfig,
    keybindings: KeybindingConfig,
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
    let mut ui = UiState::new(config, keybindings);
    let mut tab_manager = TabManager::new(Arc::clone(&pane));

    let mut star_state = config::StarPromptState::load();
    let should_show_star = star_state.increment_and_check();
    ui.star_prompt_state = star_state;
    if should_show_star {
        ui.mode = UiMode::StarPrompt;
    }

    // PRD #111: preferred landing tab after both the hydration block and the
    // snapshot-restore block have run. Defaults to dashboard (0); the
    // hydration block overwrites this to the first rebuilt orchestration tab
    // when one exists. Hoisted out of the embedded-pane scope so the
    // snapshot-restore block below can honour it instead of unconditionally
    // snapping back to the dashboard — without the hoist, the second
    // `switch_to(0)` in the restore path would undo the orchestration
    // landing on every startup.
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
            // Carry the daemon-recorded cwd through to the seeded session
            // so the session card shows the right directory.
            //
            // PRD #162: seed the card from the daemon's live, event-derived
            // `SessionSnapshot` (`h.live`) when one is attached, so the
            // reconnected dashboard shows the agent's REAL status / active
            // tool / tool count / prompt context immediately — not a bare
            // `Idle` reset until the next event arrives. When `h.live` is
            // `None` (older daemon, dummy-state attach path, or an agent that
            // never emitted an event) `seed_hydrated_session` falls back to
            // the bare `insert_placeholder_session` placeholder — today's
            // behavior, so no reconnect regresses.
            //
            // PRD #76 M2.13: the daemon-recorded spawn-time `agent_type` is
            // still passed through; the snapshot's event-derived value wins
            // when present (the "No agent" fix — a snapshot overrides a
            // `None` spawn-time type with the agent's real label), and the
            // spawn-time value is the fallback when the snapshot is absent or
            // carries no type.
            // PRD #110 followup: seeding mints the card with the
            // daemon-recorded `agent_id` (carried via `HydratedPane.agent_id`)
            // so the strict-equality reuse guard in `apply_event` lets a
            // post-reconnect `SessionStart` from the same agent remap onto
            // the seeded card. Without this, it would carry `agent_id=None`
            // and a `SessionStart` with `Some(daemon-id)` would not match → a
            // second card would appear beside the hydrated one.
            st.seed_hydrated_session(
                h.pane_id.clone(),
                h.cwd.clone(),
                h.agent_type.clone(),
                Some(h.agent_id.clone()),
                h.live.as_ref(),
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
                        // PRD #89 M2b.2: schema-only; orchestration capture
                        // from a warm daemon is handled by hydration, not the
                        // snapshot, so leave None here.
                        orchestration: None,
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
        // viewport-aware dims at create time. PRD #84 M4: the per-frame
        // `resize_panes_to_layout` pass reconciles the exact rects once a tab
        // is active, but spawning at the right size avoids the 24×80 hiccup.
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
                bucket.display_title.as_deref(),
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
        // snapshot-restore block below doesn't snap back to the
        // dashboard on startup (CodeRabbit PR #114).
        preferred_start_tab = first_orchestration_tab_index.unwrap_or(0);
        tab_manager.switch_to(preferred_start_tab);

        // PRD #84 M4: the post-hydration resize sweep is gone. The panes were
        // rebuilt from the daemon's existing PTYs (or seeded at 24×80), and the
        // per-frame `resize_panes_to_layout` (invariant 2) sizes the active tab
        // on the very first frame; a background mode/orchestration tab is sized
        // the frame it becomes active — before it is ever rendered — so no tab
        // is shown at the wrong dims.
    }

    // PRD #89 M2.1/M2.2: restore is now UNCONDITIONAL — there is no `--continue`
    // gate. Daemon hydration (above) runs first and wins: if it produced any
    // managed pane, the daemon already owns the workspace and we skip the disk
    // snapshot to avoid double-restoring. Only when hydration produced zero
    // panes (fresh daemon / crash recovery) do we load and apply the snapshot.
    // The precedence decision lives in the pure `should_apply_snapshot` seam so
    // it stays structural (any hydrated managed_pane_id) rather than flag-driven.
    let apply_snapshot = should_apply_snapshot(&state.blocking_read());
    if apply_snapshot {
        // Ensure the terminal has up-to-date dimensions before we resize
        // any PTYs — without this, get_frame().area() may return stale or
        // default values because no draw() call has happened yet.
        let _ = terminal.autoresize();

        let saved = config::SavedSession::load();
        // Collect deferred mode pane restores — we need the terminal ready
        // before we can resize PTYs, so mode tabs are opened after the loop.
        let mut deferred_mode_panes: Vec<(config::SavedPane, ModeConfig)> = Vec::new();
        // PRD #89 M2b.3 — the first orchestration tab rebuilt from the snapshot,
        // so we can land on it (start cursor) after the loop rather than snapping
        // back to the dashboard — mirroring the hydration block's landing logic.
        let mut first_restored_orch_tab: Option<usize> = None;
        for saved_pane in &saved.panes {
            let dir = std::path::Path::new(&saved_pane.dir);
            if !dir.is_dir() {
                ui.session_warnings.push(format!(
                    "Warning: skipping pane '{}' — directory {} not found",
                    saved_pane.name, saved_pane.dir
                ));
                continue;
            }
            // PRD #89 M2b.3 — orchestration-tab restore on the daemon-empty
            // path. A saved pane carrying orchestration metadata rebuilds the
            // WHOLE tab (orchestrator + role panes in saved order, the prompt
            // replayed to the start role, the start cursor) by re-resolving its
            // `OrchestrationConfig` from `project_path` + `config_name`. On
            // success we `continue`; on drift (config gone, orchestration
            // renamed/removed, role set changed) we push a clear warning NAMING
            // the orchestration and fall through to the plain-pane restore below
            // — never a half-broken tab (mirrors the mode-tab Path D/E fallback,
            // PRD #69). Unlike warm-daemon hydration (session/restore/007), this
            // path REPLAYS the saved `orchestrator_prompt` to the start role —
            // there is no live agent with the prompt already in scrollback.
            if let Some(ref orch_snap) = saved_pane.orchestration {
                match resolve_orchestration_for_restore(orch_snap, &saved_pane.dir) {
                    Ok((orch_config, saved_start_idx)) => {
                        let frame_area = terminal.get_frame().area();
                        // All roles share spawn dims (Tiled); role_index=0 so the
                        // helper falls back to "role 0 expanded" — the per-frame
                        // `resize_panes_to_layout` reconciles the exact rects once
                        // the tab is active. Mirrors the live new-pane path.
                        let spawn_dims = orchestration_role_pane_dims(
                            frame_area,
                            orch_config.roles.len(),
                            0,
                            PaneLayout::Tiled,
                            true,
                        );
                        // Empty saved prompt → `None` so the delivery gate
                        // writes nothing (matching the live path's "no prompt"
                        // semantics); a non-empty prompt is replayed to the
                        // start role once it signals readiness.
                        let replay_prompt = (!orch_snap.orchestrator_prompt.is_empty())
                            .then(|| orch_snap.orchestrator_prompt.clone());
                        match tab_manager.open_orchestration_tab(
                            &orch_config,
                            &saved_pane.dir,
                            replay_prompt,
                            // PRD #89 review-fix F4: thread the saved user title
                            // so the rebuilt tab comes back under the user's name
                            // rather than the canonical config/cwd name. `None`
                            // when unset falls back to the canonical name.
                            orch_snap.display_title.as_deref(),
                            spawn_dims,
                        ) {
                            Ok((tab_idx, role_pane_ids)) => {
                                // PRD #89 review-fix F3: honor the SAVED start
                                // cursor. `open_orchestration_tab` computed
                                // `start_role_index` from the config's `start`
                                // flags; override it with the validated saved
                                // index so the prompt-delivery gate (and the
                                // landing focus) target the role the user left
                                // as start, even when it differs from the config
                                // default. The just-opened tab is the active tab.
                                if let Tab::Orchestration {
                                    start_role_index, ..
                                } = tab_manager.active_tab_mut()
                                {
                                    *start_role_index = saved_start_idx;
                                }
                                // Snapshot each role pane's daemon agent_id before
                                // the placeholder insert so the strict-equality
                                // reuse guard accepts each role agent's first
                                // `SessionStart` (mirrors the live path).
                                let role_agent_ids: Vec<Option<String>> = role_pane_ids
                                    .iter()
                                    .map(|id| pane.pane_agent_id(id))
                                    .collect();
                                {
                                    let mut st = state.blocking_write();
                                    for (id, agent_id) in
                                        role_pane_ids.iter().zip(role_agent_ids.iter())
                                    {
                                        st.register_pane(id.clone());
                                        st.insert_placeholder_session(
                                            id.clone(),
                                            Some(saved_pane.dir.clone()),
                                            None,
                                            agent_id.clone(),
                                        );
                                    }
                                    for (i, role) in orch_config.roles.iter().enumerate() {
                                        st.pane_role_map
                                            .insert(role_pane_ids[i].clone(), role.name.clone());
                                        st.pane_cwd_map.insert(
                                            role_pane_ids[i].clone(),
                                            saved_pane.dir.clone(),
                                        );
                                        if role.start {
                                            st.orchestrator_pane_ids
                                                .insert(role_pane_ids[i].clone());
                                        }
                                    }
                                }
                                for (i, role) in orch_config.roles.iter().enumerate() {
                                    ui.pane_display_names
                                        .insert(role_pane_ids[i].clone(), role.name.clone());
                                    ui.pane_names
                                        .insert(role_pane_ids[i].clone(), role.name.clone());
                                }
                                // Re-capture the orchestration metadata onto the
                                // start role pane so a later snapshot keeps the
                                // tab (only the start role carries the block).
                                // PRD #89 review-fix F3: key it under the SAVED
                                // (validated) start cursor — consistent with the
                                // honored start role above — not the config
                                // default, so a re-save preserves the saved
                                // cursor on the same pane that carries the block.
                                ui.pane_metadata.insert(
                                    role_pane_ids[saved_start_idx].clone(),
                                    saved_pane.clone(),
                                );
                                // Record creation time so the prompt-delivery
                                // gate's 10s timeout fallback works for agents
                                // that never signal `SessionStart`.
                                if let Tab::Orchestration { id, .. } = tab_manager.active_tab() {
                                    ui.orchestration_created_at
                                        .insert(*id, std::time::Instant::now());
                                }
                                if first_restored_orch_tab.is_none() {
                                    first_restored_orch_tab = Some(tab_idx);
                                }
                                continue;
                            }
                            Err(e) => {
                                ui.session_warnings.push(format!(
                                    "Warning: failed to rebuild orchestration '{}': {e}; restoring '{}' as a plain pane",
                                    orch_snap.config_name, saved_pane.name
                                ));
                                // Fall through to the plain-pane restore below.
                            }
                        }
                    }
                    Err(reason) => {
                        ui.session_warnings.push(format!(
                            "Warning: {reason}; restoring '{}' as a plain pane",
                            saved_pane.name
                        ));
                        // Fall through to the plain-pane restore below.
                    }
                }
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
            // bleed-through from `AgentSpawnOptions::default()`. PRD #84 M4:
            // the per-frame `resize_panes_to_layout` pass reconciles any
            // rounding once every restored pane is in the layout.
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
                            // PRD #84 M4: the restored agent + side panes were
                            // spawned at the mode-tab dims (above), so commands
                            // start at the right PTY size; the per-frame
                            // `resize_panes_to_layout` reconciles the exact rect
                            // once this tab is active. No resize here.
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
        // snapping back to the dashboard. `preferred_start_tab` defaults to
        // 0 (dashboard) — and stays there in this snapshot-restore branch,
        // which only runs when hydration produced no panes — preserving the
        // overview-first landing for snapshot-restored sessions.
        //
        // PRD #89 M2b.3: when the snapshot rebuilt an orchestration tab, land
        // on the first one (start cursor) so its role cards render — the
        // daemon-empty analogue of the hydration landing above.
        let landing_tab = first_restored_orch_tab.unwrap_or(preferred_start_tab);
        tab_manager.switch_to(landing_tab);

        // Focus the first restored pane and enter PaneInput mode so the user
        // can type immediately. PRD #84 M4: PTY sizing is handled by the
        // per-frame `resize_panes_to_layout` on the first loop iteration — no
        // startup resize sweep here.
        if let Some(embedded) = pane.as_any().downcast_ref::<EmbeddedPaneController>() {
            let ids = embedded.pane_ids();
            if let Some(first_id) = ids.first() {
                let _ = pane.focus_pane(first_id);
                ui.mode = UiMode::PaneInput;
            }
        }
        ui.selected_index = Some(0);
    }

    'outer: loop {
        // Expire stale status messages
        if let Some((_, created)) = &ui.status_message
            && created.elapsed() > STATUS_MESSAGE_TTL
        {
            ui.status_message = None;
        }

        // PRD #89 M1.2/M1.3 — keep the saved-session snapshot continuously
        // fresh: if a meaningful state change / detach marked it dirty and the
        // coalescer's throttle window has elapsed, flush it to disk now. This
        // runs every iteration (including the 16ms idle ticks below), so the
        // trailing write of a coalesced burst lands without further input.
        flush_session_snapshot_if_due(&mut ui, &state);

        // PRD #120: build live tabs for any orchestrations the daemon spawned
        // mid-session (issue dispatch). Done before the snapshot clone + tab
        // derivation below so a freshly-surfaced tab paints this same frame.
        process_pending_orchestration_surfaces(&state, &pane, &mut tab_manager);

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
            // PRD #84 M4: the recreated pane's PTY is sized by the pre-draw
            // `resize_panes_to_layout` on the next iteration (it was already
            // spawned at the cached side-pane dims above), so no ad hoc resize
            // is pushed here.
        }

        // PRD #83 M4 — remap every tab's focused pane after a reactive
        // pane-pool change. `route_reactive_commands` recreates reactive
        // panes across ALL tabs (active and background), returning
        // `(closed_id, new_id)` pairs, so each tab whose remembered focus
        // was recreated follows the successor; a focus that vanished with
        // no successor is cleared (fall back to the default pane on
        // switch-in). Only the active tab's remapped id comes back here,
        // and only it needs the live pane re-focused on the controller.
        if !pane_changes.is_empty()
            && let Some(new_id) = tab_manager.remap_focus_after_reactive_change(&pane_changes)
        {
            let _ = pane.focus_pane(&new_id);
        }
        // PRD #89 M1.2 — `route_reactive_commands` recreates Mode-tab reactive
        // SIDE panes, swapping their live pane ids (`pane_changes`); keep the
        // snapshot fresh so the new ids are reflected. NOTE: this covers only
        // reactive side-pane swaps — it does NOT broadly cover an agent /clear
        // restart, whose dirtying flows through other reactive seams.
        if !pane_changes.is_empty() {
            ui.mark_session_dirty();
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
            Tab::Dashboard { .. } => {
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

        // Clamp selection. PRD #113: an active selection (`Some`) is clamped
        // into range; an inactive one (`None`) is left inactive. With no cards
        // there is nothing to highlight.
        if total > 0 {
            if let Some(idx) = ui.selected_index {
                ui.selected_index = Some(idx.min(total - 1));
            }
        } else {
            ui.selected_index = None;
        }

        // PRD #83 M3/M4 — per-tab card selection. On the Dashboard and
        // Orchestration tabs the highlighted card is DERIVED each frame
        // from the tab's remembered stable id (session id / role pane id).
        // The globally focused pane feeds that remembered id — but ONLY
        // when its tab is active, so a pane focused on one tab can no
        // longer snap the selection of another (the cross-tab leak this
        // PRD fixes). A remembered id that's no longer in the filtered
        // list is cleared and the selection falls back to the first card.
        // Mode tabs render via the early-return path below, so
        // `selected_index` is irrelevant there and left as clamped.
        let focused_pane_now = pane.focused_pane_id();
        let filtered_ids: Vec<(&str, Option<&str>)> = filtered
            .iter()
            .map(|(id, s)| (id.as_str(), s.pane_id.as_deref()))
            .collect();
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            focused_pane_now.as_deref(),
            &filtered_ids,
        );

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
            Tab::Dashboard { .. } => ActiveTabView::Dashboard {
                exclude_pane_ids: tab_manager.all_managed_pane_ids(),
            },
            Tab::Mode {
                name,
                agent_pane_id,
                mode_manager,
                focused_pane_id,
                ..
            } => ActiveTabView::Mode {
                mode_name: name.clone(),
                agent_pane_id: agent_pane_id.clone(),
                side_pane_ids: mode_manager.managed_pane_ids(),
                focused_pane_id: focused_pane_id.clone(),
            },
            Tab::Orchestration { role_pane_ids, .. } => ActiveTabView::Orchestration {
                role_pane_ids: role_pane_ids.clone(),
            },
        };
        let tab_bar_labels: Vec<String> = tab_manager
            .tabs()
            .iter()
            .map(|tab| match tab {
                Tab::Dashboard { .. } => "Dashboard".to_string(),
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
        // PRD #84 M4 (invariants 1, 2 & 4) — ONE layout pass per frame, then
        // compute → resize → render, all against the SAME live frame area.
        //
        // `terminal.draw()` autoresizes the backing buffer to the live terminal
        // size before invoking the closure, so we must autoresize HERE too
        // before reading `get_frame().area()` (mirroring the hydration sweep at
        // the `terminal.autoresize()` call above). Without it, the first frame
        // after a terminal resize would size PTYs to the STALE pre-resize area
        // while the widget renders into the NEW area — tripping the
        // `contract_guaranteed` debug_assert in debug (one-frame empty band in
        // release). `compute_frame_layout` is computed once here and fed to BOTH
        // `resize_panes_to_layout` and `render_frame`, so PTY sizes and rendered
        // rects are guaranteed identical this frame (no second layout pass).
        let _ = terminal.autoresize();
        let full_frame_area = terminal.get_frame().area();
        // PRD #139 M4.1 + PRD #84: throwaway experimental footer, gated at the
        // single user-visible seam (`show_experimental_footer()` — CLAUDE.md #9;
        // grep that name to find this site at graduation). When ON, reserve the
        // bottom row for the `experimental: on` label and lay the rest of the
        // frame out above it; when OFF the layout is byte-for-byte the
        // pre-feature baseline. The reservation happens HERE, before
        // `compute_frame_layout`, so the whole layout pass (and the PTY sizing it
        // drives) sits above the footer. Snapshot the flag ONCE per frame
        // (Greptile P2) so the gate and the footer render observe the same
        // value — reading it twice opened a TOCTOU window where the ~2s watcher
        // could flip it between the reads, cropping the main content by a row.
        let features_snap = crate::features::current();
        let (frame_area, experimental_footer_area) = if features_snap.experimental {
            let chunks = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)])
                .split(full_frame_area);
            (chunks[0], Some(chunks[1]))
        } else {
            (full_frame_area, None)
        };
        let embedded_panes = pane.as_any().downcast_ref::<EmbeddedPaneController>();
        let all_pane_ids = embedded_panes.map(|e| e.pane_ids()).unwrap_or_default();
        let focused_pane_id = embedded_panes.and_then(|e| e.focused_pane_id());
        // PRD #144: the bottom bar wraps to a second row when its full-label
        // buttons don't fit one row, so reserve its actual height up front (the
        // layout pass feeds both the PTY resize and the render this frame).
        let bar_rows = bottom_bar_rows(&ui, frame_area.width, frame_area.height, &tab_view);
        let frame_layout = compute_frame_layout(
            frame_area,
            &tab_view,
            &tab_bar_info,
            &all_pane_ids,
            pane_layout,
            focused_pane_id.as_deref(),
            bar_rows,
        );
        if let Some(embedded) = embedded_panes {
            resize_panes_to_layout(&frame_layout, embedded);
        }
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
                &frame_layout,
            );
            // PRD #139: draw the experimental footer into the reserved bottom
            // row (disjoint from `frame_layout`), using the SAME flag snapshot
            // the reservation above used.
            if let Some(footer_area) = experimental_footer_area {
                render_experimental_footer(frame, &features_snap, footer_area);
            }
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
        // PRD #127 M3.1: deliver any mode `seed_prompt`s whose agent pane has
        // become ready (gated, like orchestrations).
        process_pending_seed_prompts(&mut ui, &pane, &snapshot);

        // Drain all pending events before re-rendering. This avoids a full
        // render cycle between each keystroke, which eliminates perceived typing
        // latency in PaneInput mode.
        if !crossterm::event::poll(std::time::Duration::from_millis(16))? {
            continue;
        }

        // Process events in a tight loop until the queue is empty.
        loop {
            let ev = event::read()?;

            // PRD #84 M4 (invariant 4): a terminal resize is now just a
            // re-render trigger. The pre-draw `resize_panes_to_layout` at the
            // top of the next loop iteration recomputes the layout and commits
            // the new PTY sizes — no PTY work happens in the event handler.
            if let Event::Resize(_w, _h) = ev {
                break; // re-render; layout-driven resize handles PTY sizing
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

                // PRD #80 review FIX 5: when a blocking overlay (any modal, the
                // directory picker, or the new-pane form) is the topmost layer,
                // swallow scroll-wheel events so they don't scroll the pane
                // behind the overlay. Scroll in Normal / PaneInput is left
                // untouched below (pane scroll + child-app forwarding).
                let blocking_overlay = matches!(
                    ui.mode,
                    UiMode::QuitConfirm
                        | UiMode::StopConfirm
                        | UiMode::ConfigGenPrompt
                        | UiMode::StarPrompt
                        | UiMode::Help
                        | UiMode::DirPicker
                        | UiMode::NewPaneForm
                );
                let is_scroll = matches!(
                    mouse.kind,
                    crossterm::event::MouseEventKind::ScrollUp
                        | crossterm::event::MouseEventKind::ScrollDown
                );
                if is_scroll && blocking_overlay {
                    if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }

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
                            // Single vs double click via the shared, region-aware
                            // last_click discrimination (400ms / same row / within
                            // 3 cols / same region — PRD #80 review FIX 4).
                            let now = std::time::Instant::now();
                            let click_count = LastClick::next_count(
                                ui.last_click,
                                now,
                                col,
                                row,
                                ClickRegion::PickerRow,
                            );
                            if is_down {
                                ui.last_click = Some(LastClick {
                                    at: now,
                                    col,
                                    row,
                                    count: click_count,
                                    region: ClickRegion::PickerRow,
                                });
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

                // PRD #80 M8: the new-pane form is a topmost overlay — when
                // open, its [Submit]/[Cancel] buttons, mode chips, and field
                // rows are the only click targets (buttons first, then chips,
                // then field rows), and a miss inside the form is consumed so
                // it never falls through to the dashboard behind it.
                if ui.mode == UiMode::NewPaneForm && (is_down || is_up) {
                    let col = mouse.column;
                    let row = mouse.row;
                    let mut form_action = hit_test_button(&ui.form_button_rects, col, row);
                    if form_action.is_none()
                        && let Some(&(idx, _)) = ui
                            .form_chip_rects
                            .iter()
                            .find(|(_, r)| point_in_rect(r, col, row))
                    {
                        form_action = Some(Action::FormSelectMode(idx));
                    }
                    if form_action.is_none()
                        && let Some(&(field, _)) = ui
                            .form_field_rects
                            .iter()
                            .find(|(_, r)| point_in_rect(r, col, row))
                    {
                        form_action = Some(Action::FormFocusField(field));
                    }
                    if let Some(action) = form_action
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
                    // Consume every Down/Up while the form is open.
                    if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }

                let modal_active = matches!(
                    ui.mode,
                    UiMode::QuitConfirm
                        | UiMode::StopConfirm
                        | UiMode::ConfigGenPrompt
                        | UiMode::StarPrompt
                        | UiMode::Help
                        // PRD #127 finding #4: the Scheduled Tasks dialog is a
                        // topmost modal too — its [Add]/[Edit]/[Delete]/[Run now]
                        // buttons live in `modal_button_rects` and any miss is
                        // consumed here rather than reaching the pane behind it.
                        | UiMode::ScheduledTasks
                );
                // PRD #80 M6: in the inline-edit modes the bottom row IS the
                // input; its [Apply]/[Cancel] / [Save]/[Cancel] buttons live in
                // `button_rects`, and any other click is consumed below so it
                // keeps the field focused instead of exiting the mode.
                let text_input_mode = matches!(ui.mode, UiMode::Filter | UiMode::Rename);

                // PRD #80 M5 + review FIX 1: a modal/overlay is the topmost
                // layer, so its buttons are the ONLY click targets and every
                // Down/Up is consumed here — a miss must NOT fall through to the
                // pane selection/scroll logic behind the popup (which could
                // start a text selection / copy). Matches the picker/form
                // branches above.
                if modal_active && (is_down || is_up) {
                    if let Some(action) =
                        hit_test_button(&ui.modal_button_rects, mouse.column, mouse.row)
                        && is_down
                    {
                        let frame_area = terminal.get_frame().area();
                        let selected_id: Option<String> =
                            dashboard_focus_target(&ui, filtered.len())
                                .and_then(|i| filtered.get(i))
                                .map(|(id, _)| (*id).clone());
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
                    } else if ui.mode == UiMode::ScheduledTasks
                        && is_down
                        && let Some(&(i, _)) = ui
                            .scheduled_row_rects
                            .iter()
                            .find(|(_, r)| point_in_rect(r, mouse.column, mouse.row))
                    {
                        // PRD #127 M3.3: a click on a manager row re-selects it —
                        // keyboard parity for `j`/`k` — mirroring the directory
                        // picker's row hit-test.
                        ui.scheduled_selected = i;
                    }
                    if !crossterm::event::poll(std::time::Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }

                // No modal up: the M2/M3 chain — global button bar first, then
                // the tab affordances. PRD #80 review FIX 2: the tab rects are
                // hit-tested ONLY when NOT in a text-input row mode, so a tab
                // click is inert while Filter/Rename is active (matching the
                // keyboard, where tab-cycling is Normal-mode-only). The global
                // button_rects (which carry the inline-edit Apply/Save/Cancel
                // buttons in those modes) stay active.
                let mouse_action = if !(is_down || is_up) {
                    None
                } else {
                    let mut action = hit_test_button(&ui.button_rects, mouse.column, mouse.row);
                    if action.is_none() && !text_input_mode {
                        action = hit_test_tab_close(&ui.tab_close_rects, mouse.column, mouse.row)
                            .or_else(|| {
                                hit_test_tab_header(&ui.tab_header_rects, mouse.column, mouse.row)
                            });
                    }
                    action
                };
                if let Some(action) = mouse_action {
                    if is_down {
                        let frame_area = terminal.get_frame().area();
                        let selected_id: Option<String> =
                            dashboard_focus_target(&ui, filtered.len())
                                .and_then(|i| filtered.get(i))
                                .map(|(id, _)| (*id).clone());
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
                        // Region-aware double-click discrimination: same row,
                        // within 3 columns, inside the 400ms window, AND the
                        // previous click was also on a card (PRD #80 review
                        // FIX 4).
                        let now = std::time::Instant::now();
                        let click_count = LastClick::next_count(
                            ui.last_click,
                            now,
                            mouse.column,
                            mouse.row,
                            ClickRegion::Card,
                        );
                        ui.last_click = Some(LastClick {
                            at: now,
                            col: mouse.column,
                            row: mouse.row,
                            count: click_count,
                            region: ClickRegion::Card,
                        });

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
                                dashboard_focus_target(&ui, filtered.len())
                                    .and_then(|i| filtered.get(i))
                                    .map(|(id, _)| (*id).clone());
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
                    for (side_id, rect) in ui.side_pane_rects.iter() {
                        if mouse.column >= rect.x
                            && mouse.column < rect.x + rect.width
                            && mouse.row >= rect.y
                            && mouse.row < rect.y + rect.height
                        {
                            let side_id = side_id.clone();
                            if let Tab::Mode {
                                focused_pane_id, ..
                            } = tab_manager.active_tab_mut()
                            {
                                *focused_pane_id = Some(side_id.clone());
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
                            focused_pane_id,
                            agent_pane_id,
                            ..
                        } = tab_manager.active_tab_mut()
                    {
                        *focused_pane_id = None;
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
                                    // to handle slight mouse movement between clicks, AND
                                    // the same click region (PRD #80 review FIX 4) so a
                                    // card/picker click can't seed a pane double-click.
                                    // These coords are pane-relative — a distinct region
                                    // from the screen-coord card/picker clicks.
                                    let now = std::time::Instant::now();
                                    let click_count = LastClick::next_count(
                                        ui.last_click,
                                        now,
                                        col,
                                        row,
                                        ClickRegion::Pane,
                                    );
                                    ui.last_click = Some(LastClick {
                                        at: now,
                                        col,
                                        row,
                                        count: click_count,
                                        region: ClickRegion::Pane,
                                    });

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
                            let was_multiclick =
                                ui.last_click.map(|l| l.count >= 2).unwrap_or(false);
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

            // PRD #40: snapshot the active keybindings for this keypress (cheap
            // HashMap clone; config is immutable for the session) so the mapper
            // blocks below resolve shortcuts from config. `is_ctrl_c` marks the
            // non-overridable quit trigger — it is NEVER mapped to a config
            // action; it falls through to the per-mode handlers (which open the
            // quit flow), so no user binding can hijack the emergency quit.
            let kb = ui.keybindings.clone();
            let is_ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);

            // Jump-to-card in Normal mode (defaults 1..9): focus card N. PRD #40
            // resolves the digits from config so they can be remapped.
            if !is_ctrl_c && ui.mode == UiMode::Normal {
                const JUMP_ACTIONS: [KbAction; 9] = [
                    KbAction::Jump1,
                    KbAction::Jump2,
                    KbAction::Jump3,
                    KbAction::Jump4,
                    KbAction::Jump5,
                    KbAction::Jump6,
                    KbAction::Jump7,
                    KbAction::Jump8,
                    KbAction::Jump9,
                ];
                if let Some(idx) = JUMP_ACTIONS.iter().position(|a| kb.matches(*a, &key)) {
                    action = Some(Action::FocusCard(idx));
                }
            }

            // Global configurable shortcuts (work from any mode). PRD #40:
            // resolved from config (any chord, not just Ctrl+key); `is_ctrl_c`
            // excluded so it can't be hijacked away from the quit flow.
            if action.is_none() && !is_ctrl_c {
                action = global_action(&kb, &key);
            }

            // Tab cycling in Normal mode: move_left/move_right (defaults h/l)
            // plus the non-configurable Tab / Shift+Tab / Left / Right aliases.
            if action.is_none() && !is_ctrl_c && ui.mode == UiMode::Normal {
                action = cycle_tab_action(&kb, &key);
            }

            let selected_id: Option<String> = dashboard_focus_target(&ui, filtered.len())
                .and_then(|i| filtered.get(i))
                .map(|(id, _)| (*id).clone());

            // On a mode tab in Normal mode, move_down/move_up (defaults j/k,
            // plus Down/Up arrows) navigate side panes, Enter focuses, Esc
            // resets. `is_ctrl_c` excluded for the same safety-net reason.
            if action.is_none()
                && !is_ctrl_c
                && ui.mode == UiMode::Normal
                && matches!(tab_manager.active_tab(), Tab::Mode { .. })
            {
                action = mode_tab_nav_action(&kb, &key);
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
                            &kb,
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
                    UiMode::ScheduledTasks => handle_scheduled_tasks_key(key, &mut ui),
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

    // Snapshot the session for auto-restore *before* tearing down mode
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
) -> TabStripRects {
    // PRD #13: the tab-bar row is left unpainted so the terminal's own
    // background shows through (no absolute `tab_bar_bg` fill).

    // Cap long labels so trailing tabs stay at least partially visible for
    // click-to-switch (same width-fitting the `Tabs` widget previously used).
    let fitted_labels = fit_tab_labels(labels, area.width);

    // Inactive tab labels render at full contrast (readable text); the active
    // tab inverts the terminal's own fg/bg in place (Modifier::REVERSED) for a
    // self-contained, single-frame highlight that needs no absolute color. The
    // `│` divider between tabs is decoration, so it stays dim.
    let base_style = text_primary();
    let divider_style = text_dim();
    let active_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);

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
            let (after, _) = buf.set_span(x, area.y, &Span::styled("│", divider_style), end - x);
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

/// PRD #84 M3/M4 — the result of the single per-frame layout pass. Holds every
/// structural rect the render path draws into: the optional tab-bar row, the
/// hints/button-bar row, and the per-tab-variant content rects. `render_frame`
/// and `render_mode_tab` read their rects from here instead of splitting layout
/// inline (contract invariant 1: one layout pass per frame). M4: it also
/// carries each terminal pane's OUTER rect so `resize_panes_to_layout` can
/// derive PTY size from the layout (invariant 2). See
/// `docs/develop/rendering-contract.md`.
struct FrameLayout {
    /// Tab-bar row, present only when the tab strip is shown this frame
    /// (`TabBarInfo::show`).
    tab_bar: Option<Rect>,
    /// Bottom hints / button-bar row.
    hints: Rect,
    /// Per-tab-variant content rects.
    content: FrameContent,
}

/// The content region of a frame, resolved per active-tab variant.
enum FrameContent {
    /// Dashboard / Orchestration: card grid on the left, optional terminal
    /// pane column on the right. `pane_ids` is the filtered, render-ordered set
    /// of panes shown in `panes_area`. `pane_rects` is each pane's OUTER rect
    /// in that column (M4: same `pane_stack_rects` split `render_terminal_panes`
    /// draws into, so the PTY-resize target and the rendered rect can't drift).
    Cards {
        dashboard_area: Rect,
        panes_area: Option<Rect>,
        pane_ids: Vec<String>,
        pane_rects: Vec<(String, Rect)>,
    },
    /// Mode tab: single agent pane (left 50%) and stacked side panes
    /// (right 50%). `side_pane_rects` is the per-side-pane OUTER rect keyed by
    /// pane id, in render order — the source for `ui.side_pane_rects` (scroll +
    /// click hit-testing) and (M4) the side-pane PTY-resize targets.
    /// `agent_pane_id` keys the agent pane, whose OUTER rect is `agent_area`.
    Mode {
        agent_area: Rect,
        side_area: Rect,
        agent_pane_id: String,
        side_pane_rects: Vec<(String, Rect)>,
    },
}

impl FrameLayout {
    /// PRD #84 M4 — every local terminal pane's target PTY size `(rows, cols)`
    /// keyed by pane id, derived from its layout rect: the bordered
    /// `TerminalWidget`'s inner content area is the OUTER rect shrunk by the
    /// 1-cell border on each side, so `(rows, cols) = (h - 2, w - 2)`. This is
    /// the single source `resize_panes_to_layout` drives PTYs from (invariant
    /// 2). Collapsed Stacked panes resolve to a zero dimension and are filtered
    /// by the caller.
    fn pane_target_dims(&self) -> Vec<(&str, u16, u16)> {
        // Inner area of a bordered pane = outer minus 1 cell on each side.
        let dims = |rect: Rect| (rect.height.saturating_sub(2), rect.width.saturating_sub(2));
        let mut out = Vec::new();
        match &self.content {
            FrameContent::Cards { pane_rects, .. } => {
                for (id, rect) in pane_rects {
                    let (rows, cols) = dims(*rect);
                    out.push((id.as_str(), rows, cols));
                }
            }
            FrameContent::Mode {
                agent_area,
                agent_pane_id,
                side_pane_rects,
                ..
            } => {
                if !agent_pane_id.is_empty() {
                    let (rows, cols) = dims(*agent_area);
                    out.push((agent_pane_id.as_str(), rows, cols));
                }
                for (id, rect) in side_pane_rects {
                    let (rows, cols) = dims(*rect);
                    out.push((id.as_str(), rows, cols));
                }
            }
        }
        out
    }
}

/// PRD #84 M3/M4 — the single per-frame layout pass. Given the frame area, the
/// active tab view, the tab-bar snapshot, the full set of embedded pane ids,
/// the active `PaneLayout`, and the controller's focused pane, produce every
/// structural rect the render path draws into — including each terminal pane's
/// OUTER rect (M4, used to size PTYs). The split math mirrors
/// `render_frame` / `render_mode_tab` / `render_terminal_panes` exactly, so the
/// rendered output and the resize target agree by construction.
fn compute_frame_layout(
    frame_area: Rect,
    tab_view: &ActiveTabView,
    tab_bar: &TabBarInfo,
    all_pane_ids: &[String],
    pane_layout: PaneLayout,
    focused_pane_id: Option<&str>,
    bottom_bar_rows: u16,
) -> FrameLayout {
    // PRD #144: the bottom bar may WRAP to more than one row, so reserve its
    // ACTUAL height (`bottom_bar_rows`, at least 1) — the main content area
    // above shrinks by exactly that, preventing card/pane clip or overlap.
    let bar_rows = bottom_bar_rows.max(1);
    // Vertical: optional tab bar at top, main content, hints bar at bottom.
    let (tab_bar_rect, main_area, hints) = if tab_bar.show {
        let chunks = Layout::vertical([
            Constraint::Length(1),        // tab bar
            Constraint::Fill(1),          // main content
            Constraint::Length(bar_rows), // hints bar (1 or more wrapped rows)
        ])
        .split(frame_area);
        (Some(chunks[0]), chunks[1], chunks[2])
    } else {
        let chunks =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(bar_rows)]).split(frame_area);
        (None, chunks[0], chunks[1])
    };

    let content = match tab_view {
        ActiveTabView::Mode {
            agent_pane_id,
            side_pane_ids,
            ..
        } => {
            // 50/50 horizontal split: agent pane left, side panes right.
            let chunks =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(main_area);
            let agent_area = chunks[0];
            let side_area = chunks[1];
            // Side-pane rects: the same Tiled vertical split
            // `render_terminal_panes` draws the side panes into (and the source
            // for `ui.side_pane_rects`). Skipped when there are no side panes
            // (the old code only pushed these inside its non-empty guard).
            let side_pane_rects: Vec<(String, Rect)> = if side_pane_ids.is_empty() {
                Vec::new()
            } else {
                let chunks =
                    pane_stack_rects(side_area, side_pane_ids, PaneLayout::Tiled, focused_pane_id);
                side_pane_ids.iter().cloned().zip(chunks).collect()
            };
            FrameContent::Mode {
                agent_area,
                side_area,
                agent_pane_id: agent_pane_id.clone(),
                side_pane_rects,
            }
        }
        ActiveTabView::Dashboard { exclude_pane_ids } => {
            let pane_ids: Vec<String> = all_pane_ids
                .iter()
                .filter(|&id| !exclude_pane_ids.contains(id))
                .cloned()
                .collect();
            let (dashboard_area, panes_area) = split_cards_area(
                main_area,
                &pane_ids,
                DASHBOARD_LEFT_PERCENT,
                DASHBOARD_PANES_PERCENT,
            );
            let pane_rects = cards_pane_rects(panes_area, &pane_ids, pane_layout, focused_pane_id);
            FrameContent::Cards {
                dashboard_area,
                panes_area,
                pane_ids,
                pane_rects,
            }
        }
        ActiveTabView::Orchestration { role_pane_ids, .. } => {
            let pane_ids: Vec<String> = all_pane_ids
                .iter()
                .filter(|&id| role_pane_ids.contains(id))
                .cloned()
                .collect();
            let (dashboard_area, panes_area) = split_cards_area(
                main_area,
                &pane_ids,
                ORCHESTRATION_LEFT_PERCENT,
                ORCHESTRATION_PANES_PERCENT,
            );
            let pane_rects = cards_pane_rects(panes_area, &pane_ids, pane_layout, focused_pane_id);
            FrameContent::Cards {
                dashboard_area,
                panes_area,
                pane_ids,
                pane_rects,
            }
        }
    };

    FrameLayout {
        tab_bar: tab_bar_rect,
        hints,
        content,
    }
}

/// Per-pane OUTER rects for the dashboard / orchestration right pane column,
/// keyed by pane id. `None` (no panes) yields an empty list; otherwise the
/// `pane_stack_rects` split that `render_terminal_panes` draws the panes into.
fn cards_pane_rects(
    panes_area: Option<Rect>,
    pane_ids: &[String],
    pane_layout: PaneLayout,
    focused_pane_id: Option<&str>,
) -> Vec<(String, Rect)> {
    match panes_area {
        Some(area) if !pane_ids.is_empty() => {
            let chunks = pane_stack_rects(area, pane_ids, pane_layout, focused_pane_id);
            pane_ids.iter().cloned().zip(chunks).collect()
        }
        _ => Vec::new(),
    }
}

/// Split a dashboard / orchestration main area into the left card grid and the
/// optional right pane column. With no panes the whole area is the grid (no
/// right column); otherwise a `[left_percent, panes_percent]` horizontal split.
fn split_cards_area(
    main_area: Rect,
    pane_ids: &[String],
    left_percent: u16,
    panes_percent: u16,
) -> (Rect, Option<Rect>) {
    if pane_ids.is_empty() {
        (main_area, None)
    } else {
        let chunks = Layout::horizontal([
            Constraint::Percentage(left_percent),
            Constraint::Percentage(panes_percent),
        ])
        .split(main_area);
        (chunks[0], Some(chunks[1]))
    }
}

/// PRD #84 — which pane index is the expanded (full-height) slot in a `Stacked`
/// pane column: the focused pane, or — when nothing in the stack is focused —
/// the first pane. `None` only for an empty stack. Single source of truth so
/// the rect split and `render_terminal_panes`' per-pane render agree on which
/// slot expands.
fn stacked_expanded_index(pane_ids: &[String], focused_id: Option<&str>) -> Option<usize> {
    if let Some(i) = pane_ids
        .iter()
        .position(|id| focused_id == Some(id.as_str()))
    {
        Some(i)
    } else if pane_ids.is_empty() {
        None
    } else {
        Some(0)
    }
}

/// PRD #84 — the per-pane OUTER rects for a vertical terminal stack, matching
/// exactly how `render_terminal_panes` lays panes out for the given
/// `PaneLayout` and resolved focus. Single source of truth so the layout pass
/// (which drives PTY resize) and the renderer can't disagree on a pane's rect.
/// `Tiled`: equal vertical division. `Stacked`: the expanded slot fills, every
/// other pane collapses to a 1-row title bar.
fn pane_stack_rects(
    area: Rect,
    pane_ids: &[String],
    layout: PaneLayout,
    focused_id: Option<&str>,
) -> Vec<Rect> {
    if pane_ids.is_empty() {
        return Vec::new();
    }
    let constraints: Vec<Constraint> = match layout {
        PaneLayout::Tiled => pane_ids
            .iter()
            .map(|_| Constraint::Ratio(1, pane_ids.len() as u32))
            .collect(),
        PaneLayout::Stacked => {
            // Focused pane gets remaining space; unfocused get a single
            // collapsed title row (`title_bar_height = 1`).
            let expanded = stacked_expanded_index(pane_ids, focused_id);
            pane_ids
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    if expanded == Some(i) {
                        Constraint::Fill(1)
                    } else {
                        Constraint::Length(1)
                    }
                })
                .collect()
        }
    };
    Layout::vertical(constraints).split(area).to_vec()
}

/// PRD #84 M4 (invariant 2) — derive each local pane's PTY size from its layout
/// rect and commit only the deltas. Runs once per frame, after
/// `compute_frame_layout` and before `terminal.draw`, so every layout-changing
/// path (resize, tab open/close, mode switch, reactive pane recreation,
/// orchestration role transition) converges here instead of pushing its own
/// `resize_pane_pty` from a private dimension calculation.
///
/// A pane whose target inner area has a zero dimension (a collapsed Stacked
/// slot, or a viewport too small for the border) is skipped — matching the old
/// helpers' `rows > 0 && cols > 0` guard. `resize_pane_pty` is the one resize
/// primitive and handles local vs stream-backed panes itself (stream panes
/// coalesce to the daemon; see `embedded_pane.rs`), so no per-backend
/// special-casing is needed here.
fn resize_panes_to_layout(layout: &FrameLayout, embedded: &EmbeddedPaneController) {
    for (pane_id, rows, cols) in layout.pane_target_dims() {
        if rows == 0 || cols == 0 {
            continue;
        }
        // Compare against the pane's current parser size (kept in lockstep with
        // the PTY by `resize_pane_pty`) and only commit a real delta, so a
        // steady frame issues no resize traffic.
        let current = embedded.get_screen(pane_id).and_then(|arc| {
            let parser = arc.lock().ok()?;
            Some(parser.screen().size())
        });
        if current != Some((rows, cols)) {
            let _ = embedded.resize_pane_pty(pane_id, rows, cols);
        }
    }
}

/// PRD #155 (M3): build the per-frame join from each managed pane id to its
/// agent status, read from the SAME source the deck cards use
/// (`state.sessions[*].status`). The result keys an embedded pane's border to
/// the centralized-palette status color so deck cards and panes agree
/// (criterion #2).
///
/// Only sessions that own a pane (`pane_id.is_some()`) appear — pane-less
/// sessions are excluded, since there is no pane to color. Borrowed `&str` keys
/// avoid a per-frame clone of every pane id; the status enum clone is cheap
/// (fieldless). Extracted from `render_frame` so the join can be unit-tested
/// without a live daemon.
pub(crate) fn build_pane_status(state: &AppState) -> HashMap<&str, SessionStatus> {
    state
        .sessions
        .values()
        .filter_map(|s| s.pane_id.as_deref().map(|pid| (pid, s.status.clone())))
        .collect()
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
    layout: &FrameLayout,
) {
    // PRD #84 + #139: `render_frame` reads the precomputed `FrameLayout` (one
    // layout pass per frame). The PRD #139 experimental-footer row is reserved
    // in the main loop BEFORE `compute_frame_layout` (so the whole layout, and
    // the PTY sizing it drives, sits above the footer) and the footer itself is
    // drawn there in the same `terminal.draw` — see the gating block around the
    // `compute_frame_layout` call. So there is nothing footer-related to do here.
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

    // PRD #13: the canvas is left unpainted so the terminal's own background
    // shows through (`Color::Reset`). Filling it with an absolute color was
    // the original light-terminal bug — a black slab over a light theme.

    // Determine if we have embedded terminal panes to show on the right.
    let embedded = pane_controller
        .as_any()
        .downcast_ref::<EmbeddedPaneController>();

    // PRD #84 M3/M4 — one layout pass per frame (invariant 1). The single
    // `FrameLayout` is computed once by the caller from the live frame area and
    // fed to BOTH the pre-draw `resize_panes_to_layout` (PTY sizing) and this
    // render pass, so the rects drawn here are exactly the PTY-resize targets.
    // This function only reads from it; it never splits layout itself. See
    // `docs/develop/rendering-contract.md`.
    let hints_area = layout.hints;

    // Tab strip: render into the bar rect the layout pass reserved, or drop any
    // stale click rects when no strip is shown this frame.
    if let Some(tab_bar_rect) = layout.tab_bar {
        // PRD #80 M3: the Dashboard tab (always index 0) carries no close
        // affordance; Mode and Orchestration tabs do. Pass the per-tab
        // closeable mask to the tab-strip renderer, and record the clickable
        // header / [×] rects for the mouse hit-test. PRD #13: the strip is
        // terminal-relative — the active tab is cued with Modifier::REVERSED,
        // not an absolute background tint.
        let closeable: Vec<bool> = (0..tab_bar.labels.len()).map(|i| i != 0).collect();
        let strip = render_tab_strip(
            frame,
            tab_bar_rect,
            &tab_bar.labels,
            &closeable,
            tab_bar.active_index,
        );
        ui.tab_header_rects = strip.headers;
        ui.tab_close_rects = strip.closes;
    } else {
        // No tab strip this frame — drop any stale rects so a click can't hit a
        // tab affordance that isn't on screen.
        ui.tab_header_rects.clear();
        ui.tab_close_rects.clear();
    }

    // PRD #155 (M3): map each managed pane to its agent status from the SAME
    // source the deck cards read (`state.sessions[*].status`), so an embedded
    // pane's border encodes its status with the SAME centralized-palette color
    // the deck card uses — closing the deck/pane consistency gap (criterion #2).
    // Built once and threaded into every `render_terminal_panes` call below (and
    // into `render_mode_tab`). Extracted into `build_pane_status` so the join can
    // be unit-tested without a live daemon.
    let pane_status: HashMap<&str, SessionStatus> = build_pane_status(state);

    // Branch on the content the layout pass resolved. Mode tabs render and
    // return here; dashboard / orchestration fall through to the card grid.
    let (dashboard_area, panes_area, pane_ids, pane_rects) = match &layout.content {
        FrameContent::Mode {
            agent_area,
            side_area,
            side_pane_rects,
            ..
        } => {
            let ActiveTabView::Mode {
                agent_pane_id,
                side_pane_ids,
                focused_pane_id,
                ..
            } = tab_view
            else {
                unreachable!("FrameContent::Mode is produced only for ActiveTabView::Mode")
            };
            render_mode_tab(
                frame,
                ui,
                embedded,
                agent_pane_id,
                side_pane_ids,
                *agent_area,
                *side_area,
                side_pane_rects,
                hints_area,
                has_pane_control,
                active_mode_name,
                focused_pane_id.as_deref(),
                &pane_status,
            );
            return;
        }
        FrameContent::Cards {
            dashboard_area,
            panes_area,
            pane_ids,
            pane_rects,
        } => (*dashboard_area, *panes_area, pane_ids, pane_rects),
    };

    // PRD #84: the OUTER rect each pane in the right column was sized to this
    // frame, in `pane_ids` order — threaded into `render_terminal_panes` so it
    // draws into exactly these (single source of truth with the PTY resize).
    let pane_outer_rects: Vec<Rect> = pane_rects.iter().map(|(_, rect)| *rect).collect();

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
        .style(text_primary())
        .centered();
        frame.render_widget(msg, vertical[1]);
        let ctx_buttons = dashboard_context_buttons(&ui.keybindings, !filtered.is_empty());
        render_bottom_bar(frame, ui, hints_area, has_pane_control, &ctx_buttons);

        if let Some(right) = panes_area {
            ui.focused_pane_rect = render_terminal_panes(
                frame,
                embedded,
                right,
                pane_ids,
                pane_layout,
                &ui.pane_display_names,
                &pane_status,
                &ui.selection,
                None,
                Some(&pane_outer_rects),
            );
        }

        render_overlays(frame, ui, active_mode_name);
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
        Span::styled(title_text, text_primary()),
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
            .style(text_primary())
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
        );
        let ctx_buttons = dashboard_context_buttons(&ui.keybindings, !filtered.is_empty());
        render_bottom_bar(frame, ui, hints_area, has_pane_control, &ctx_buttons);
        // Still render live terminal panes even when filter matches zero sessions.
        if let Some(right) = panes_area {
            ui.focused_pane_rect = render_terminal_panes(
                frame,
                embedded,
                right,
                pane_ids,
                pane_layout,
                &ui.pane_display_names,
                &pane_status,
                &ui.selection,
                None,
                Some(&pane_outer_rects),
            );
        }
        render_overlays(frame, ui, active_mode_name);
        return;
    }

    let all_rows: Vec<&[&SessionState]> = sessions.chunks(cols).collect();
    let all_row_ids: Vec<&[&String]> = session_ids.chunks(cols).collect();
    let total_rows = all_rows.len();

    // Calculate how many rows fit in the available area
    let visible_rows = (available_for_density / card_height).max(1) as usize;

    // Adjust scroll offset to keep selected row visible. PRD #113: only when a
    // card is actively highlighted — an inactive selection (`None`) leaves the
    // scroll position alone.
    if let Some(sel) = ui.selected_index {
        let selected_row = sel / cols;
        if selected_row < ui.scroll_offset {
            ui.scroll_offset = selected_row;
        } else if selected_row >= ui.scroll_offset + visible_rows {
            ui.scroll_offset = selected_row + 1 - visible_rows;
        }
    }

    // Re-clamp after a resize may have grown `visible_rows`: an offset left over
    // from a previous overflow state must shrink so the last card row still sits
    // at the bottom (no scrolled-off top / blank tail). Only reduces an
    // over-large offset; legitimate scrolling is unchanged.
    ui.scroll_offset = clamp_scroll_offset(ui.scroll_offset, total_rows, visible_rows);

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
            let is_selected = ui.selected_index == Some(flat_index);
            // PRD #127 finding #2: `ui.display_names` is populated by hydration
            // and explicit renames; a live scheduler-spawned card has no entry
            // there, so fall back to the friendly name the synthetic
            // `SessionStart` carried onto `SessionState.display_name`. Without
            // this the live card degraded to the truncated pane id while a
            // reconnect (which reads the daemon registry's display_name into
            // `ui.display_names`) titled it correctly.
            let display_name = ids
                .get(col_idx)
                .and_then(|id| ui.display_names.get(*id))
                .or(session.display_name.as_ref());
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
    );

    // Full-width hints bar
    let ctx_buttons = dashboard_context_buttons(&ui.keybindings, !filtered.is_empty());
    render_bottom_bar(frame, ui, hints_area, has_pane_control, &ctx_buttons);

    // Render terminal panes on the right side
    if let Some(right) = panes_area {
        ui.focused_pane_rect = render_terminal_panes(
            frame,
            embedded,
            right,
            pane_ids,
            pane_layout,
            &ui.pane_display_names,
            &pane_status,
            &ui.selection,
            None,
            Some(&pane_outer_rects),
        );
    }

    render_overlays(frame, ui, active_mode_name);
}

fn render_overlays(frame: &mut Frame, ui: &mut UiState, active_mode_name: Option<&str>) {
    // PRD #80 M5/M7: rebuilt below for whichever modal/overlay is shown;
    // cleared here so a click can't hit an affordance from a prior frame once
    // the overlay closes.
    ui.modal_button_rects.clear();
    ui.picker_button_rects.clear();
    ui.picker_row_rects.clear();
    ui.scheduled_row_rects.clear();
    ui.form_field_rects.clear();
    ui.form_chip_rects.clear();
    ui.form_button_rects.clear();
    if ui.mode == UiMode::Help {
        ui.modal_button_rects = render_help_overlay(frame, &ui.keybindings, active_mode_name);
    }
    if ui.mode == UiMode::DirPicker {
        // Capture the picker's row/button rects after the `dir_picker` borrow
        // ends so they can be stored back on `ui`.
        let captured = ui
            .dir_picker
            .as_mut()
            .map(|picker| render_dir_picker(frame, picker));
        if let Some((rows, buttons)) = captured {
            ui.picker_row_rects = rows;
            ui.picker_button_rects = buttons;
        }
    }
    if ui.mode == UiMode::NewPaneForm {
        // Capture the form's field/chip/button rects after the `new_pane_form`
        // borrow ends so they can be stored back on `ui`.
        let captured = ui
            .new_pane_form
            .as_ref()
            .map(|form| render_new_pane_form(frame, form));
        if let Some((fields, chips, buttons)) = captured {
            ui.form_field_rects = fields;
            ui.form_chip_rects = chips;
            ui.form_button_rects = buttons;
        }
    }
    if ui.mode == UiMode::ScheduledTasks {
        let (button_rects, row_rects) = render_scheduled_tasks(frame, ui);
        ui.modal_button_rects = button_rects;
        ui.scheduled_row_rects = row_rects;
    }
    if ui.mode == UiMode::StarPrompt {
        ui.modal_button_rects = render_star_prompt(frame);
    }
    if ui.mode == UiMode::ConfigGenPrompt {
        ui.modal_button_rects = render_config_gen_prompt(frame, ui.config_gen_selected);
    }
    if ui.mode == UiMode::QuitConfirm {
        ui.modal_button_rects = render_quit_confirm(frame, ui.quit_confirm_selected);
    }
    if ui.mode == UiMode::StopConfirm {
        // M5 adds no buttons to the secondary Stop-confirm dialog (not in the
        // contract); its keystrokes (y/n/Enter/Esc) remain the only path.
        render_stop_confirm(frame, ui.stop_confirm_selected, ui.stop_confirm_agent_count);
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
    // PRD #155 (M3): per-pane agent status keyed by pane_id, sourced from the
    // SAME `state.sessions[*].status` the deck cards render through
    // `status_style`. Threaded so a non-focused pane's border encodes its status
    // via the centralized `palette` — the SAME color the deck card uses for that
    // state (criterion #2). A pane absent from the map (no backing session)
    // keeps the legacy focus-only border (the `TerminalWidget::new` default).
    pane_status: &HashMap<&str, SessionStatus>,
    selection: &Option<TextSelection>,
    visual_focus_id: Option<&str>,
    // PRD #84: per-pane OUTER rects (aligned 1:1 with `pane_ids`) precomputed by
    // `compute_frame_layout` — the SAME rects `resize_panes_to_layout` sized the
    // PTYs to this frame. `Some` => draw into exactly those; `None` => recompute
    // via `pane_stack_rects` (used by callers without a `FrameLayout` rect list,
    // e.g. the mode-tab agent / side panes).
    precomputed_rects: Option<&[Rect]>,
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

    // PRD #84: the per-pane OUTER rects are the single source of truth shared
    // with `resize_panes_to_layout`. When the caller threads in the exact
    // `FrameLayout` rects the PTYs were sized to (the Cards render path), draw
    // into THOSE — the drawn rect is the resized rect, no second
    // `pane_stack_rects` pass, no correctness-by-timing assumption. Callers
    // without a `FrameLayout` rect list recompute via the same helper.
    let computed_chunks;
    let chunks: &[Rect] = match precomputed_rects {
        Some(rects) => {
            debug_assert_eq!(
                rects.len(),
                pane_ids.len(),
                "precomputed pane rects must align 1:1 with pane_ids"
            );
            rects
        }
        None => {
            computed_chunks = pane_stack_rects(area, pane_ids, layout, focused_id.as_deref());
            &computed_chunks
        }
    };
    match layout {
        PaneLayout::Tiled => {
            for (i, pane_id) in pane_ids.iter().enumerate() {
                if let Some(screen) = ctrl.get_screen(pane_id) {
                    let focused = focused_id.as_deref() == Some(pane_id.as_str());
                    let title = pane_name(pane_id);
                    // PRD #84 M5: this pane was sized to `chunks[i]` by
                    // `resize_panes_to_layout` this frame, so attest the contract.
                    let mut widget = TerminalWidget::new(Arc::clone(&screen), title, focused)
                        .contract_guaranteed(true);
                    // PRD #155 (M3): supply the pane's status so a non-focused
                    // pane renders its status-colored border via the palette (the
                    // focus override stays inside TerminalWidget). Panes without a
                    // backing session keep the legacy focus-only border.
                    if let Some(status) = pane_status.get(pane_id.as_str()) {
                        widget = widget.with_status(status.clone());
                    }
                    if focused {
                        focused_pane_rect = Some(chunks[i]);
                        focused_screen = Some(screen);
                    }
                    frame.render_widget(widget, chunks[i]);
                }
            }
        }
        PaneLayout::Stacked => {
            // Focused pane gets remaining space; unfocused get a single
            // collapsed title row. `stacked_expanded_index` resolves which slot
            // expands (focused, else first) — the same decision the split used.
            let focused_idx = stacked_expanded_index(pane_ids, focused_id.as_deref());
            for (i, pane_id) in pane_ids.iter().enumerate() {
                let is_expanded = focused_idx == Some(i);
                let title = pane_name(pane_id);
                if is_expanded {
                    if let Some(screen) = ctrl.get_screen(pane_id) {
                        let is_focused = focused_id.as_deref() == Some(pane_id.as_str());
                        // PRD #84 M5: the expanded pane was sized to `chunks[i]`
                        // by `resize_panes_to_layout` this frame — attest it.
                        let mut widget =
                            TerminalWidget::new(Arc::clone(&screen), title, is_focused)
                                .contract_guaranteed(true);
                        // PRD #155 (M3): same status threading as the Tiled arm.
                        if let Some(status) = pane_status.get(pane_id.as_str()) {
                            widget = widget.with_status(status.clone());
                        }
                        if is_focused {
                            focused_pane_rect = Some(chunks[i]);
                            focused_screen = Some(screen);
                        }
                        frame.render_widget(widget, chunks[i]);
                    }
                } else {
                    // Collapsed: show a titled border block. PRD #155 (M3): a
                    // collapsed pane is by definition not the expanded/focused
                    // slot, so the unified Option-A precedence resolves its
                    // border to the agent's STATUS color — the SAME palette role
                    // the Tiled and expanded-Stacked arms apply via
                    // `with_status`, so a given state looks identical across all
                    // embedded-pane contexts (criterion #2). Panes without a
                    // backing session status keep the dimmed fallback.
                    let border_style = pane_status
                        .get(pane_id.as_str())
                        .map(|status| Style::default().fg(palette::status_color(status)))
                        .unwrap_or_else(text_dim);
                    let block = Block::default()
                        .borders(Borders::TOP)
                        .border_style(border_style)
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
///
/// PRD #84 M3: the 50/50 split and the side-pane hit-test rects are computed by
/// `compute_frame_layout` and passed in (`agent_area`, `side_area`,
/// `side_pane_rects`); this function no longer splits layout itself.
#[allow(clippy::too_many_arguments)]
fn render_mode_tab(
    frame: &mut Frame,
    ui: &mut UiState,
    embedded: Option<&EmbeddedPaneController>,
    agent_pane_id: &str,
    side_pane_ids: &[String],
    agent_area: Rect,
    side_area: Rect,
    side_pane_rects: &[(String, Rect)],
    hints_area: Rect,
    has_pane_control: bool,
    active_mode_name: Option<&str>,
    focused_pane_id: Option<&str>,
    // PRD #155 (M3): per-pane agent status (pane_id → status) forwarded from
    // `render_frame` into both `render_terminal_panes` calls below, so mode-tab
    // panes get the same status-colored borders as the dashboard's panes.
    pane_status: &HashMap<&str, SessionStatus>,
) {
    // PRD #83: `focused_pane_id` is keyed by stable pane id. `None` (or an
    // id that isn't one of this tab's side panes) means the agent pane is
    // focused; otherwise the matching side pane is the visually focused one.
    let side_visual_focus: Option<String> = focused_pane_id
        .filter(|id| side_pane_ids.iter().any(|s| s == id))
        .map(|id| id.to_string());
    let agent_visual_focus: Option<&str> = if side_visual_focus.is_none() {
        Some(agent_pane_id)
    } else {
        None
    };

    // Track agent pane rect for click-to-focus (sourced from FrameLayout).
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
            pane_status,
            &ui.selection,
            agent_visual_focus,
            // Mode-tab panes recompute their rects (out of the Cards finding's
            // scope); the agent pane is a single Stacked pane filling agent_area.
            None,
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
            pane_status,
            &ui.selection,
            side_visual_focus.as_deref(),
            // Mode side panes recompute via pane_stack_rects (Tiled) — same
            // split as the `side_pane_rects` above; out of the Cards finding's scope.
            None,
        );
        // Use side pane rect when a side pane is visually focused, or as fallback.
        if side_visual_focus.is_some() || ui.focused_pane_rect.is_none() {
            ui.focused_pane_rect = rect;
        }

        // Track side pane rects for scroll / click hit-testing — sourced from
        // the FrameLayout pass (same vertical split that draws them).
        ui.side_pane_rects.extend(side_pane_rects.iter().cloned());
    }

    // Full-width hints bar — mode tabs show only the global buttons (no
    // dashboard context buttons).
    render_bottom_bar(frame, ui, hints_area, has_pane_control, &[]);

    render_overlays(frame, ui, active_mode_name);
}

fn render_stats_bar(
    frame: &mut Frame,
    stats: &DashboardStats,
    area: Rect,
    active_mode_name: Option<&str>,
) {
    let mut spans: Vec<Span> = Vec::new();

    // Always show active count
    spans.push(Span::styled(
        format!(" {} active", stats.active),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    // Semantic statuses resolve their color through `palette::status_color` —
    // the single source of truth shared with the deck-card and embedded-pane
    // borders (PRD #155). This is why `compacting` shows Blue (it shares the
    // thinking role) and never reuses the `selected` Magenta accent. Idle has no
    // accent color, but its count is neutral text the user reads (like the tools
    // count), so per the PRD #13 readability decision it renders at full contrast
    // via text_primary() rather than the dimmed idle role. Only decoration — the
    // `│` separators below — stays text_dim().
    let segments: &[(usize, &str, Style)] = &[
        (
            stats.working,
            "working",
            Style::default().fg(palette::status_color(&SessionStatus::Working)),
        ),
        (
            stats.thinking,
            "thinking",
            Style::default().fg(palette::status_color(&SessionStatus::Thinking)),
        ),
        (
            stats.compacting,
            "compacting",
            Style::default().fg(palette::status_color(&SessionStatus::Compacting)),
        ),
        (
            stats.waiting,
            "waiting",
            Style::default().fg(palette::status_color(&SessionStatus::WaitingForInput)),
        ),
        (
            stats.errors,
            "error",
            Style::default().fg(palette::status_color(&SessionStatus::Error)),
        ),
        (stats.idle, "idle", text_primary()),
    ];

    for &(count, label, style) in segments {
        if count > 0 {
            spans.push(Span::styled("  \u{2502}  ", text_dim()));
            spans.push(Span::styled(format!("{count} {label}"), style));
        }
    }

    // Always show total tools
    spans.push(Span::styled("  \u{2502}  ", text_dim()));
    spans.push(Span::styled(
        format!("{} tools", stats.total_tools),
        text_primary(),
    ));

    if let Some(name) = active_mode_name {
        spans.push(Span::styled("  \u{2502}  ", text_dim()));
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
/// PRD #40: format a keybinding's notation for a BUTTON label. Buttons use the
/// prd-80 convention of `Ctrl+<UPPERCASE letter>` (e.g. `Ctrl+N`), while
/// `notation()` emits lowercase (`Ctrl+n`). Uppercase ONLY the trailing single
/// ASCII letter of a modifier combo (one containing `+`); bare keys (`r`, `g`,
/// `?`, `/`) and non-letter tails are left untouched. So under the default
/// config every button label is byte-for-byte identical to the old hardcoded
/// strings, while a remap (e.g. `new_pane = "Alt+p"`) renders `Alt+P`. An
/// unbound action renders `(unbound)` (via `display_notation`).
fn button_shortcut_label(keybindings: &KeybindingConfig, action: KbAction) -> String {
    let s = display_notation(keybindings, action);
    if let Some(plus) = s.rfind('+') {
        let (head, tail) = s.split_at(plus + 1);
        let mut tail_chars = tail.chars();
        if let Some(c) = tail_chars.next()
            && tail_chars.next().is_none()
            && c.is_ascii_alphabetic()
        {
            return format!("{head}{}", c.to_ascii_uppercase());
        }
    }
    s
}

fn global_bar_buttons(keybindings: &KeybindingConfig, has_pane_control: bool) -> Vec<Button> {
    vec![
        // PRD #80 review FIX 3: New Pane is ALWAYS enabled — you can always
        // create the first pane, even with no panes / controller yet.
        Button::new(
            "New Pane",
            button_shortcut_label(keybindings, KbAction::NewPane),
            Action::NewPane,
            true,
        ),
        Button::new(
            "Close",
            button_shortcut_label(keybindings, KbAction::ClosePane),
            Action::CloseSelected,
            has_pane_control,
        ),
        Button::new(
            "Toggle Layout",
            button_shortcut_label(keybindings, KbAction::ToggleLayout),
            Action::ToggleLayout,
            has_pane_control,
        ),
        Button::new(
            "Help",
            button_shortcut_label(keybindings, KbAction::Help),
            Action::ToggleHelp,
            true,
        ),
        // Quit is the non-overridable Ctrl+C modal trigger — not a remappable
        // KbAction, so its label stays fixed.
        Button::new("Quit", "Ctrl+C", Action::Quit, true),
    ]
}

/// PRD #80 M4: the dashboard-only context buttons appended to the global bar
/// while on the dashboard in Normal mode, each carrying its (live) inline
/// shortcut. Filter is always actionable; Rename / Generate-config act on the
/// selected card, so they're disabled (dimmed) when there are no cards —
/// matching the keys' `total > 0` guard.
fn dashboard_context_buttons(keybindings: &KeybindingConfig, has_cards: bool) -> Vec<Button> {
    let mut buttons = vec![
        Button::new(
            "Filter",
            button_shortcut_label(keybindings, KbAction::Filter),
            Action::EnterFilter,
            true,
        ),
        Button::new(
            "Rename",
            button_shortcut_label(keybindings, KbAction::Rename),
            Action::EnterRename,
            has_cards,
        ),
        Button::new(
            "Generate",
            button_shortcut_label(keybindings, KbAction::GenerateConfig),
            Action::RequestConfigGen,
            has_cards,
        ),
    ];
    // PRD #80 / PRD #127 finding #4 / PRD #144: the `[Scheduled Tasks s]` open
    // button is always shown — it opens the manager, which is itself how you
    // CREATE the first schedule (its `[Add a]` action works on an empty list),
    // so gating it on a non-empty schedule list would hide the only entry
    // point. The notation is folded into `label` with an EMPTY shortcut field;
    // since PRD #144 the bar always renders every button's full label and wraps
    // any overflow onto a fresh row (no shortcut-only chips), so this renders
    // `[Scheduled Tasks s]` in full like every other button. The notation is
    // config-derived so a remap of `open_scheduled_tasks` is reflected.
    buttons.push(Button::new(
        format!(
            "Scheduled Tasks {}",
            display_notation(keybindings, KbAction::OpenScheduledTasks)
        ),
        "",
        Action::OpenScheduledTasks,
        true,
    ));
    buttons
}

/// PRD #144 — greedy line-wrap for the bottom button bar. Lay the buttons out
/// left-to-right separated by one space, keeping every button's FULL label;
/// when the next button would overflow `width`, wrap it to a fresh row rather
/// than collapsing to shortcut-only chips (the pre-#144 degradation). Returns
/// one `Rect` per input width — `x` in `[0, width)`, `y` the 0-based row,
/// `height` 1 — in input order, plus the total number of rows occupied (0 for
/// an empty input). A button wider than `width` is still placed at the start of
/// its own row (the caller clips it to the buffer). Pure data: no rendering, so
/// the same layout drives the render AND the reserved height budget.
fn layout_button_bar(label_widths: &[u16], width: u16) -> (Vec<Rect>, u16) {
    const SEP: u16 = 1;
    let mut rects = Vec::with_capacity(label_widths.len());
    let mut x = 0u16;
    let mut y = 0u16;
    let mut row_has_button = false;
    for &w in label_widths {
        if row_has_button && x.saturating_add(SEP).saturating_add(w) > width {
            // Would overflow the current row — wrap to a fresh one.
            y += 1;
            x = 0;
            row_has_button = false;
        }
        if row_has_button {
            x = x.saturating_add(SEP);
        }
        rects.push(Rect {
            x,
            y,
            width: w,
            height: 1,
        });
        x = x.saturating_add(w);
        row_has_button = true;
    }
    let rows = if label_widths.is_empty() { 0 } else { y + 1 };
    (rects, rows)
}

/// PRD #80 M2 / PRD #144: render the persistent global button bar into `area`
/// and return the `(Action, Rect)` pairs to record in `UiState::button_rects`
/// so a later click hit-tests back to the right action. Every button keeps its
/// FULL `[Label Shortcut]` label; when the full set doesn't fit one row it WRAPS
/// to a second (or further) row (PRD #144) rather than collapsing to
/// shortcut-only chips — degradation is uniform, no button is special-cased.
/// Buttons are separated by one space and laid out left to right via
/// [`layout_button_bar`]; a button whose wrapped row falls outside `area`'s
/// reserved height is dropped whole (the height budget reserves the bar's
/// actual row count, so on the dashboard this never clips a real button).
fn render_button_bar(
    frame: &mut Frame,
    keybindings: &KeybindingConfig,
    area: Rect,
    has_pane_control: bool,
    extra_buttons: &[Button],
) -> Vec<(Action, Rect)> {
    // Global commands first, then any context-specific buttons (e.g. the
    // dashboard's Filter / Rename / Generate). One funnel, one bar.
    let mut buttons = global_bar_buttons(keybindings, has_pane_control);
    buttons.extend(extra_buttons.iter().cloned());

    let widths: Vec<u16> = buttons
        .iter()
        .map(|b| b.display_label().chars().count() as u16)
        .collect();
    let (placements, _rows) = layout_button_bar(&widths, area.width);

    let mut rects = Vec::with_capacity(buttons.len());
    let buf = frame.buffer_mut();
    for (button, rel) in buttons.iter().zip(placements) {
        // Drop any button whose wrapped row falls outside the reserved height.
        if rel.y >= area.height {
            continue;
        }
        let rect = Rect {
            x: area.x.saturating_add(rel.x),
            y: area.y.saturating_add(rel.y),
            width: rel.width,
            height: 1,
        };
        let pair = button.render(rect, buf);
        // PRD #80 review FIX 3: a disabled button renders dimmed but is inert —
        // its rect is NOT recorded, so a click on it is a no-op (matching the
        // keyboard, where the bound key is a silent no-op).
        if button.enabled {
            rects.push(pair);
        }
    }
    rects
}

/// PRD #144 — how many rows the bottom bar will occupy this frame, so the layout
/// pass can reserve exactly that height (a wrapped 2-row bar must cede one row
/// from the dashboard/pane region or the cards clip/overlap). Input / status
/// modes render a single-row bar (an input field or a status message); the
/// default button bar wraps via [`layout_button_bar`] across the full frame
/// width. Mirrors the button set `render_bottom_bar` builds: the dashboard /
/// orchestration views append the context buttons, the Mode view shows only the
/// global commands. The enabled/disabled state never changes a button's width
/// (disabled buttons render full-label-but-dimmed), so it is irrelevant here.
///
/// PRD #144 A2: at extreme-narrow widths the wrapped bar can request more rows
/// than the frame is tall, which would starve the main content area to zero
/// rows. The reservation is therefore capped at `frame_height - 1` so at least
/// one content row always remains (Layout doesn't panic, but content vanishing
/// is a usability bug).
fn bottom_bar_rows(ui: &UiState, width: u16, frame_height: u16, tab_view: &ActiveTabView) -> u16 {
    let rows = match ui.mode {
        UiMode::Filter | UiMode::Rename | UiMode::PaneInput => 1,
        _ if ui.status_message.is_some() => 1,
        _ => {
            let mut buttons = global_bar_buttons(&ui.keybindings, true);
            if !matches!(tab_view, ActiveTabView::Mode { .. }) {
                buttons.extend(dashboard_context_buttons(&ui.keybindings, true));
            }
            let widths: Vec<u16> = buttons
                .iter()
                .map(|b| b.display_label().chars().count() as u16)
                .collect();
            let (_, rows) = layout_button_bar(&widths, width);
            rows.max(1)
        }
    };
    // Reserve at most `frame_height - 1` rows so the content region keeps ≥1 row.
    rows.min(frame_height.saturating_sub(1))
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
            ui.button_rects = render_right_aligned_buttons(frame, &buttons, area);
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
            ui.button_rects = render_right_aligned_buttons(frame, &buttons, area);
        }
        UiMode::PaneInput => {
            // PRD #80 M6: while interacting with a pane, keep the status
            // message (e.g. "PaneInput mode …") on the left and expose the
            // [Command Mode Ctrl+D] affordance at the right edge — clicking it
            // returns to the dashboard (command mode) exactly as Ctrl+D does.
            if let Some((ref msg, _)) = ui.status_message {
                let line = Line::styled(msg.as_str(), Style::default().fg(Color::Yellow));
                frame.render_widget(Paragraph::new(line), area);
            }
            let command_mode_label = button_shortcut_label(&ui.keybindings, KbAction::Dashboard);
            let buttons = [Button::new(
                "Command Mode",
                command_mode_label,
                Action::DetachToNormal,
                true,
            )];
            ui.button_rects = render_right_aligned_buttons(frame, &buttons, area);
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
                let rects = render_button_bar(
                    frame,
                    &ui.keybindings,
                    area,
                    has_pane_control,
                    extra_buttons,
                );
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
        let pair = button.render(rect, buf);
        // PRD #80 review FIX 3: disabled buttons render dimmed but are inert —
        // don't record their rect.
        if button.enabled {
            rects.push(pair);
        }
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
    render_modal_button_row(frame, buttons, row, 0)
}

/// PRD #144 — shared modal sizing. Given a modal's desired (content-driven)
/// `desired_w` × `desired_h`, the `terminal` area, and per-modal minimum
/// dimensions, return a centered modal `Rect` clamped so that:
///   * it is never smaller than `min_w` × `min_h` (small content → the minimum),
///   * it never exceeds 90% of the terminal in either axis (large content → the
///     90% cap), and
///   * it therefore never exceeds the terminal bounds even on a very narrow /
///     short terminal (the 90% cap wins over the minimum — the very-narrow
///     clamp), and the centered rect always fits inside `terminal`.
///
/// Pure data — no rendering, fully unit-testable. Supersedes the per-modal width
/// caps and the wrap / truncate band-aids (PRD #127) with one consistent
/// content-driven sizing rule.
fn modal_rect(desired_w: u16, desired_h: u16, terminal: Rect, min_w: u16, min_h: u16) -> Rect {
    // Clamp a single axis: up to the per-modal minimum, then down to the 90%
    // cap. Because the 90% cap is always < the terminal extent, the result is
    // guaranteed to fit — even when `min` itself exceeds the terminal.
    let clamp_axis = |desired: u16, min: u16, term: u16| -> u16 {
        let cap = (term as u32 * 9 / 10) as u16; // 90% of the terminal axis
        // `cap` is always ≤ `term`, so clamping to `cap` already fits the
        // terminal — no separate `.min(term)` needed (R-Nit2).
        desired.max(min).min(cap)
    };
    let width = clamp_axis(desired_w, min_w, terminal.width);
    let height = clamp_axis(desired_h, min_h, terminal.height);
    let x = terminal.x + terminal.width.saturating_sub(width) / 2;
    let y = terminal.y + terminal.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn render_quit_confirm(frame: &mut Frame, selected: usize) -> Vec<(Action, Rect)> {
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
            text_primary()
        };
        text.push(Line::styled(
            format!("  {cursor} {label:<7} \u{2014} {desc}"),
            style,
        ));
    }

    text.push(Line::from(""));
    text.push(Line::styled(
        "  Up/Down: navigate  Enter: confirm  Esc: cancel",
        text_primary(),
    ));

    let block = Block::default()
        .title(" Quit ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
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
    render_modal_button_row(frame, &buttons, btn_row, 1)
}

/// PRD #92 F1: secondary y/n confirmation dialog when the user picked
/// Stop with at least one managed agent alive. Renders the count
/// explicitly so the user sees exactly how many agents are about to be
/// terminated. Default selection is No (index 0).
fn render_stop_confirm(frame: &mut Frame, selected: usize, agent_count: usize) {
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
            text_primary()
        };
        text.push(Line::styled(
            format!("  {cursor} {label:<5} \u{2014} {desc}"),
            style,
        ));
    }

    text.push(Line::from(""));
    text.push(Line::styled(
        "  y / Enter on Yes confirms  ·  n / Esc / Enter on No returns to Quit dialog",
        text_primary(),
    ));

    let block = Block::default()
        .title(" Stop ")
        .title_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_star_prompt(frame: &mut Frame) -> Vec<(Action, Rect)> {
    let area = frame.area();
    let popup_width = 50u16.min(area.width.saturating_sub(4));
    let popup_height = 10u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let text = vec![
        Line::from(""),
        Line::styled("  If you find dot-agent-deck useful,", text_primary()),
        Line::styled("  please consider starring the repo!", text_primary()),
        Line::from(""),
        Line::styled(
            "  github.com/vfarcic/dot-agent-deck",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        ),
        Line::from(""),
        Line::styled("  s Star  l Later  d Don't ask again", text_primary()),
    ];

    let block = Block::default()
        .title(" ⭐ Enjoying dot-agent-deck? ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

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
    render_modal_button_row(frame, &buttons, btn_row, 1)
}

fn render_config_gen_prompt(frame: &mut Frame, selected: usize) -> Vec<(Action, Rect)> {
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
        Line::styled("  No workspace modes config found for this", text_primary()),
        Line::styled("  project. Want to instruct your agent to", text_primary()),
        Line::styled("  analyze the project and create one?", text_primary()),
        Line::from(""),
    ];

    for (i, (label, desc)) in options.iter().enumerate() {
        let cursor = if i == selected { ">" } else { " " };
        let style = if i == selected {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            text_primary()
        };
        text.push(Line::styled(
            format!("  {cursor} {label:<6} \u{2014} {desc}"),
            style,
        ));
    }

    text.push(Line::from(""));
    text.push(Line::styled(
        "  Disable: dot-agent-deck config set auto_config_prompt false",
        text_primary(),
    ));
    text.push(Line::from(""));
    text.push(Line::styled(
        "  Up/Down: navigate  Enter: confirm  Esc: cancel",
        text_primary(),
    ));

    let block = Block::default()
        .title(" Generate .dot-agent-deck.toml ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

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
    render_modal_button_row(frame, &buttons, btn_row, 1)
}

/// Format one help row: a key column (left-padded to a fixed width so the
/// description column lines up) followed by the description. PRD #40 — the
/// key column is sourced from the active [`KeybindingConfig`] so the overlay
/// always reflects the user's real bindings.
fn help_key_line(keys: &str, desc: &str) -> Line<'static> {
    Line::from(format!("  {keys:<18} {desc}"))
}

/// PRD #40: an action's active key notation for *display* in the help overlay
/// and hints bar, with `(unbound)` substituted for the empty string so an
/// unbound action never renders as a bare key column (`: new`). Shared by both
/// renderers so they can't drift (Greptile P2).
fn display_notation(keybindings: &KeybindingConfig, action: KbAction) -> String {
    let s = keybindings.notation(action);
    if s.is_empty() {
        "(unbound)".to_string()
    } else {
        s
    }
}

/// PRD #40: the displayed jump-key hint for `Jump1..Jump9`. When every jump is
/// at its bare-digit default (`1`..`9`) this returns the compact `"1-9"` so the
/// default help/hints stay byte-for-byte unchanged; once any jump is remapped
/// it lists the actual notations (slash-joined) so the remap is visible.
fn jump_range_notation(keybindings: &KeybindingConfig) -> String {
    const JUMPS: [KbAction; 9] = [
        KbAction::Jump1,
        KbAction::Jump2,
        KbAction::Jump3,
        KbAction::Jump4,
        KbAction::Jump5,
        KbAction::Jump6,
        KbAction::Jump7,
        KbAction::Jump8,
        KbAction::Jump9,
    ];
    let all_default = JUMPS
        .iter()
        .enumerate()
        .all(|(i, &a)| keybindings.notation(a) == (i + 1).to_string());
    if all_default {
        "1-9".to_string()
    } else {
        JUMPS
            .iter()
            .map(|&a| display_notation(keybindings, a))
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// PRD #40: build the dashboard hints-bar text from the active keybinding
/// config. With the default config this reproduces the previous hardcoded
/// string byte-for-byte (`Ctrl+n: new  …  Ctrl+c: quit`); a remapped action
/// shows the user's key (e.g. `Alt+Shift+l: layout`), and an unbound action
/// shows `(unbound)` rather than a bare `: new`. Single source of truth for
/// both the live hints bar (`render_bottom_bar`) and the standalone
/// [`render_hints_bar_to_buffer`] snapshot entrypoint.
fn dashboard_hints_string(keybindings: &KeybindingConfig) -> String {
    // `Ctrl+c: quit` is hardcoded: quit is not a remappable action — Ctrl+C is
    // the non-overridable modal trigger (Detach/Stop/Cancel), so the hint is a
    // fixed string rather than a config-derived notation.
    format!(
        "{}: new  {}: close  {}: layout  {}: dashboard ({} {} {})  Ctrl+c: quit",
        display_notation(keybindings, KbAction::NewPane),
        display_notation(keybindings, KbAction::ClosePane),
        display_notation(keybindings, KbAction::ToggleLayout),
        display_notation(keybindings, KbAction::Dashboard),
        jump_range_notation(keybindings),
        display_notation(keybindings, KbAction::Help),
        display_notation(keybindings, KbAction::Filter),
    )
}

/// PRD #40 (L1 `keybindings/help/001`): render the help overlay against an
/// arbitrary [`KeybindingConfig`] into a standalone `Buffer` for snapshot
/// testing. Mirrors [`render_card_to_buffer`] — a `TestBackend` of the given
/// size, drawn through the *same* `render_help_overlay` code path the live UI
/// uses, so the snapshot and the running overlay can never drift. (The
/// default-bindings PRD #80 seam is [`render_help_overlay_to_buffer`].)
pub fn render_help_overlay_with_bindings_to_buffer(
    keybindings: &KeybindingConfig,
    active_mode_name: Option<&str>,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    terminal
        .draw(|frame| {
            // The live overlay returns its clickable button rects; the
            // snapshot path doesn't need them.
            let _ = render_help_overlay(frame, keybindings, active_mode_name);
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #40 (L1 `keybindings/hints/001`): render the dashboard hints bar
/// against an arbitrary [`KeybindingConfig`] into a standalone `Buffer` for
/// snapshot testing. Uses the shared [`dashboard_hints_string`] builder so the
/// snapshot matches the live bar's content.
pub fn render_hints_bar_to_buffer(
    keybindings: &KeybindingConfig,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let hints = dashboard_hints_string(keybindings);
    terminal
        .draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height,
            };
            // PRD #13: hint text is read, so it renders full-contrast
            // (text_primary), not dimmed.
            let line = Line::from(Span::styled(hints, text_primary()));
            frame.render_widget(Paragraph::new(line), area);
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

fn render_help_overlay(
    frame: &mut Frame,
    keybindings: &KeybindingConfig,
    active_mode_name: Option<&str>,
) -> Vec<(Action, Rect)> {
    let cyan = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // PRD #40: notation for a remappable action, or "(unbound)" when the user
    // has cleared the binding (`action = ""`), so the overlay is honest about
    // a key that no longer fires. Shared with the hints bar via
    // `display_notation` so the two renderers can't drift.
    let n = |action: KbAction| -> String { display_notation(keybindings, action) };

    let left: Vec<Line> = vec![
        Line::styled("  Global (works from any pane)", cyan),
        Line::from(""),
        help_key_line(&n(KbAction::Dashboard), "Command mode (dashboard)"),
        help_key_line(&n(KbAction::NewPane), "Create new pane"),
        help_key_line(&n(KbAction::ClosePane), "Close selected pane"),
        help_key_line(&n(KbAction::ToggleLayout), "Toggle layout (stacked/tiled)"),
        // Quit is not a remappable action: Ctrl+C (non-overridable) opens the
        // Detach/Stop/Cancel modal, so the help row is a fixed string.
        help_key_line("Ctrl+c", "Quit"),
        Line::from(""),
        Line::styled("  Tab Navigation", cyan),
        Line::from(""),
        help_key_line(
            &format!("Tab / Right / {}", n(KbAction::MoveRight)),
            "Next tab",
        ),
        help_key_line(
            &format!("Shift+Tab / Left / {}", n(KbAction::MoveLeft)),
            "Prev tab",
        ),
        help_key_line(&format!("{MOD_KEY}+PgDn"), "Next tab"),
        help_key_line(&format!("{MOD_KEY}+PgUp"), "Prev tab"),
        Line::from(""),
        Line::styled("  Dashboard (command mode)", cyan),
        Line::from(""),
        help_key_line(
            &format!("{} / Down", n(KbAction::MoveDown)),
            "Select next card",
        ),
        help_key_line(
            &format!("{} / Up", n(KbAction::MoveUp)),
            "Select previous card",
        ),
        help_key_line(&jump_range_notation(keybindings), "Jump to pane N"),
        help_key_line(&n(KbAction::FocusPane), "Focus selected pane"),
        help_key_line(&n(KbAction::Filter), "Filter sessions"),
        help_key_line(&n(KbAction::ClearFilter), "Clear filter"),
        help_key_line(&n(KbAction::Rename), "Rename session"),
        help_key_line(
            &n(KbAction::GenerateConfig),
            "Generate .dot-agent-deck.toml",
        ),
        help_key_line(
            &format!(
                "{} / {}",
                n(KbAction::ApprovePermission),
                n(KbAction::DenyPermission)
            ),
            "Approve / deny permission",
        ),
        help_key_line(&n(KbAction::OpenScheduledTasks), "Scheduled Tasks manager"),
        help_key_line(&n(KbAction::Help), "Toggle this help"),
    ];

    let mut right: Vec<Line> = vec![
        Line::styled("  Mode Tab (in-tab navigation)", cyan),
        Line::from(""),
        help_key_line(
            &format!("{} / Down", n(KbAction::MoveDown)),
            "Focus next pane",
        ),
        help_key_line(
            &format!("{} / Up", n(KbAction::MoveUp)),
            "Focus previous pane",
        ),
        Line::from("  Enter           Enter PaneInput on selected"),
        Line::from("  Esc             Deselect side pane"),
        Line::from("  Mouse click     Focus pane"),
        Line::from("  Ctrl+click      Open hyperlink"),
        help_key_line(&n(KbAction::Dashboard), "Return to Normal mode"),
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
        Line::from("  Panes auto-saved continuously."),
        Line::from("  Restored automatically on launch."),
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
        .border_style(Style::default().fg(Color::Cyan));
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
        Line::styled("  Press ? or Esc to close", text_primary()),
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
    render_modal_button_row(frame, &close_button, btn_row, 2)
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
fn render_dir_picker(frame: &mut Frame, picker: &mut DirPickerState) -> PickerClickTargets {
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
        let mut spans = vec![Span::styled("  / ", text_primary())];
        spans.push(Span::styled(picker.filter_text.clone(), text_primary()));
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
        lines.push(Line::styled(message, text_primary()));
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
                text_primary()
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
    lines.push(Line::styled(nav_footer, text_primary()));
    let mode_footer = if picker.filtering {
        "  Typing: add characters  Enter: accept filter  Esc: clear"
    } else if !picker.filter_text.is_empty() {
        "  /: edit filter  Esc: clear filter  q: cancel"
    } else {
        "  /: filter directories  Esc or q: cancel"
    };
    lines.push(Line::styled(mode_footer, text_primary()));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Select Directory ")
        .border_style(Style::default().fg(Color::Cyan));
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
    let button_rects = render_modal_button_row(frame, &buttons, btn_row, 1);

    (row_rects, button_rects)
}

/// Footer-hint string for the unified new-pane form. Factored out so the
/// focus-dependent wording can be unit-tested without driving a TestBackend.
/// `name_submits` is true when focus is on Name and the Command field is
/// hidden (orchestration selected) — i.e. Enter on Name submits the form.
///
/// PRD #170 round 2 (reviewer finding 6): the mode-locked schedule form has a
/// single navigable field (Command), so the generic "Tab: switch field" hint is
/// misleading — `schedule_locked` shows a Command-only `Enter: confirm  Esc:
/// cancel` instead.
fn new_pane_form_footer_hint(
    has_mode_field: bool,
    name_submits: bool,
    schedule_locked: bool,
) -> &'static str {
    if schedule_locked {
        return "  Enter: confirm  Esc: cancel";
    }
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

/// PRD #127 M3.3: clickable geometry returned by [`render_scheduled_tasks`] —
/// the `[Add]`/`[Edit]`/`[Delete]`/`[Run now]` button rects (recorded in
/// `UiState::modal_button_rects`) and the schedule row rects each paired with
/// its index into `scheduled_tasks` (recorded in `UiState::scheduled_row_rects`).
type ScheduledTasksClickTargets = (Vec<(Action, Rect)>, Vec<(usize, Rect)>);

/// PRD #127 M3.3: render the "Scheduled Tasks" manager dialog — a
/// read-only-plus-actions list of the configured schedules, each row showing
/// name, status (live/idle/disabled), and next-fire. Shows a delete
/// confirmation when armed.
///
/// PRD #127 finding #4 / PRD #80: the action line is now a row of clickable
/// `[Add a] [Edit e] [Delete d] [Run now r]` buttons; returns their
/// `(Action, Rect)` pairs to record in `UiState::modal_button_rects` for the
/// mouse hit-test (empty while the delete confirmation is armed). The keyboard
/// a/e/d/r/Enter still drive the same actions.
///
/// PRD #127 M3.3: also returns each schedule row's `(index, Rect)` to record in
/// `UiState::scheduled_row_rects` so a click on a row re-selects it (keyboard
/// parity for `j`/`k`); empty while the delete confirmation is armed.
fn render_scheduled_tasks(frame: &mut Frame, ui: &UiState) -> ScheduledTasksClickTargets {
    let area = frame.area();
    // PRD #144: the manager dialog is content-sized via `modal_rect` (no fixed
    // 72-col cap). The list columns size to the widest schedule name / next-fire
    // cell so a long name renders un-clipped, and the modal grows wide enough to
    // contain the delete confirmation on its natural lines — superseding the
    // PRD #127 width cap + `truncate_cell` + `wrap_to_width` band-aids.
    let count_chars = |s: &str| s.chars().count();
    let name_col = ui
        .scheduled_tasks
        .iter()
        .map(|t| count_chars(&t.name))
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let next_col = ui
        .scheduled_tasks
        .iter()
        .map(|t| count_chars(&schedule_next_fire_display(t)))
        .max()
        .unwrap_or(0)
        .max("NEXT FIRE".len());
    let status_col = 11usize; // widest status ("disabled") + padding
    let row_count = ui.scheduled_tasks.len().max(1) as u16;

    // PRD #144: the delete confirmation renders on its natural lines (the name
    // on its own line, the fixed `… (y/n)` trailer on the next). The modal is
    // sized to contain them below, so the trailer is never pushed past the
    // border — replacing the PRD #127 `wrap_to_width` band-aid.
    let confirm_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let confirm_lines: Vec<Line> = if ui.scheduled_delete_confirm {
        let name = ui
            .scheduled_tasks
            .get(ui.scheduled_selected)
            .map(|t| t.name.as_str())
            .unwrap_or("");
        vec![
            Line::styled(format!("  Delete schedule '{name}'?"), confirm_style),
            Line::styled(
                "  definition only \u{2014} open tab kept. (y/n)".to_string(),
                confirm_style,
            ),
        ]
    } else {
        Vec::new()
    };

    // Chrome inside the border: leading blank + column header + trailing blank +
    // footer. The footer is the confirmation (≥1 line) when armed, else the
    // button-placeholder row plus a dedicated `Esc close` hint row below it.
    let footer_lines = if ui.scheduled_delete_confirm {
        confirm_lines.len().max(1)
    } else {
        2
    };
    let chrome_lines = 3 + footer_lines;

    // Widest content line drives the modal width. The padded field rows fill to
    // the inner width, so they don't drive it — the list columns, the header,
    // the confirmation, the action-button row, the title, and the empty-state
    // message do. (`[Add a] [Edit e] [Delete d] [Run now r]`, indented one cell.)
    const BUTTON_ROW_W: usize = 1 + 7 + 1 + 8 + 1 + 10 + 1 + 11;
    let list_w = 2 + name_col + status_col + next_col;
    // R-Sug1: the header line appends a scroll indicator (e.g. `  (↑12 ↓34)`)
    // when rows are hidden. Reserve its worst-case width in the header budget so
    // a long name + a scrolling list never clips the up/down counts at the right
    // border. Worst case is both arrows present, each count as wide as the digit
    // count of the total row count: `  (↑` (4) + digits + ` ↓` (2) + digits +
    // `)` (1) = 7 + 2·digits.
    let scroll_hint_w = if ui.scheduled_tasks.is_empty() {
        0
    } else {
        7 + 2 * ui.scheduled_tasks.len().to_string().len()
    };
    let header_w = 2 + name_col + status_col + "NEXT FIRE".len() + scroll_hint_w;
    let confirm_w = confirm_lines.iter().map(|l| l.width()).max().unwrap_or(0);
    let empty_w = if ui.scheduled_tasks.is_empty() {
        count_chars("  No schedules configured. Press `a` to add one.")
    } else {
        0
    };
    let content_w = [
        list_w,
        header_w,
        confirm_w,
        BUTTON_ROW_W,
        count_chars("  Esc close"),
        empty_w,
        count_chars(" Scheduled Tasks "),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);

    // Size & center the modal: content width + borders + a little right margin
    // (+4 = 2 borders + 2 margin), content height + borders + one slack row
    // (+3 = 2 borders + 1 row, mirroring the width margin so the list never sits
    // flush against the bottom border). Clamped to [min, 90% of terminal] by
    // `modal_rect`.
    let desired_w = (content_w as u16).saturating_add(4);
    let desired_h = row_count + chrome_lines as u16 + 3;
    let popup_area = modal_rect(desired_w, desired_h, area, 56, 9);
    let popup_height = popup_area.height;

    frame.render_widget(Clear, popup_area);

    // PRD #127 N2: bound the rendered rows to what fits and scroll the selected
    // row into view, so a list taller than the viewport is fully reachable.
    let inner_height = popup_height.saturating_sub(2) as usize;
    let max_visible_rows = inner_height.saturating_sub(chrome_lines).max(1);
    let (win_start, win_end) = visible_window(
        ui.scheduled_tasks.len(),
        ui.scheduled_selected,
        max_visible_rows,
    );

    let mut lines: Vec<Line> = Vec::new();
    // PRD #127 M3.3: collect each schedule row's clickable rect so the mouse
    // can re-select a row (keyboard parity for `j`/`k`). Built alongside the
    // rendered lines so the screen-row math stays in lock-step with the layout.
    let mut row_rects: Vec<(usize, Rect)> = Vec::new();
    lines.push(Line::from(""));

    if ui.scheduled_tasks.is_empty() {
        lines.push(Line::styled(
            "  No schedules configured. Press `a` to add one.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        // Column header, with a scroll indicator when rows are hidden.
        let hidden_above = win_start;
        let hidden_below = ui.scheduled_tasks.len().saturating_sub(win_end);
        let scroll_hint = match (hidden_above, hidden_below) {
            (0, 0) => String::new(),
            (a, 0) => format!("  (\u{2191}{a})"),
            (0, b) => format!("  (\u{2193}{b})"),
            (a, b) => format!("  (\u{2191}{a} \u{2193}{b})"),
        };
        lines.push(Line::styled(
            format!(
                "  {:<name_col$}{:<status_col$}{}{}",
                "NAME", "STATUS", "NEXT FIRE", scroll_hint
            ),
            text_primary().add_modifier(Modifier::BOLD),
        ));
        for (i, task) in ui
            .scheduled_tasks
            .iter()
            .enumerate()
            .skip(win_start)
            .take(win_end - win_start)
        {
            let is_live = ui.scheduled_live_names.contains(&task.name);
            let status = schedule_status_label(task.enabled, is_live);
            let next = schedule_next_fire_display(task);
            let selected = i == ui.scheduled_selected;
            let marker = if selected { "\u{25b6} " } else { "  " };
            // PRD #144: render the FULL name (no `truncate_cell`) — the modal is
            // sized to the widest name, so the `name_col`-padded cell never clips.
            let row = format!(
                "{marker}{:<name_col$}{:<status_col$}{}",
                task.name, status, next,
            );
            let style = if selected {
                text_primary().add_modifier(Modifier::BOLD)
            } else {
                text_primary()
            };
            // Screen row of this line: the Paragraph renders inside the border,
            // so content line `n` sits at `popup_area.y + 1 + n`. Capture the
            // rect (full inner width) before pushing.
            let line_idx = lines.len();
            row_rects.push((
                i,
                Rect {
                    x: popup_area.x + 1,
                    y: popup_area.y + 1 + line_idx as u16,
                    width: popup_area.width.saturating_sub(2),
                    height: 1,
                },
            ));
            lines.push(Line::styled(row, style));
        }
    }

    lines.push(Line::from(""));
    // The footer is either the delete confirmation (no buttons) or a blank
    // placeholder row that the clickable action buttons are overlaid onto after
    // the Paragraph render (mirroring `render_quit_confirm`). The `Esc close`
    // keyboard hint (Esc has no button) sits on its OWN row below the buttons so
    // it never clips on narrow terminals — a Paragraph doesn't wrap, so padding
    // it to the right of the buttons would push it past the border once the
    // modal shrinks with the terminal width.
    let button_row_index = if ui.scheduled_delete_confirm {
        // Pre-wrapped above so a long name grows the modal in HEIGHT (name on its
        // own line) instead of spilling the trailing `(y/n)` past the border.
        lines.extend(confirm_lines);
        None
    } else {
        let idx = lines.len();
        // Blank placeholder row overlaid with the action buttons below.
        lines.push(Line::from(""));
        // Dedicated keyboard-hint row, left-aligned like the list rows so it
        // stays fully inside the border at any reasonable width.
        lines.push(Line::styled("  Esc close", text_dim()));
        Some(idx)
    };

    // PRD #13: terminal-relative — no absolute background fill; the terminal's
    // own background shows through.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Scheduled Tasks ")
        .border_style(Style::default().fg(Color::Cyan));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);

    // PRD #127 finding #4 + PRD #80: overlay the clickable action buttons on the
    // footer row. [Edit]/[Delete]/[Run now] act on the selected row, so they're
    // disabled (dimmed + inert) when the list is empty; [Add] is always
    // actionable — mirroring the a / e-Enter / d / r keys.
    let Some(row_idx) = button_row_index else {
        // Delete confirmation is armed: no action buttons and no row-click
        // re-selection — `y`/`n`/Esc are the only inputs.
        return (Vec::new(), Vec::new());
    };
    let has_rows = !ui.scheduled_tasks.is_empty();
    let run_now_action = ui
        .scheduled_tasks
        .get(ui.scheduled_selected)
        .map(|t| Action::ScheduleRunNow(t.name.clone()))
        .unwrap_or(Action::Continue);
    // PRD #127: each button advertises its shortcut key alongside the label —
    // `[Add a]` / `[Edit e]` / `[Delete d]` / `[Run now r]` — mirroring the
    // `[Scheduled Tasks s]` button-bar button so a keyboard user can tell which
    // key drives each action. These in-dialog keys are matched as literals in
    // `handle_scheduled_tasks_key` (not remappable `KbAction`s), so the literal
    // key is the shortcut.
    let buttons = [
        Button::new("Add", "a", Action::ScheduleAdd, true),
        Button::new("Edit", "e", Action::ScheduleEdit, has_rows),
        Button::new("Delete", "d", Action::ScheduleArmDelete, has_rows),
        Button::new("Run now", "r", run_now_action, has_rows),
    ];
    // R-Nit1: `BUTTON_ROW_W` (used above to budget the modal width before the
    // buttons exist) must equal the actual rendered button-row width — indent
    // (1) + each `[Label key]` label + one separator between buttons. Recompute
    // it from the real labels so a label rename can't silently drift the budget.
    debug_assert_eq!(
        BUTTON_ROW_W,
        1 + buttons
            .iter()
            .map(|b| b.display_label().chars().count())
            .sum::<usize>()
            + buttons.len().saturating_sub(1),
        "BUTTON_ROW_W drifted from the rendered action-button labels; update the constant"
    );
    let btn_row = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1 + row_idx as u16,
        width: popup_area.width.saturating_sub(2),
        height: 1,
    };
    (
        render_modal_button_row(frame, &buttons, btn_row, 1),
        row_rects,
    )
}

/// PRD #127 N2: the `[start, end)` slice of `len` rows to render so `selected`
/// is always visible within a window of `max_rows`. When `selected` would fall
/// below the window it scrolls so the selected row is the last visible one.
/// Pure so the scroll math is unit-testable.
fn visible_window(len: usize, selected: usize, max_rows: usize) -> (usize, usize) {
    if len == 0 || max_rows == 0 {
        return (0, 0);
    }
    let window = max_rows.min(len);
    let start = if selected < window {
        0
    } else {
        (selected + 1 - window).min(len - window)
    };
    (start, start + window)
}

/// PRD #80 M8: clickable geometry returned by [`render_new_pane_form`] — the
/// field rows (each paired with the [`FormField`] it focuses), the mode chips
/// (each paired with its option index), and the `[Submit]`/`[Cancel]` button
/// rects.
type FormClickTargets = (
    Vec<(FormField, Rect)>,
    Vec<(usize, Rect)>,
    Vec<(Action, Rect)>,
);

fn render_new_pane_form(frame: &mut Frame, form: &NewPaneFormState) -> FormClickTargets {
    let area = frame.area();
    // PRD #170 (unify): the Scheduled-Tasks manager's Add/Edit reuse this form
    // MODE-LOCKED to schedule authoring — hide the Mode cycler + the Name field
    // and retitle the modal. The unlocked `Ctrl+n` form (the common case) is
    // unaffected: with `schedule_locked == false`, `show_mode == has_mode_field`
    // and `show_name == true`, so every branch below reduces to the prior
    // behavior and the render stays byte-identical.
    let show_mode = form.has_mode_field && !form.schedule_locked;
    let show_name = !form.schedule_locked;
    // PRD #144: the Mode chips render on a SINGLE row (no `layout_mode_chips`
    // wrap band-aid) and the modal is content-sized via `modal_rect` to be wide
    // enough to contain the whole row — so the trailing `[schedule]` chip is
    // never clipped. `  Mode: ` is 8 cols; chips start one space after it.
    let chip_label_w: u16 = 8; // "  Mode: "
    let first_chip_x: u16 = chip_label_w + 1;
    let chip_widths: Vec<u16> = if show_mode {
        (0..form.mode_option_count())
            .map(|i| form.mode_option_name(i).chars().count() as u16 + 2) // [name]
            .collect()
    } else {
        Vec::new()
    };
    // x-offset of each chip on the single chip row (relative to the inner area):
    // each chip follows the previous one plus a trailing space.
    let chip_x_offsets: Vec<u16> = {
        let mut xs = Vec::with_capacity(chip_widths.len());
        let mut cx = first_chip_x;
        for &w in &chip_widths {
            xs.push(cx);
            cx = cx.saturating_add(w).saturating_add(1);
        }
        xs
    };
    // Inner width the chip row needs: the rightmost chip's end column.
    let chip_row_w = chip_x_offsets
        .iter()
        .zip(&chip_widths)
        .map(|(&x, &w)| x.saturating_add(w))
        .max()
        .unwrap_or(first_chip_x);
    let mode_lines: u16 = 1;
    // The mode field (when modes exist) or the tip line (when they don't) needs
    // its content line plus one spacing row before the next field. PRD #170: the
    // locked schedule form drops the whole mode block.
    let mode_extra: u16 = if form.schedule_locked {
        0
    } else {
        mode_lines + 1
    };
    // PRD #170: the locked schedule form also drops the Name row.
    let name_rows: u16 = if show_name { 1 } else { 0 };
    // PRD #106: when the Command field is hidden (orchestration selected) the
    // form is two rows shorter — Command's label row plus its spacing row.
    let cmd_visible = form.command_visible();
    let cmd_rows: u16 = if cmd_visible { 2 } else { 0 };
    // PRD #127 M3.2 / PRD #120: either authoring option ("schedule" or
    // "schedule: issues") adds one separator/label row marking it as a throwaway
    // authoring session (only in the unlocked Mode cycler — the locked schedule
    // form has no mode block).
    let schedule_rows: u16 = if show_mode && form.is_authoring_selected() {
        1
    } else {
        0
    };
    // PRD #144: content-size & center. Width grows to fit the mode chip row
    // (plus borders + a little margin) but never below the comfortable 56-col
    // base; height is the reserved field rows. `modal_rect` clamps to 90% of
    // terminal. The `9 + name_rows` base reproduces the prior `10` when the Name
    // row is shown (unlocked) and trims a row when it is hidden (locked).
    let desired_w = chip_row_w.saturating_add(4).max(56);
    let desired_h = 9 + name_rows + mode_extra + cmd_rows + schedule_rows;
    let popup_area = modal_rect(desired_w, desired_h, area, 56, 10);
    let popup_width = popup_area.width;

    frame.render_widget(Clear, popup_area);

    let inner_width = popup_width.saturating_sub(4) as usize;

    let focused_label = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let unfocused_label = text_dim();

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

    // PRD #80 M8: the Mode field is now a row of clickable chips (one per
    // option, including "No mode"), overlaid after the Paragraph render so each
    // chip gets its own rect. Reserve its blank line here.
    // PRD #170: the locked schedule form omits the whole mode block (chips/tip
    // + schedule hint). The unlocked branch is unchanged (`show_mode` equals the
    // old `has_mode_field` there).
    let mut mode_line_idx: Option<usize> = None;
    if show_mode {
        mode_line_idx = Some(lines.len());
        // Reserve one blank line per (possibly wrapped) chip row, plus a spacing
        // line before the next field — matching the height reserved above.
        for _ in 0..mode_lines {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(""));
    } else if !form.schedule_locked {
        // No .dot-agent-deck.toml or no modes — show a contextual hint.
        lines.push(Line::styled(
            "  Tip: press g on dashboard to create modes",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        ));
        lines.push(Line::from(""));
    }
    // PRD #127 M3.2 / PRD #120: subtle visual separation — when either authoring
    // option ("schedule" or "schedule: issues") is selected, the second reserved
    // line shows a throwaway-session hint (the first holds the chips, the second
    // is spare).
    if show_mode && form.is_authoring_selected() {
        lines.push(Line::styled(
            "           \u{21b3} authoring (one-off)",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        ));
    }

    // PRD #170: the locked schedule form hides the Name field — the card's name
    // is fixed to `SCHEDULE_MODE_NAME` and the schedule's own name is authored
    // conversationally.
    let mut name_line_idx: Option<usize> = None;
    if show_name {
        name_line_idx = Some(lines.len());
        lines.push(Line::from(vec![
            Span::styled("  Name:    ", name_style),
            Span::styled(
                format!(
                    "{:<width$}",
                    form.name,
                    width = inner_width.saturating_sub(11)
                ),
                if form.focused == FormField::Name {
                    text_primary()
                } else {
                    unfocused_label
                },
            ),
        ]));
    }
    let mut cmd_line_idx: Option<usize> = None;
    if cmd_visible {
        lines.push(Line::from(""));
        cmd_line_idx = Some(lines.len());
        lines.push(Line::from(vec![
            Span::styled("  Command: ", cmd_style),
            Span::styled(
                format!(
                    "{:<width$}",
                    form.command,
                    width = inner_width.saturating_sub(11)
                ),
                if form.focused == FormField::Command {
                    text_primary()
                } else {
                    unfocused_label
                },
            ),
        ]));
    }
    lines.push(Line::from(""));
    // PRD #80 M8: reserve a row for the [Submit]/[Cancel] buttons (overlaid).
    let submit_line_idx = lines.len();
    lines.push(Line::from(""));

    // PRD #106 follow-up: when the Command field is hidden (orchestration
    // selected) and focus is on Name, Enter submits — surface that instead of
    // the generic "Enter: next" wording, which is misleading in that state.
    let name_submits = form.focused == FormField::Name && !cmd_visible;
    // PRD #170: pass `show_mode` (false when locked) so the locked footer drops
    // the `◀▶: mode` hint; unlocked it equals the old `has_mode_field`. Finding 6:
    // `schedule_locked` selects the Command-only `Enter: confirm  Esc: cancel`.
    let footer = new_pane_form_footer_hint(show_mode, name_submits, form.schedule_locked);
    lines.push(Line::styled(footer, text_primary()));

    // PRD #170: the locked schedule form retitles the modal by action; otherwise
    // the title reflects the selected mode (unchanged).
    let title = if form.schedule_locked {
        if form.schedule_existing.is_some() {
            " Edit Schedule ".to_string()
        } else {
            " New Schedule ".to_string()
        }
    } else {
        match form.selected_mode() {
            Some(cfg) => format!(" New Agent \u{2014} {} mode ", cfg.name),
            None => " New Agent ".to_string(),
        }
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);

    // ── Clickable geometry (overlaid / computed after the Paragraph) ────────
    let line_y = |n: usize| popup_area.y + 1 + n as u16;
    // PRD #144 A1: the overlays below (mode chips, Submit/Cancel row, cursor)
    // are placed by absolute line index, but on a degenerate short terminal the
    // `modal_rect`-clamped popup can be far shorter than the form's reserved
    // rows. Any write whose row falls at/below `popup_bottom` would land past
    // the buffer (`set_span` only guards the x-axis) → panic, so every overlay
    // row is skipped when it doesn't fit — mirroring how `render_scheduled_tasks`
    // bounds its rows. `popup_bottom <= buffer height` because `modal_rect`
    // clamps the popup to the terminal.
    let popup_bottom = popup_area.bottom();
    let row_x = popup_area.x + 1;
    let row_width = popup_area.width.saturating_sub(2);
    let inner_end = row_x + row_width;

    let mut field_rects: Vec<(FormField, Rect)> = Vec::new();
    if let Some(ni) = name_line_idx {
        field_rects.push((
            FormField::Name,
            Rect {
                x: row_x,
                y: line_y(ni),
                width: row_width,
                height: 1,
            },
        ));
    }
    if let Some(ci) = cmd_line_idx {
        field_rects.push((
            FormField::Command,
            Rect {
                x: row_x,
                y: line_y(ci),
                width: row_width,
                height: 1,
            },
        ));
    }

    // Mode chip row: render `  Mode: ` then one `[name]` chip per option, the
    // selected one highlighted. PRD #144: the chips sit on a SINGLE row —
    // `chip_x_offsets`, computed up front, gives each chip's x-offset, and the
    // modal was sized wide enough to contain the rightmost chip (e.g.
    // `[schedule]`) un-clipped. Each chip records its own clickable rect so the
    // mode-chip click hit-test still selects the right option.
    let mut chip_rects: Vec<(usize, Rect)> = Vec::new();
    if let Some(mi) = mode_line_idx
        && line_y(mi) < popup_bottom
    {
        let mode_y = line_y(mi);
        // Whole chip area (off-chip clicks focus the Mode field).
        field_rects.push((
            FormField::Mode,
            Rect {
                x: row_x,
                y: mode_y,
                width: row_width,
                height: mode_lines,
            },
        ));
        let mode_label_style = if form.focused == FormField::Mode {
            focused_label
        } else {
            unfocused_label
        };
        // PRD #13: the selected mode chip inverts the terminal's own fg/bg in
        // place (REVERSED) rather than painting an absolute pair; unselected
        // chips dim the terminal foreground.
        let selected_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
        let chip_style = text_primary();
        let buf = frame.buffer_mut();
        let _ = buf.set_span(
            row_x,
            mode_y,
            &Span::styled("  Mode: ", mode_label_style),
            inner_end.saturating_sub(row_x),
        );
        for (i, &x_off) in chip_x_offsets.iter().enumerate() {
            let cx = row_x + x_off;
            let chip = format!("[{}]", form.mode_option_name(i));
            let w = chip.chars().count() as u16;
            let style = if i == form.selection_index {
                selected_style
            } else {
                chip_style
            };
            let avail = inner_end.saturating_sub(cx);
            let (_after, _) = buf.set_span(cx, mode_y, &Span::styled(chip, style), avail);
            chip_rects.push((
                i,
                Rect {
                    x: cx,
                    y: mode_y,
                    width: w.min(avail),
                    height: 1,
                },
            ));
        }
    }

    // [Submit] / [Cancel] buttons on the reserved row.
    let buttons = [
        Button::new("Submit", "", Action::FormSubmit, true),
        Button::new("Cancel", "", Action::FormCancel, true),
    ];
    let btn_row = Rect {
        x: row_x,
        y: line_y(submit_line_idx),
        width: row_width,
        height: 1,
    };
    // PRD #144 A1: skip the button row entirely when it falls outside the
    // clamped popup (degenerate short terminal) rather than writing past the
    // buffer bottom.
    let button_rects = if btn_row.y < popup_bottom {
        render_modal_button_row(frame, &buttons, btn_row, 1)
    } else {
        Vec::new()
    };

    // Cursor in the active text field (Mode uses chips, so no cursor there).
    // PRD #144 A1: only place the cursor when its row fits inside the popup.
    if form.focused == FormField::Name
        && let Some(ni) = name_line_idx
        && line_y(ni) < popup_bottom
    {
        let cursor_x = popup_area.x + 12 + form.name.len() as u16;
        frame.set_cursor_position(Position::new(cursor_x, line_y(ni)));
    } else if form.focused == FormField::Command
        && let Some(ci) = cmd_line_idx
        && line_y(ci) < popup_bottom
    {
        let cursor_x = popup_area.x + 12 + form.command.len() as u16;
        frame.set_cursor_position(Position::new(cursor_x, line_y(ci)));
    }

    (field_rects, chip_rects, button_rects)
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
    idle_art: Option<&IdleArtEntry>,
) {
    // PRD #201 M5.1: gate the Pi first-class identity/status behind the
    // experimental flag at this single render seam (CLAUDE.md rule 9). With the
    // flag OFF a Pi pane's card is byte-identical to the pre-feature
    // `AgentType::None` placeholder (the unrecognized-agent baseline a
    // `command = "pi"` pane showed before this PRD); ON, it shows `Pi · <id>`.
    // This is a PRESENTATION switch only — `session.agent_type` is never
    // mutated, so `from_command`, the daemon protocol, hooks, the extension,
    // and agent-event routing are untouched, and gating never hides the pane.
    // `grep show_pi_agent` finds this gate for the graduate-pi-agent cleanup.
    let effective_agent =
        if session.agent_type == crate::event::AgentType::Pi && !crate::features::show_pi_agent() {
            crate::event::AgentType::None
        } else {
            session.agent_type.clone()
        };
    let is_placeholder = effective_agent == crate::event::AgentType::None;
    let (status_label, status_style) = if is_placeholder {
        ("No agent", text_primary())
    } else {
        status_style(&session.status)
    };
    let status_color = status_style.fg.unwrap_or(Color::Reset);

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
            effective_agent, id_display
        )
    };

    let dot = flash_dot(&session.status, tick);
    let status_text = format!(" {} {} ", dot, status_label);
    // area.width includes left+right borders (2 chars)
    let max_title = (area.width as usize).saturating_sub(status_text.chars().count() + 2);
    title_left = truncate_with_ellipsis(&title_left, max_title);

    let border_style = if is_selected {
        // PRD #155 Option A: selection uses the dedicated `selected` accent role
        // (Magenta + BOLD, paired with the `▸ ` title marker above) — distinct
        // from every status color and from the focused-pane cyan.
        Style::default()
            .fg(palette::SELECTED)
            .add_modifier(Modifier::BOLD)
    } else if is_placeholder {
        // Placeholder ("No agent") cards read as secondary: dim the terminal's
        // own foreground (matching the prior DarkGray intent) so the empty slot
        // doesn't draw a full-strength border like a live agent.
        text_dim()
    } else {
        Style::default().fg(status_color)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            title_left,
            text_primary().add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Left)
        .title(
            Line::from(Span::styled(status_text, status_style))
                .alignment(ratatui::layout::Alignment::Right),
        );

    // PRD #13 Option A: selection is cued by the `▸ ` title prefix and the
    // Magenta+BOLD border above (the `selected` palette accent, PRD #155) — no
    // whole-card `Modifier::REVERSED`. The full-card inversion was too heavy and
    // redundant with those two cues, and stays terminal-relative (no absolute
    // `selected_bg` tint).

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
            Span::styled("Last: ", text_primary()),
            Span::raw(format!("{}  ", elapsed)),
            Span::styled("Tools: ", text_primary()),
            Span::raw(session.tool_count.to_string()),
        ];
        let right_len: usize = right_spans.iter().map(|s| s.width()).sum();
        let dir_label_len = 6; // "Dir:  "
        let max_dir = w.saturating_sub(right_len + dir_label_len + 1);

        let dir_display = truncate_with_ellipsis(cwd_display.as_ref(), max_dir);

        lines.push(padded_line(
            vec![
                Span::styled("Dir:  ", text_primary()),
                Span::raw(dir_display),
            ],
            right_spans,
            w,
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Dir:  ", text_primary()),
            Span::raw(cwd_display),
        ]));
    }

    if is_placeholder {
        lines.push(Line::from(Span::styled(
            "Launch an agent to get started",
            text_primary(),
        )));
    } else {
        let prompts = collect_recent_prompts(session, density.max_prompts());
        for (i, prompt) in prompts.iter().enumerate() {
            let prefix = if i == 0 { "Prmt: " } else { "      " };
            let max_prompt = w.saturating_sub(6);
            let display = truncate_with_ellipsis(prompt, max_prompt);
            lines.push(Line::from(vec![
                Span::styled(prefix, text_primary()),
                Span::raw(display),
            ]));
        }
    }

    if !wide {
        lines.push(Line::from(vec![
            Span::styled("Last: ", text_primary()),
            Span::raw(format!("{}  ", elapsed)),
            Span::styled("Tools: ", text_primary()),
            Span::raw(session.tool_count.to_string()),
        ]));
    }

    if density != CardDensity::Compact {
        lines.push(Line::from(""));
    }
    let tool_lines = recent_tool_lines(session, density.max_tools());
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
            .map(|l| Line::from(Span::styled(l.to_string(), text_primary())))
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

fn recent_tool_lines(session: &SessionState, max_tools: usize) -> Vec<Line<'static>> {
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
            Line::styled(text, text_primary())
        })
        .collect()
}

fn status_style(status: &SessionStatus) -> (&str, Style) {
    // PRD #155: status colors are resolved through the centralized palette (the
    // single source of truth), so the deck-card badge/border and the
    // embedded-pane border agree for the same state. The label and the
    // attention-grabbing BOLD on "Needs Input" stay here (presentation), only
    // the color moves to the palette role.
    let style = Style::default().fg(palette::status_color(status));
    match status {
        SessionStatus::Thinking => ("Thinking", style),
        SessionStatus::Working => ("Working", style),
        SessionStatus::Compacting => ("Compacting", style),
        SessionStatus::WaitingForInput => ("Needs Input", style.add_modifier(Modifier::BOLD)),
        SessionStatus::Idle => ("Idle", style),
        SessionStatus::Error => ("Error", style),
        // PRD #162 forward-compat: an unknown wire status renders with the
        // neutral idle label/color so a future daemon's status never shows as
        // a misleading active state on an older TUI.
        SessionStatus::Unknown => ("Idle", style),
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

/// Shared L1-seam plumbing: build a `TestBackend` of the caller-given
/// size, run `render` against its `Frame`, and return the resulting
/// buffer. Centralizes the `TestBackend`/`Terminal`/draw/clone
/// boilerplate (and its `.expect()` calls) so the per-overlay wrappers
/// below stay one line each.
fn draw_to_buffer<F>(width: u16, height: u16, render: F) -> ratatui::buffer::Buffer
where
    F: FnOnce(&mut Frame),
{
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    terminal
        .draw(render)
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// L1 test seam: render `render_stats_bar` into a standalone Buffer.
/// Mirrors `render_card_to_buffer` so integration tests (which can only
/// reach `pub` items) can pin the stats-bar styling. The underlying
/// `render_stats_bar` stays private; this wrapper just sets up a
/// `TestBackend` of the caller-given size and draws into it.
#[doc(hidden)]
pub fn render_stats_bar_to_buffer(
    stats: &DashboardStats,
    active_mode_name: Option<&str>,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    draw_to_buffer(width, height, |frame| {
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        render_stats_bar(frame, stats, area, active_mode_name);
    })
}

/// PRD #139 M4.1 — render the throwaway experimental footer into `area`.
/// When `features.experimental` is set, draws the exact label
/// `experimental: on`; otherwise renders nothing, leaving the row blank (the
/// pre-feature baseline). Takes `&Features` by reference, matching every
/// existing L1 render seam. The live dashboard calls this only when
/// [`crate::features::show_experimental_footer`] is true (one wrapper, gated
/// at the user-visible seam — CLAUDE.md #9).
fn render_experimental_footer(frame: &mut Frame, features: &Features, area: Rect) {
    if !features.experimental {
        return;
    }
    let line = Line::from(Span::styled("experimental: on", text_primary()));
    frame.render_widget(Paragraph::new(line), area);
}

/// L1 test seam: render the experimental footer into a standalone Buffer.
/// See `render_stats_bar_to_buffer` for the rationale. Renders the exact text
/// `experimental: on` iff `features.experimental`, else a blank row.
#[doc(hidden)]
pub fn render_experimental_footer_to_buffer(
    features: &Features,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    draw_to_buffer(width, height, |frame| {
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        render_experimental_footer(frame, features, area);
    })
}

/// L1 test seam: render `render_quit_confirm` into a standalone Buffer.
/// See `render_stats_bar_to_buffer` for the rationale.
#[doc(hidden)]
pub fn render_quit_confirm_to_buffer(
    selected: usize,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    draw_to_buffer(width, height, |frame| {
        render_quit_confirm(frame, selected);
    })
}

/// L1 test seam: render `render_stop_confirm` into a standalone Buffer.
/// See `render_stats_bar_to_buffer` for the rationale.
#[doc(hidden)]
pub fn render_stop_confirm_to_buffer(
    selected: usize,
    agent_count: usize,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    draw_to_buffer(width, height, |frame| {
        render_stop_confirm(frame, selected, agent_count);
    })
}

/// L1 test seam: render `render_star_prompt` into a standalone Buffer.
/// See `render_stats_bar_to_buffer` for the rationale.
#[doc(hidden)]
pub fn render_star_prompt_to_buffer(width: u16, height: u16) -> ratatui::buffer::Buffer {
    draw_to_buffer(width, height, |frame| {
        render_star_prompt(frame);
    })
}

/// L1 test seam: render `render_config_gen_prompt` into a standalone Buffer.
/// See `render_stats_bar_to_buffer` for the rationale.
#[doc(hidden)]
pub fn render_config_gen_prompt_to_buffer(
    selected: usize,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    draw_to_buffer(width, height, |frame| {
        render_config_gen_prompt(frame, selected);
    })
}

/// L1 harness helper — render exactly one session card at the requested
/// density into a fresh `TestBackend` buffer and return it for snapshot
/// assertions.
///
/// Wraps the internal `render_session_card` so the L1 snapshot test in
/// `tests/render_dashboard.rs` can pin a card's text layout without
/// re-implementing the renderer. See PRD #77 catalog entry
/// `dashboard/pane/004`. The `selected` flag drives the renderer's
/// selection-highlight path so tests can pin the highlight styling
/// (PRD #13 `theme/guard/001`).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn render_card_to_buffer(
    session: &SessionState,
    display_name: Option<&str>,
    card_number: Option<u8>,
    density: CardDensityKind,
    tick: u64,
    selected: bool,
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
                selected,
                display_name_owned.as_ref(),
                card_number,
                density.into(),
                None,
            );
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #83 M3/M4 seam: render a vertical stack of dashboard cards into a
/// `Buffer` for L1 tests, highlighting the card at `selected`. Mirrors
/// [`render_card_to_buffer`] but lays out multiple cards exactly as the live
/// dashboard does, so a test can assert that the selection highlight follows
/// the derived index (see `sync_and_derive_selection`). PRD #113: `selected`
/// is now `Option<usize>` — `Some(i)` paints the highlight on card `i`, `None`
/// (an inactive selection) paints no highlight at all.
#[doc(hidden)]
pub fn render_dashboard_cards_to_buffer(
    cards: &[(&SessionState, Option<&str>)],
    selected: Option<usize>,
    density: CardDensityKind,
    tick: u64,
    width: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // `render_session_card` computes `wide` from the inner width (full
    // width minus the two border columns); mirror that so the card height
    // we size the buffer with matches what the renderer actually draws.
    let wide = (width as usize).saturating_sub(2) >= 60;
    let card_height = density.rendered_height(wide);
    let height = card_height.saturating_mul(cards.len() as u16).max(1);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let card_density: CardDensity = density.into();
    let owned: Vec<(&SessionState, Option<String>)> = cards
        .iter()
        .map(|(s, name)| (*s, name.map(str::to_string)))
        .collect();
    terminal
        .draw(|frame| {
            let constraints: Vec<Constraint> = (0..owned.len())
                .map(|_| Constraint::Length(card_height))
                .collect();
            let chunks = Layout::vertical(constraints).split(Rect {
                x: 0,
                y: 0,
                width,
                height,
            });
            for (flat_index, (session, display_name)) in owned.iter().enumerate() {
                let card_number = {
                    let n = flat_index + 1;
                    if n <= 9 { Some(n as u8) } else { None }
                };
                render_session_card(
                    frame,
                    chunks[flat_index],
                    session,
                    tick,
                    selected == Some(flat_index),
                    display_name.as_ref(),
                    card_number,
                    card_density,
                    None,
                );
            }
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
pub fn render_button_bar_to_buffer(width: u16) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let mut ui = UiState::new(DashboardConfig::default(), KeybindingConfig::default());
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

/// PRD #40 L1 seam: render the dashboard button bar (global + dashboard
/// context buttons) against an arbitrary [`KeybindingConfig`] into a `Buffer`,
/// so a test can assert the button labels track a remapped config. Mirrors
/// [`render_hints_bar_to_buffer`]'s shape. `has_pane_control` is `true` and the
/// dashboard context buttons are included so every remappable label is
/// exercised; the bar is one row but `height` is honored for layout headroom.
pub fn render_button_bar_with_bindings_to_buffer(
    keybindings: &KeybindingConfig,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    // Seam renders the full global+context bar (including the always-on
    // Scheduled Tasks button) so every remappable label is exercised.
    let ctx_buttons = dashboard_context_buttons(keybindings, true);
    let mut ui = UiState::new(DashboardConfig::default(), keybindings.clone());
    terminal
        .draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height,
            };
            render_bottom_bar(frame, &mut ui, area, true, &ctx_buttons);
        })
        .expect("TestBackend draw should succeed");
    terminal.backend().buffer().clone()
}

/// PRD #80 M6 L1 seam: render the filter-mode bottom row (the inline filter
/// input carrying `filter_text`) into a one-row `Buffer`. After M6 this row
/// also renders the inline `[Apply]` / `[Cancel]` buttons at its right edge.
/// Mirrors [`render_button_bar_to_buffer`].
pub fn render_filter_bar_to_buffer(filter_text: &str, width: u16) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let mut ui = UiState::new(DashboardConfig::default(), KeybindingConfig::default());
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
pub fn render_rename_bar_to_buffer(rename_text: &str, width: u16) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).expect("TestBackend should construct");
    let mut ui = UiState::new(DashboardConfig::default(), KeybindingConfig::default());
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
            render_tab_strip(frame, area, &owned, closeable, active_index);
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

// The quit-confirm / config-gen / star-prompt L1 seams are the
// `#[doc(hidden)]` ones defined earlier (PRD #13 Phase 2); the per-modal seams
// below cover the surfaces PRD #80 added on top.

/// PRD #80 M5 L1 seam: render the help overlay (default keybindings). The
/// PRD #40 keybinding-aware seam is [`render_help_overlay_with_bindings_to_buffer`].
pub fn render_help_overlay_to_buffer(width: u16, height: u16) -> ratatui::buffer::Buffer {
    let keybindings = KeybindingConfig::default();
    render_overlay_to_buffer(width, height, |frame| {
        let _ = render_help_overlay(frame, &keybindings, None);
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
) -> ratatui::buffer::Buffer {
    let mut picker = DirPickerState::new(start);
    render_overlay_to_buffer(width, height, |frame| {
        render_dir_picker(frame, &mut picker);
    })
}

/// PRD #80 M8 L1 seam: render the new-pane form with the given `modes` as
/// selectable mode options into a `Buffer`. Drives the production
/// `render_new_pane_form` through a `TestBackend`; after M8 the form renders
/// clickable mode chips and `[Submit]` / `[Cancel]` buttons, which this
/// seam's buffer then shows. Mirrors [`render_button_bar_to_buffer`].
pub fn render_new_pane_form_to_buffer(
    mode_names: &[&str],
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    let modes: Vec<ModeConfig> = mode_names
        .iter()
        .map(|n| ModeConfig {
            name: (*n).to_string(),
            init_command: None,
            seed_prompt: None,
            panes: Vec::new(),
            rules: Vec::new(),
            reactive_panes: 0,
        })
        .collect();
    let form = NewPaneFormState::new(
        std::path::PathBuf::from("/tmp/project"),
        "myname".to_string(),
        "mycmd".to_string(),
        modes,
        Vec::new(),
    );
    render_overlay_to_buffer(width, height, |frame| {
        render_new_pane_form(frame, &form);
    })
}

/// PRD #170 (unify) L1 seam: render the new-pane form MODE-LOCKED to schedule
/// authoring into a `Buffer`. `edit` picks the variant — `false` builds the Add
/// form (` New Schedule `, no edit row), `true` builds the Edit form
/// (` Edit Schedule `, a dummy existing row). The locked form shows only Dir +
/// Command (no Mode cycler, no Name field). Drives the production
/// `render_new_pane_form` through a `TestBackend` — mirrors
/// [`render_new_pane_form_to_buffer`].
pub fn render_new_pane_form_schedule_to_buffer(
    edit: bool,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    let existing = edit.then(|| config::ScheduledTask {
        name: "digest".to_string(),
        cron: "0 9 * * *".to_string(),
        working_dir: "/tmp/project".to_string(),
        command: Some("cat".to_string()),
        prompt: "digest prompt".to_string(),
        new_tab_per_fire: false,
        enabled: true,
        issue_dispatch: None,
    });
    let form = NewPaneFormState::new_schedule_locked(
        std::path::PathBuf::from("/tmp/project"),
        "claude".to_string(),
        existing,
    );
    render_overlay_to_buffer(width, height, |frame| {
        render_new_pane_form(frame, &form);
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
    use spec::spec;
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
        let (rows, cols) =
            orchestration_role_pane_dims(Rect::new(0, 0, 100, 30), 2, 0, PaneLayout::Tiled, false);
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
            orchestration_role_pane_dims(area, 3, 0, PaneLayout::Tiled, false);
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
        let r0 = orchestration_role_pane_dims(area, 4, 0, PaneLayout::Tiled, true);
        let r1 = orchestration_role_pane_dims(area, 4, 1, PaneLayout::Tiled, true);
        let r2 = orchestration_role_pane_dims(area, 4, 2, PaneLayout::Tiled, true);
        let r3 = orchestration_role_pane_dims(area, 4, 3, PaneLayout::Tiled, true);
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
            orchestration_role_pane_dims(area, 3, 0, PaneLayout::Stacked, false);
        let (rows_unfocused, _) =
            orchestration_role_pane_dims(area, 3, 1, PaneLayout::Stacked, false);
        assert!(rows_focused > rows_unfocused);
        assert_eq!(rows_unfocused, 0);
    }

    // PRD #84 M4 removed the `focused_role_index` parameter (and the resize
    // sweep that threaded a live focused-role index through this helper), so
    // the Stacked expanded slot is always role 0. The two tests that pinned the
    // `focused_role_index = Some(non-zero)` behavior are gone with the param;
    // the renderer-match drift guard below now pins role 0's expanded height.

    #[test]
    fn orchestration_role_pane_dims_stacked_role_zero_matches_renderer_expanded_height() {
        // Drift guard: the helper's expanded-row height for the Stacked
        // expanded slot (role 0) must equal the renderer's expanded height.
        // The renderer gives the expanded slot `Constraint::Fill(1)` after
        // carving 1-row title bars off the other (count-1) slots — i.e. the
        // expanded inner height = main_height - (count-1) - 2 (border).
        let area = Rect::new(0, 0, 200, 50);
        let role_count: u16 = 4;
        let chrome_rows: u16 = 1; // hints bar; no tab bar in this test
        let main_height = area.height.saturating_sub(chrome_rows);
        let expanded_outer = main_height.saturating_sub(role_count - 1);
        let expanded_inner = expanded_outer.saturating_sub(2);
        let (helper_rows, _) =
            orchestration_role_pane_dims(area, role_count as usize, 0, PaneLayout::Stacked, false);
        assert_eq!(helper_rows, expanded_inner);
    }

    #[test]
    fn orchestration_role_pane_dims_zero_role_count_does_not_divide_by_zero() {
        // Defensive: role_count = 0 (transient state during a tab
        // teardown). Clamp to 1 so the helper returns a sane value.
        let (rows, cols) =
            orchestration_role_pane_dims(Rect::new(0, 0, 100, 30), 0, 0, PaneLayout::Tiled, false);
        // main_height = 29, count = 1, chunk = 29, rows = 27.
        // right_width = 66, cols = 64.
        assert_eq!((rows, cols), (27, 64));
    }

    // PRD #84 (deferred render/layout/004) — pin `compute_frame_layout`, the
    // single per-frame layout pass that is now the source of truth BOTH the
    // pre-draw `resize_panes_to_layout` (PTY sizing) and `render_frame` read.
    // These exercise the named rects of a representative Cards layout and a
    // Mode layout for a fixed 100x40 frame. Plain unit tests of a private
    // pure-data fn — not `#[spec]`/CATALOG reproducers.

    #[test]
    fn compute_frame_layout_cards_two_panes_geometry() {
        // 100x40 frame, tab bar shown, 2 dashboard panes, Tiled pane column.
        let frame_area = Rect::new(0, 0, 100, 40);
        let tab_view = ActiveTabView::Dashboard {
            exclude_pane_ids: vec![],
        };
        let tab_bar = TabBarInfo {
            show: true,
            labels: vec!["Dashboard".into(), "Mode".into()],
            active_index: 0,
        };
        let pane_ids = vec!["p0".to_string(), "p1".to_string()];
        // A 1-row bottom bar (this fixture exercises the split math, not the
        // PRD #144 wrap height — that is covered by `render/layout/004`).
        let layout = compute_frame_layout(
            frame_area,
            &tab_view,
            &tab_bar,
            &pane_ids,
            PaneLayout::Tiled,
            None,
            1,
        );

        // Vertical chrome: a 1-row tab bar at the top, a 1-row hints bar at the
        // bottom, the main content in between.
        assert_eq!(layout.tab_bar, Some(Rect::new(0, 0, 100, 1)));
        assert_eq!(layout.hints, Rect::new(0, 39, 100, 1));

        let FrameContent::Cards {
            dashboard_area,
            panes_area,
            pane_ids: laid_out_ids,
            pane_rects,
        } = layout.content
        else {
            panic!("Dashboard tab must produce FrameContent::Cards");
        };

        // Main area (rows 1..39, height 38) splits 33% / 67% horizontally: the
        // card grid on the left, the pane column on the right, together
        // spanning the full width with no gap.
        assert_eq!(dashboard_area, Rect::new(0, 1, 33, 38));
        let panes_area = panes_area.expect("two panes => a right pane column");
        assert_eq!(panes_area, Rect::new(33, 1, 67, 38));
        assert_eq!(dashboard_area.width + panes_area.width, frame_area.width);
        assert_eq!(panes_area.x, dashboard_area.width);
        assert_eq!(laid_out_ids, pane_ids);

        // Tiled: the pane column divides equally, keyed by pane id in order,
        // stacked top-to-bottom (pane 1 starts where pane 0 ends).
        assert_eq!(
            pane_rects,
            vec![
                ("p0".to_string(), Rect::new(33, 1, 67, 19)),
                ("p1".to_string(), Rect::new(33, 20, 67, 19)),
            ]
        );
    }

    #[test]
    fn compute_frame_layout_mode_geometry() {
        // 100x40 frame, tab bar shown, agent pane + 2 stacked side panes.
        let frame_area = Rect::new(0, 0, 100, 40);
        let tab_view = ActiveTabView::Mode {
            mode_name: "demo".to_string(),
            agent_pane_id: "agent".to_string(),
            side_pane_ids: vec!["s0".to_string(), "s1".to_string()],
            focused_pane_id: None,
        };
        let tab_bar = TabBarInfo {
            show: true,
            labels: vec!["Dashboard".into(), "demo".into()],
            active_index: 1,
        };
        let layout = compute_frame_layout(
            frame_area,
            &tab_view,
            &tab_bar,
            &[],
            PaneLayout::Tiled,
            None,
            1,
        );

        assert_eq!(layout.tab_bar, Some(Rect::new(0, 0, 100, 1)));
        assert_eq!(layout.hints, Rect::new(0, 39, 100, 1));

        let FrameContent::Mode {
            agent_area,
            side_area,
            agent_pane_id,
            side_pane_rects,
        } = layout.content
        else {
            panic!("Mode tab must produce FrameContent::Mode");
        };

        // 50 / 50 horizontal split of the main area: agent pane left, side
        // panes right.
        assert_eq!(agent_area, Rect::new(0, 1, 50, 38));
        assert_eq!(side_area, Rect::new(50, 1, 50, 38));
        assert_eq!(agent_pane_id, "agent");

        // Side panes: equal vertical division of the right half, keyed in
        // order — the source for `ui.side_pane_rects`.
        assert_eq!(
            side_pane_rects,
            vec![
                ("s0".to_string(), Rect::new(50, 1, 50, 19)),
                ("s1".to_string(), Rect::new(50, 20, 50, 19)),
            ]
        );
    }

    // PRD #144 — `modal_rect` is the shared content-driven modal sizer: clamp the
    // desired content dims into `[min, 90% of terminal]`, never exceeding the
    // terminal bounds, and center the result. Pure data, so a plain unit test.
    #[test]
    fn modal_rect_clamps_to_min_max_and_terminal() {
        let term = Rect::new(0, 0, 200, 60);

        // Small content clamps UP to the per-modal minimum, centered.
        let small = modal_rect(10, 5, term, 40, 12);
        assert_eq!((small.width, small.height), (40, 12));
        assert_eq!((small.x, small.y), ((200 - 40) / 2, (60 - 12) / 2));

        // Large content is capped at 90% of the terminal in each axis.
        let large = modal_rect(500, 500, term, 40, 12);
        assert_eq!((large.width, large.height), (180, 54)); // 90% of 200 / 60
        assert!(large.width <= term.width && large.height <= term.height);

        // A narrow/short terminal clamps the modal to fit (the 90% cap wins over
        // the larger minimum), and the centered rect stays within the bounds.
        let narrow = Rect::new(0, 0, 50, 20);
        let clamped = modal_rect(500, 500, narrow, 72, 16);
        assert_eq!((clamped.width, clamped.height), (45, 18)); // 90% of 50 / 20
        assert!(clamped.x + clamped.width <= narrow.width);
        assert!(clamped.y + clamped.height <= narrow.height);
    }

    // PRD #144 — `layout_button_bar` greedily wraps the full-label buttons across
    // rows when they don't fit one row, keeping every label (no shortcut chips).
    #[test]
    fn layout_button_bar_wraps_when_row_overflows() {
        // Three 10-wide buttons + 1-space separators = 32 cols. At 32 they fit on
        // one row; at 31 the third wraps to a second row.
        let widths = [10u16, 10, 10];

        let (fit, rows) = layout_button_bar(&widths, 32);
        assert_eq!(rows, 1, "32 cols fits all three on one row");
        assert_eq!(fit[0], Rect::new(0, 0, 10, 1));
        assert_eq!(fit[1], Rect::new(11, 0, 10, 1)); // after a 1-col separator
        assert_eq!(fit[2], Rect::new(22, 0, 10, 1));

        let (wrapped, rows) = layout_button_bar(&widths, 31);
        assert_eq!(rows, 2, "31 cols forces the third button onto a second row");
        assert_eq!(wrapped[2], Rect::new(0, 1, 10, 1));

        // Empty input occupies no rows.
        assert_eq!(layout_button_bar(&[], 80), (Vec::new(), 0));
    }

    // PRD #144 A2 — `bottom_bar_rows` must never reserve so many rows that the
    // main content area is starved to zero. At an extreme-narrow width the
    // full-label bar wraps to one row per button (9 on the dashboard), far more
    // than a short frame is tall; the reservation is capped at `frame_height - 1`
    // so at least one content row always remains. Pure data, so a plain unit test.
    #[test]
    fn bottom_bar_rows_caps_to_leave_a_content_row() {
        let ui = UiState::default();
        let tab_view = ActiveTabView::Dashboard {
            exclude_pane_ids: Vec::new(),
        };
        // 8 cols is narrower than any single full button label, so every button
        // wraps onto its own row — the uncapped count (9) exceeds each of these
        // tiny frame heights, exercising the cap.
        for frame_height in 2u16..=9 {
            let rows = bottom_bar_rows(&ui, 8, frame_height, &tab_view);
            assert!(
                rows < frame_height,
                "bottom_bar_rows at frame_height={frame_height} reserved {rows} \
                 rows, leaving no content row (must be <= frame_height - 1)"
            );
            assert!(rows >= 1, "the bar still occupies at least one row");
        }
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
            live: None,
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
                    display_title: None,
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
                    display_title: None,
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
                    display_title: None,
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

    /// PRD #107 follow-up: the user-typed title persisted on each role
    /// pane's `TabMembership::Orchestration.display_title` flows into the
    /// hydration bucket so reattach can restore it. A leading slot that
    /// omits the title (legacy/dead pane) must not mask a later slot that
    /// carries it — the first non-`None` value wins.
    #[test]
    fn partition_propagates_orchestration_display_title_first_non_none_wins() {
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
                    display_title: None, // leading slot omits the title
                }),
            ),
            hydrated(
                "2",
                "a-2",
                Some("/proj"),
                Some(TabMembership::Orchestration {
                    name: "tdd-cycle".into(),
                    role_index: 1,
                    role_name: "coder".into(),
                    is_start_role: false,
                    orchestration_cwd: Some(orch_cwd.clone()),
                    display_title: Some("My Custom Run".into()),
                }),
            ),
        ];
        let p = partition_hydrated_panes(&panes);
        assert_eq!(p.orchestration_buckets.len(), 1);
        assert_eq!(
            p.orchestration_buckets[0].display_title.as_deref(),
            Some("My Custom Run"),
            "a later slot's display_title backfills a leading None slot"
        );
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
                    display_title: None,
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
                    display_title: None,
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
                display_title: None,
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
                    display_title: None,
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
                    display_title: None,
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
        let result =
            tm.open_orchestration_tab_with_existing_role_panes(&cfg, "/work", bad_vec, None);
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
                    display_title: None,
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
                    display_title: None,
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
                    display_title: None,
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
                    display_title: None,
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
                    display_title: None,
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
                    display_title: None,
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
                    display_title: None,
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
            .open_orchestration_tab_with_existing_role_panes(
                &cfg,
                &bucket.cwd,
                role_pane_ids,
                bucket.display_title.as_deref(),
            )
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
                    display_title: None,
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
                    display_title: None,
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
            display_title: None,
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
        // PRD #155: status colors now resolve through the centralized palette
        // (single source of truth). LOCKED mapping: Working=Green,
        // Thinking=Blue, WaitingForInput=Yellow, Error=Red, Idle=DarkGray, with
        // Compacting sharing the thinking/Blue role.
        let (label, style) = status_style(&SessionStatus::Thinking);
        assert_eq!(label, "Thinking");
        assert_eq!(style.fg, Some(Color::Blue));

        let (label, style) = status_style(&SessionStatus::Working);
        assert_eq!(label, "Working");
        assert_eq!(style.fg, Some(Color::Green));

        let (label, style) = status_style(&SessionStatus::WaitingForInput);
        assert_eq!(label, "Needs Input");
        assert_eq!(style.fg, Some(Color::Yellow));

        let (label, style) = status_style(&SessionStatus::Idle);
        assert_eq!(label, "Idle");
        assert_eq!(style.fg, Some(Color::DarkGray));

        let (label, style) = status_style(&SessionStatus::Error);
        assert_eq!(label, "Error");
        assert_eq!(style.fg, Some(Color::Red));

        let (label, style) = status_style(&SessionStatus::Compacting);
        assert_eq!(label, "Compacting");
        assert_eq!(style.fg, Some(Color::Blue));
    }

    #[test]
    fn build_pane_status_keys_by_pane_id_and_excludes_pane_less() {
        // PRD #155 (R2): `build_pane_status` is the M3 deck/pane border join —
        // the map render_frame uses to color each managed pane by its agent
        // status. Guard the join directly: every pane-bearing session maps its
        // `pane_id` to its own status, and a pane-less session (`pane_id ==
        // None`) is dropped. Two pane-bearing sessions carry DISTINCT statuses
        // so a broken key can't silently swap colors or leave a managed pane on
        // the dimmed (idle) border.
        let mut working = make_session(SessionStatus::Working);
        working.session_id = "s-working".into();
        working.pane_id = Some("pane-w".into());

        let mut waiting = make_session(SessionStatus::WaitingForInput);
        waiting.session_id = "s-waiting".into();
        waiting.pane_id = Some("pane-y".into());

        // Pane-less: distinct status (Thinking) so its absence is unambiguous.
        let mut paneless = make_session(SessionStatus::Thinking);
        paneless.session_id = "s-paneless".into();
        paneless.pane_id = None;

        let mut state = AppState::default();
        state.sessions.insert("s-working".into(), working);
        state.sessions.insert("s-waiting".into(), waiting);
        state.sessions.insert("s-paneless".into(), paneless);

        let map = build_pane_status(&state);

        // Exactly the two pane-bearing sessions appear, each keyed by its own
        // pane id and mapped to its own status.
        assert_eq!(
            map.len(),
            2,
            "only pane-bearing sessions appear in the join"
        );
        assert_eq!(
            map.get("pane-w"),
            Some(&SessionStatus::Working),
            "the working pane keeps its working status"
        );
        assert_eq!(
            map.get("pane-y"),
            Some(&SessionStatus::WaitingForInput),
            "the waiting pane keeps its waiting status"
        );
        // The pane-less session contributes no key (no pane to color) — and its
        // Thinking status never leaks into the map.
        assert!(
            !map.values().any(|s| *s == SessionStatus::Thinking),
            "the pane-less session must be absent from the join"
        );
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
                // PRD #84: render_frame now reads a precomputed FrameLayout
                // (one layout pass per frame); compute it from the same inputs.
                let tab_view = ActiveTabView::Dashboard {
                    exclude_pane_ids: vec![],
                };
                let tab_bar = TabBarInfo {
                    show: false,
                    labels: vec!["Dashboard".into()],
                    active_index: 0,
                };
                let layout = compute_frame_layout(
                    frame.area(),
                    &tab_view,
                    &tab_bar,
                    &[],
                    PaneLayout::Stacked,
                    None,
                    1,
                );
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &tab_view,
                    &tab_bar,
                    &layout,
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
                // PRD #84: render_frame now reads a precomputed FrameLayout
                // (one layout pass per frame); compute it from the same inputs.
                let tab_view = ActiveTabView::Dashboard {
                    exclude_pane_ids: vec![],
                };
                let tab_bar = TabBarInfo {
                    show: false,
                    labels: vec!["Dashboard".into()],
                    active_index: 0,
                };
                let layout = compute_frame_layout(
                    frame.area(),
                    &tab_view,
                    &tab_bar,
                    &[],
                    PaneLayout::Stacked,
                    None,
                    1,
                );
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &tab_view,
                    &tab_bar,
                    &layout,
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
            display_name: None,
        };

        let lines = recent_tool_lines(&session, 3);
        assert_eq!(lines.len(), 3);
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text[0], "  Write — out.txt");
        assert_eq!(text[1], "  Bash");
        assert_eq!(text[2], "  Grep — pattern");

        // Compact mode: only 1 tool (most recent)
        let lines_compact = recent_tool_lines(&session, 1);
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
                // PRD #84: render_frame now reads a precomputed FrameLayout
                // (one layout pass per frame); compute it from the same inputs.
                let tab_view = ActiveTabView::Dashboard {
                    exclude_pane_ids: vec![],
                };
                let tab_bar = TabBarInfo {
                    show: false,
                    labels: vec!["Dashboard".into()],
                    active_index: 0,
                };
                let layout = compute_frame_layout(
                    frame.area(),
                    &tab_view,
                    &tab_bar,
                    &[],
                    PaneLayout::Stacked,
                    None,
                    1,
                );
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &tab_view,
                    &tab_bar,
                    &layout,
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
                // PRD #84: render_frame now reads a precomputed FrameLayout
                // (one layout pass per frame); compute it from the same inputs.
                let tab_view = ActiveTabView::Dashboard {
                    exclude_pane_ids: vec![],
                };
                let tab_bar = TabBarInfo {
                    show: false,
                    labels: vec!["Dashboard".into()],
                    active_index: 0,
                };
                let layout = compute_frame_layout(
                    frame.area(),
                    &tab_view,
                    &tab_bar,
                    &[],
                    PaneLayout::Stacked,
                    None,
                    1,
                );
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &tab_view,
                    &tab_bar,
                    &layout,
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
                // PRD #84: render_frame now reads a precomputed FrameLayout
                // (one layout pass per frame); compute it from the same inputs.
                let tab_view = ActiveTabView::Dashboard {
                    exclude_pane_ids: vec![],
                };
                let tab_bar = TabBarInfo {
                    show: false,
                    labels: vec!["Dashboard".into()],
                    active_index: 0,
                };
                let layout = compute_frame_layout(
                    frame.area(),
                    &tab_view,
                    &tab_bar,
                    &[],
                    PaneLayout::Stacked,
                    None,
                    1,
                );
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &tab_view,
                    &tab_bar,
                    &layout,
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
                // PRD #84: render_frame now reads a precomputed FrameLayout
                // (one layout pass per frame); compute it from the same inputs.
                let tab_view = ActiveTabView::Dashboard {
                    exclude_pane_ids: vec![],
                };
                let tab_bar = TabBarInfo {
                    show: false,
                    labels: vec!["Dashboard".into()],
                    active_index: 0,
                };
                let layout = compute_frame_layout(
                    frame.area(),
                    &tab_view,
                    &tab_bar,
                    &[],
                    PaneLayout::Stacked,
                    None,
                    1,
                );
                render_frame(
                    frame,
                    &state,
                    &mut ui,
                    &filtered,
                    0,
                    false,
                    &noop,
                    PaneLayout::Stacked,
                    &tab_view,
                    &tab_bar,
                    &layout,
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
        // PRD #127 M3.2: the built-in "schedule" authoring option is always
        // available, so the Mode field always shows and the form opens on it.
        assert!(form.has_mode_field);
        assert_eq!(form.focused, FormField::Mode);
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
            &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
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
                &KeybindingConfig::default(),
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
                &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
        );
        assert!(matches!(result_y, Action::Continue));
        let result_n = handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut ui,
            0,
            None,
            &KeybindingConfig::default(),
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
            &KeybindingConfig::default(),
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
        ui.selected_index = Some(0);

        // j advances 0 → 1
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, Some(1));

        // Down advances 1 → 2
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, Some(2));

        // wraps 2 → 0
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, Some(0));
    }

    #[test]
    fn handle_normal_key_k_and_up_retreat_with_wrap() {
        let mut ui = default_ui();
        ui.selected_index = Some(0);

        // k from 0 wraps to total - 1
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, Some(2));

        // Up retreats 2 → 1
        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(ui.selected_index, Some(1));
    }

    #[test]
    fn handle_normal_key_jk_no_op_when_no_cards() {
        // `total == 0` (empty dashboard): j/k/Up/Down must not panic
        // and must leave the startup selection on card 0 (still active).
        let mut ui = default_ui();
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Down,
            KeyCode::Up,
        ] {
            let r = handle_normal_key(
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut ui,
                0,
                None,
                &KeybindingConfig::default(),
            );
            assert!(matches!(r, Action::Continue));
            assert_eq!(ui.selected_index, Some(0));
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
        assert_eq!(ui.selected_index, Some(1));
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
        ui.selected_index = Some(0);

        let kb = KeybindingConfig::default();
        let press = |ui: &mut UiState, key: KeyEvent| {
            dispatch_normal_mode_key(key, ui, total, None, &filtered, &pc, &kb);
        };

        // j: 0 → 1, focus mirrored to p1.
        press(
            &mut ui,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        assert_eq!(ui.selected_index, Some(1));
        // Down: 1 → 2, focus mirrored to p2.
        press(&mut ui, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(ui.selected_index, Some(2));
        // k: 2 → 1, focus mirrored to p1.
        press(
            &mut ui,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        );
        assert_eq!(ui.selected_index, Some(1));

        let calls = pc.focused.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &["p1".to_string(), "p2".to_string(), "p1".to_string()],
            "every j/k/Up/Down move must mirror the new selection into focus"
        );
    }

    // -----------------------------------------------------------------------
    // PRD #113 — dashboard selection becomes active/inactive
    //
    // These pin the PRD #113 contract: the blue card highlight is painted
    // ONLY while the dashboard selection is "active". Tab-switching
    // deactivates it; explicit input (`1`-`9`, `j`, `k`, Enter) and the
    // focused-pane sync reactivate it. The tests assert the observable
    // outcome — the highlighted card *index* and whether the highlight is
    // *painted at all* — via the selection state and the dashboard renderer.
    //
    // RED note (TDD): the production API does not exist yet. These tests
    // reference the intended shape — `UiState.selected_index: Option<usize>`
    // (`None` = inactive), `render_dashboard_cards_to_buffer(..)` taking
    // `Option<usize>`, plus the new `dashboard_focus_target` and
    // `reconcile_dashboard_selection` seams — so they fail to compile / fail
    // assertions until the coder lands the feature. See the RED report.
    // -----------------------------------------------------------------------

    /// Stringify the whole rendered buffer (symbol layer) so an assertion can
    /// look for the `▸ ` selection marker the dashboard paints on the
    /// highlighted card.
    fn buf_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Stringify a single rendered row — used to assert the `▸` marker lands on
    /// the FIRST card (whose title row is row 0) rather than just somewhere.
    fn buf_row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        let area = buf.area();
        (0..area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// A minimal mode config with `side_pane_count` persistent side panes —
    /// enough to open a real second tab so a tab-switch can be exercised.
    /// Mirrors the verified helper in `tab.rs`'s test module (private there).
    fn mode_config_local(name: &str, side_pane_count: usize) -> ModeConfig {
        ModeConfig {
            name: name.to_string(),
            init_command: None,
            seed_prompt: None,
            panes: (0..side_pane_count)
                .map(|i| crate::project_config::ModePersistentPane {
                    command: format!("echo side-{i}"),
                    name: Some(format!("side-{i}")),
                    watch: false,
                })
                .collect(),
            rules: Vec::new(),
            reactive_panes: 0,
        }
    }

    /// Pane controller that hands out UNIQUE pane ids (so `open_mode_tab`'s
    /// `activate_mode` registers distinct side panes) and records every
    /// `focus_pane` target. Ported from `tab.rs`'s `MockPaneController`
    /// (private to that module); the existing `RecordingFocusPC` returns an
    /// empty id from `create_pane`, which can't back a real mode tab.
    struct OpenTabPC {
        next: std::sync::Mutex<u32>,
        focused: std::sync::Mutex<Vec<String>>,
        closed: std::sync::Mutex<Vec<String>>,
    }
    impl OpenTabPC {
        fn new() -> Self {
            Self {
                next: std::sync::Mutex::new(0),
                focused: std::sync::Mutex::new(Vec::new()),
                closed: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl crate::pane::PaneController for OpenTabPC {
        fn create_pane(
            &self,
            _cmd: Option<&str>,
            _cwd: Option<&str>,
        ) -> Result<String, crate::pane::PaneError> {
            let mut n = self.next.lock().unwrap();
            let id = format!("mock-pane-{n}");
            *n += 1;
            Ok(id)
        }
        fn write_to_pane(&self, _id: &str, _text: &str) -> Result<(), crate::pane::PaneError> {
            Ok(())
        }
        fn close_pane(&self, id: &str) -> Result<(), crate::pane::PaneError> {
            self.closed.lock().unwrap().push(id.to_string());
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
            "open-tab-mock"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// Build N synthetic dashboard sessions `("s0","p0") .. ("sN","pN")` in a
    /// snapshot, returning the snapshot plus a sorted `(session_id, &session)`
    /// filtered view — the shape the dispatch/focus helpers expect.
    fn dashboard_snapshot(n: usize) -> AppState {
        let mut snapshot = AppState::default();
        for i in 0..n {
            let mut sess = make_session(SessionStatus::Idle);
            sess.session_id = format!("s{i}");
            sess.pane_id = Some(format!("p{i}"));
            snapshot.sessions.insert(format!("s{i}"), sess);
        }
        snapshot
    }

    /// Scenario: Open a second (Mode) tab so a tab switch is possible, arm the
    /// Dashboard's highlight on card 2 (active selection), then drive the real
    /// tab-switch path — `Action::CycleTabNext` away from the Dashboard and
    /// `Action::CycleTabPrev` back. After the round-trip the dashboard
    /// selection must be inactive (`None`) and the rendered cards must carry no
    /// `▸` selection marker, so a card never *looks* selected when nothing is
    /// armed. (PRD #113 M2 / SC1.)
    #[spec("dashboard/selection/005")]
    #[test]
    fn selection_005_tab_switch_clears_highlight() {
        use tokio::sync::RwLock;

        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        tab_manager
            .open_mode_tab(
                &mode_config_local("m", 1),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open a second tab");
        // open_mode_tab leaves the mode tab active — return to the Dashboard.
        assert!(tab_manager.switch_to(0));

        let snapshot = dashboard_snapshot(3);
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        // Active highlight armed on the 2nd card (index 1).
        ui.selected_index = Some(1);
        if let Tab::Dashboard {
            selected_session_id,
        } = tab_manager.active_tab_mut()
        {
            *selected_session_id = Some("s1".to_string());
        }

        let area = Rect::new(0, 0, 80, 24);
        // Switch away from the Dashboard…
        dispatch_action(
            Action::CycleTabNext,
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            area,
        );
        // …and back to it.
        dispatch_action(
            Action::CycleTabPrev,
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            area,
        );

        assert_eq!(
            ui.selected_index, None,
            "a tab switch away from the Dashboard and back must deactivate the highlight"
        );

        // And the renderer paints no selection marker when inactive.
        let cards: [(&SessionState, Option<&str>); 3] = [
            (filtered[0].1, Some("alpha")),
            (filtered[1].1, Some("beta")),
            (filtered[2].1, Some("gamma")),
        ];
        let buf = render_dashboard_cards_to_buffer(
            &cards,
            ui.selected_index,
            CardDensityKind::Normal,
            0,
            80,
        );
        assert!(
            !buf_text(&buf).contains('▸'),
            "no card may carry the `▸` selection marker after a tab switch"
        );
    }

    /// Scenario: With the dashboard selection inactive (`None`), press `j`. The
    /// highlight must land on the FIRST card (index 0) and the selection must
    /// become active. (PRD #113 M3 / SC2.)
    #[spec("dashboard/selection/006")]
    #[test]
    fn selection_006_j_from_inactive_jumps_to_first() {
        let mut ui = default_ui();
        ui.selected_index = None; // inactive

        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(
            ui.selected_index,
            Some(0),
            "`j` from an inactive selection jumps to the first card and activates"
        );
    }

    /// Scenario: With the dashboard selection inactive (`None`), press `k`. The
    /// highlight must land on the LAST card (index `total - 1`) and the
    /// selection must become active. (PRD #113 M3 / SC3.)
    #[spec("dashboard/selection/007")]
    #[test]
    fn selection_007_k_from_inactive_jumps_to_last() {
        let mut ui = default_ui();
        ui.selected_index = None; // inactive

        let r = handle_normal_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(matches!(r, Action::Continue));
        assert_eq!(
            ui.selected_index,
            Some(2),
            "`k` from an inactive selection jumps to the last card and activates"
        );
    }

    /// Scenario: PRD #113 design revision (2026-06-13) — when a deck has no
    /// active highlight (e.g. just after returning from another tab), Enter must
    /// RESTORE the previously-selected card, not jump to card 0. Arm the
    /// dashboard highlight on a NON-first card (index 1), drive a REAL tab
    /// round-trip (Dashboard → Mode → Dashboard via `switch_tab_with_focus`,
    /// using a Mode tab as a non-deck intermediate so only the Dashboard-leave
    /// records the prior selection) which clears the live highlight but must
    /// REMEMBER index 1, then assert Enter still maps to `Action::Focus` and the
    /// focus target (`dashboard_focus_target`) is the remembered card (index 1),
    /// NOT card 0. The active-selection and no-cards targets are unchanged.
    #[spec("dashboard/selection/008")]
    #[test]
    fn selection_008_enter_restores_previous_selection() {
        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let (mode_idx, _side_ids) = tab_manager
            .open_mode_tab(
                &mode_config_local("m", 1),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open a second (mode) tab");
        // open_mode_tab leaves the mode tab active — start on the Dashboard.
        assert!(tab_manager.switch_to(0));

        let snapshot = dashboard_snapshot(3);

        let mut ui = default_ui();
        // Arm the highlight on a NON-first card (index 1).
        ui.selected_index = Some(1);
        if let Tab::Dashboard {
            selected_session_id,
        } = tab_manager.active_tab_mut()
        {
            *selected_session_id = Some("s1".to_string());
        }

        // Real round-trip: Dashboard → Mode → Dashboard. Leaving the Dashboard
        // clears the live highlight but must REMEMBER the prior selection
        // (index 1). The Mode tab is a non-deck intermediate, so the return leg
        // does not overwrite the remembered selection.
        switch_tab_with_focus(&mut tab_manager, mode_idx, &*pc, &snapshot, &mut ui);
        switch_tab_with_focus(&mut tab_manager, 0, &*pc, &snapshot, &mut ui);

        // SC1 still holds: no live highlight on return (selection_011/013/015).
        assert_eq!(
            ui.selected_index, None,
            "the live highlight is still cleared on return (SC1 unchanged)"
        );

        // Enter still maps to the focus action.
        let action = handle_normal_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &KeybindingConfig::default(),
        );
        assert!(
            matches!(action, Action::Focus),
            "Enter on the dashboard maps to Action::Focus"
        );

        // THE REVISION: Enter with an inactive selection RESTORES the
        // previously-selected card (index 1), NOT card 0.
        assert_eq!(
            dashboard_focus_target(&ui, 3),
            Some(1),
            "Enter restores the previously-selected card (index 1), not card 0"
        );

        // Unchanged: an active selection focuses the highlighted card.
        ui.selected_index = Some(2);
        assert_eq!(
            dashboard_focus_target(&ui, 3),
            Some(2),
            "Enter with an active selection focuses the highlighted card"
        );

        // Unchanged: no cards → no focus target.
        assert_eq!(
            dashboard_focus_target(&ui, 0),
            None,
            "Enter is a no-op when there are no cards"
        );
    }

    /// Scenario: A digit jump (`1`-`9` → `focus_deck`) must select card N,
    /// focus its embedded pane, and ACTIVATE the highlight — even when the
    /// selection started inactive. Here the selection begins inactive (`None`);
    /// `focus_deck(1, ..)` then leaves it active on index 1 with the pane
    /// focused and the UI in PaneInput mode. (PRD #113 M3 / SC5.)
    #[spec("dashboard/selection/003")]
    #[test]
    fn selection_003_digit_jump_focuses_and_activates() {
        use tokio::sync::RwLock;

        let snapshot = dashboard_snapshot(3);
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let pc = RecordingFocusPC {
            focused: std::sync::Mutex::new(Vec::new()),
        };
        let mut ids: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        ids.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        ui.selected_index = None; // inactive before the digit jump

        let ok = focus_deck(1, &mut ui, &ids, &snapshot, &state, &pc);
        assert!(ok, "focus_deck must accept an in-range idx");
        assert_eq!(
            ui.selected_index,
            Some(1),
            "a digit jump activates the highlight on the targeted card"
        );
        assert_eq!(ui.mode, UiMode::PaneInput);
        assert_eq!(
            pc.focused.lock().unwrap().as_slice(),
            &["p1".to_string()],
            "the targeted card's pane is focused"
        );
    }

    /// Scenario: While the selection is ACTIVE, `j` and `Down` advance to the
    /// next card and wrap from the last card back to the first — and the
    /// selection stays active throughout. (PRD #113 SC6.)
    #[spec("dashboard/selection/001")]
    #[test]
    fn selection_001_active_j_down_next_with_wrap() {
        let mut ui = default_ui();
        ui.selected_index = Some(0); // active on card 0
        let kb = KeybindingConfig::default();

        // j: 0 → 1
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &kb,
        );
        assert_eq!(ui.selected_index, Some(1));
        // Down: 1 → 2
        handle_normal_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &kb,
        );
        assert_eq!(ui.selected_index, Some(2));
        // j wraps: 2 → 0
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &kb,
        );
        assert_eq!(ui.selected_index, Some(0));
    }

    /// Scenario: While the selection is ACTIVE, `k` and `Up` retreat to the
    /// previous card and wrap from the first card back to the last — and the
    /// selection stays active throughout. (PRD #113 SC6.)
    #[spec("dashboard/selection/002")]
    #[test]
    fn selection_002_active_k_up_prev_with_wrap() {
        let mut ui = default_ui();
        ui.selected_index = Some(0); // active on card 0
        let kb = KeybindingConfig::default();

        // k wraps: 0 → 2
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &kb,
        );
        assert_eq!(ui.selected_index, Some(2));
        // Up: 2 → 1
        handle_normal_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut ui,
            3,
            None,
            &kb,
        );
        assert_eq!(ui.selected_index, Some(1));
    }

    /// Scenario: Focused-pane sync — with the selection inactive, when the
    /// embedded controller's focused pane corresponds to a dashboard session's
    /// pane, the per-frame reconcile reactivates the highlight on that card.
    /// With no matching focused pane, an inactive selection stays inactive
    /// (so a tab-switch deactivation isn't instantly undone). (PRD #113 M4.)
    #[spec("dashboard/selection/009")]
    #[test]
    fn selection_009_focused_pane_sync_reactivates() {
        let filtered: [(&str, Option<&str>); 3] =
            [("s0", Some("p0")), ("s1", Some("p1")), ("s2", Some("p2"))];

        // Focused pane maps to card 1 → reactivate on index 1.
        let mut ui = default_ui();
        ui.selected_index = None; // inactive
        let mut tab = Tab::Dashboard {
            selected_session_id: None,
        };
        reconcile_dashboard_selection(&mut ui, &mut tab, Some("p1"), &filtered);
        assert_eq!(
            ui.selected_index,
            Some(1),
            "a focused dashboard pane reactivates the highlight on its card"
        );

        // No focused dashboard pane → an inactive selection stays inactive.
        let mut ui2 = default_ui();
        ui2.selected_index = None;
        let mut tab2 = Tab::Dashboard {
            selected_session_id: None,
        };
        reconcile_dashboard_selection(&mut ui2, &mut tab2, None, &filtered);
        assert_eq!(
            ui2.selected_index, None,
            "with no focused dashboard pane the selection stays inactive"
        );
    }

    /// Scenario: A freshly-built dashboard is active on card 0 — the startup /
    /// default state — so the renderer paints the `▸` highlight on the first
    /// card (first-launch UX unchanged), while an inactive selection paints
    /// none. (PRD #113 M1.)
    #[spec("dashboard/selection/010")]
    #[test]
    fn selection_010_startup_active_at_card_zero() {
        let ui = default_ui();
        assert_eq!(
            ui.selected_index,
            Some(0),
            "startup default is an ACTIVE selection on card 0"
        );

        let now = chrono::Utc::now();
        let make = |sid: &str, pane: &str| SessionState {
            session_id: sid.to_string(),
            agent_type: AgentType::ClaudeCode,
            cwd: Some("/home/dev/x".to_string()),
            status: SessionStatus::Working,
            active_tool: None,
            started_at: now,
            last_activity: now,
            recent_events: std::collections::VecDeque::new(),
            tool_count: 0,
            last_user_prompt: None,
            first_prompts: Vec::new(),
            pane_id: Some(pane.to_string()),
            agent_id: None,
            display_name: None,
        };
        let s0 = make("s0", "p0");
        let s1 = make("s1", "p1");
        let cards: [(&SessionState, Option<&str>); 2] = [(&s0, Some("alpha")), (&s1, Some("beta"))];

        // Active at index 0 → the FIRST card's title row carries the marker.
        let active = render_dashboard_cards_to_buffer(
            &cards,
            ui.selected_index,
            CardDensityKind::Normal,
            0,
            80,
        );
        assert!(
            buf_row(&active, 0).contains('▸'),
            "startup paints the highlight on the first card"
        );

        // Inactive → no card carries the marker.
        let inactive =
            render_dashboard_cards_to_buffer(&cards, None, CardDensityKind::Normal, 0, 80);
        assert!(
            !buf_text(&inactive).contains('▸'),
            "an inactive selection paints no highlight"
        );
    }

    /// A minimal orchestration config with two roles — enough to open a real
    /// Orchestration tab so the Dashboard→Orchestration→Dashboard path can be
    /// exercised. Mirrors the verified helper in `tab.rs`'s test module.
    fn orch_config_local(name: &str) -> OrchestrationConfig {
        OrchestrationConfig {
            name: name.to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "orchestrator".to_string(),
                    command: "echo orch".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    name: "coder".to_string(),
                    command: "echo coder".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
            ],
        }
    }

    /// Scenario: PRD #113 review finding 1 — switching Dashboard → Orchestration
    /// → Dashboard must leave the Dashboard selection INACTIVE. The Orchestration
    /// tab shares `selected_index` and its always-active per-frame reconcile
    /// re-arms `Some(0)` while that tab is active; tab-switch deactivation fires
    /// only on Dashboard-LEAVE, so the return into the Dashboard does not clear
    /// it. This drives the REAL switch path (`switch_tab_with_focus`) AND the
    /// REAL per-frame `reconcile_dashboard_selection` on each frame — which is
    /// why `selection_005` (Mode tab + direct `dispatch_action`, no per-frame
    /// reconcile) cannot catch this. After the round-trip `selected_index` must
    /// be `None` (SC1: switching to ANY other tab and back leaves no card armed).
    #[spec("dashboard/selection/011")]
    #[test]
    fn selection_011_orchestration_round_trip_clears_highlight() {
        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let (orch_idx, _role_pane_ids) = tab_manager
            .open_orchestration_tab(&orch_config_local("orch"), "/work", None, None, (24, 80))
            .expect("open an orchestration tab");
        // open_orchestration_tab leaves the orchestration tab active — return to
        // the Dashboard for the start state.
        assert!(tab_manager.switch_to(0));

        let snapshot = dashboard_snapshot(3);
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));
        let dashboard_filtered: Vec<(&str, Option<&str>)> = filtered
            .iter()
            .map(|(id, s)| (id.as_str(), s.pane_id.as_deref()))
            .collect();
        // The orchestration tab carries no role sessions in this snapshot; with
        // no focused role pane the orchestration reconcile falls back to
        // `Some(0)` — which is exactly the stale re-arm this test pins.
        let orch_filtered: [(&str, Option<&str>); 0] = [];

        let mut ui = default_ui();
        // Arm the dashboard highlight on the 2nd card (index 1).
        ui.selected_index = Some(1);
        if let Tab::Dashboard {
            selected_session_id,
        } = tab_manager.active_tab_mut()
        {
            *selected_session_id = Some("s1".to_string());
        }

        // Frame 1: Dashboard → Orchestration (leaving the Dashboard).
        switch_tab_with_focus(&mut tab_manager, orch_idx, &*pc, &snapshot, &mut ui);
        // Simulate the orchestration frame's per-frame reconcile.
        reconcile_dashboard_selection(&mut ui, tab_manager.active_tab_mut(), None, &orch_filtered);

        // Frame 2: Orchestration → Dashboard (returning).
        switch_tab_with_focus(&mut tab_manager, 0, &*pc, &snapshot, &mut ui);
        // Simulate the dashboard frame's per-frame reconcile.
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            None,
            &dashboard_filtered,
        );

        assert_eq!(
            ui.selected_index, None,
            "Dashboard → Orchestration → Dashboard must leave the selection inactive (SC1)"
        );
    }

    /// Scenario: PRD #113 review finding 2 — an INACTIVE dashboard selection
    /// means nothing is armed, so the close-pane action must be a NO-OP. With
    /// three cards and `selected_index = None`, dispatching `Action::CloseSelected`
    /// must NOT fall back to closing card 0: no `close_pane` call is issued and
    /// no session is removed (matching the `dashboard/pane/003` "never close
    /// without a real selection" invariant).
    #[spec("dashboard/selection/012")]
    #[test]
    fn selection_012_inactive_close_is_noop() {
        use tokio::sync::RwLock;

        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let snapshot = dashboard_snapshot(3);
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        ui.selected_index = None; // inactive — nothing armed

        let sessions_before = state.blocking_read().sessions.len();
        let area = Rect::new(0, 0, 80, 24);
        dispatch_action(
            Action::CloseSelected,
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            area,
        );

        assert!(
            pc.closed.lock().unwrap().is_empty(),
            "an inactive selection must not close any card (no fallback to card 0)"
        );
        assert_eq!(
            state.blocking_read().sessions.len(),
            sessions_before,
            "no session may be removed when the selection is inactive"
        );
    }

    /// Scenario: PR #151 e2e regression (e2e_render_contract::layout_002) — the
    /// inactive-selection close no-op (selection_012) must NOT suppress closing a
    /// Mode/Orchestration TAB via Ctrl+W. With a Mode tab active and
    /// `selected_index == None` (the real condition on a Mode tab — nothing armed
    /// on the dashboard), dispatching `Action::CloseSelected` must close that tab;
    /// likewise for an active Orchestration tab. Today this FAILS: the close
    /// routes through the dashboard-selection gate that short-circuits on `None`,
    /// so the tab persists (the keyboard Ctrl+W tab-close regressed while the
    /// mouse click-to-close path, which bypasses the gate, still works).
    #[spec("dashboard/selection/016")]
    #[test]
    fn selection_016_inactive_selection_does_not_block_tab_close() {
        use tokio::sync::RwLock;

        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let snapshot = AppState::default();
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        // No dashboard cards armed — the close must come from the active tab,
        // not a selected card.
        let filtered: Vec<(&String, &SessionState)> = Vec::new();
        let area = Rect::new(0, 0, 80, 24);

        let mut ui = default_ui();

        // --- Mode tab: the exact case the e2e (layout_002) caught. ---
        tab_manager
            .open_mode_tab(
                &mode_config_local("m", 1),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open a mode tab");
        // open_mode_tab leaves the Mode tab active.
        assert_eq!(tab_manager.tab_count(), 2, "Dashboard + Mode tab");

        ui.selected_index = None; // inactive — nothing armed on the dashboard
        dispatch_action(
            Action::CloseSelected,
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            area,
        );
        assert_eq!(
            tab_manager.tab_count(),
            1,
            "Ctrl+W must close the active MODE tab even when the dashboard selection is inactive (None)"
        );
        assert!(
            matches!(tab_manager.active_tab(), Tab::Dashboard { .. }),
            "after the Mode tab closes the lone Dashboard is active"
        );

        // --- Orchestration tab: same contract (same gate). ---
        tab_manager
            .open_orchestration_tab(&orch_config_local("orch"), "/work", None, None, (24, 80))
            .expect("open an orchestration tab");
        assert_eq!(tab_manager.tab_count(), 2, "Dashboard + Orchestration tab");

        ui.selected_index = None;
        dispatch_action(
            Action::CloseSelected,
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            area,
        );
        assert_eq!(
            tab_manager.tab_count(),
            1,
            "Ctrl+W must close the active ORCHESTRATION tab even when the dashboard selection is inactive (None)"
        );
    }

    /// Scenario: PR #151 manual-test regression (UNIFIED deck behavior, Issue 1) —
    /// Enter (Action::Focus) on an inactive deck that has a remembered selection
    /// must PAINT the highlight by setting `ui.selected_index = Some(restored
    /// target)`, for BOTH the Orchestration and Dashboard decks. Today
    /// Action::Focus only focuses the pane and never sets `selected_index`; on the
    /// Orchestration deck the role pane is already focused on return, so the
    /// per-frame reconcile (which reactivates only on a focus TRANSITION) never
    /// re-arms the highlight and Enter leaves the deck unhighlighted. Pins the
    /// unified fix: Action::Focus sets `selected_index` from the
    /// `dashboard_focus_target` index for both decks. RED today (stays `None`).
    #[spec("dashboard/selection/017")]
    #[test]
    fn selection_017_enter_paints_highlight_on_both_decks() {
        use tokio::sync::RwLock;

        // ---- Orchestration deck (the manifesting case — currently RED). ----
        {
            let pc = Arc::new(OpenTabPC::new());
            let mut tab_manager = TabManager::new(pc.clone());
            let (_orch_idx, role_pane_ids) = tab_manager
                .open_orchestration_tab(&orch_config_local("orch"), "/work", None, None, (24, 80))
                .expect("open an orchestration tab");

            // Placeholder sessions for the role panes, so the orchestration deck's
            // filtered card list maps each index to a role pane (mirrors run_tui's
            // per-tab scoping at ui.rs:6330).
            let mut snapshot = AppState::default();
            for (i, pid) in role_pane_ids.iter().enumerate() {
                let mut s = make_session(SessionStatus::Idle);
                s.session_id = format!("role{i}");
                s.pane_id = Some(pid.clone());
                snapshot.sessions.insert(format!("role{i}"), s);
            }
            let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
            let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
            filtered.sort_by_key(|(_, s)| {
                s.pane_id
                    .as_ref()
                    .and_then(|pid| role_pane_ids.iter().position(|p| p == pid))
                    .unwrap_or(usize::MAX)
            });

            let mut ui = default_ui();
            ui.selected_index = None; // inactive on return
            ui.last_active_selection = Some(1); // remembered role 1

            // Mirror the real Enter path: resolve the restore target via the SSOT.
            let target = dashboard_focus_target(&ui, filtered.len());
            assert_eq!(
                target,
                Some(1),
                "precondition: the restore target is the remembered role 1"
            );
            let selected_id: Option<String> = target
                .and_then(|i| filtered.get(i))
                .map(|(id, _)| (*id).clone());

            dispatch_action(
                Action::Focus,
                &mut ui,
                &*pc,
                &state,
                &mut tab_manager,
                &snapshot,
                &filtered,
                selected_id.as_deref(),
                Rect::new(0, 0, 80, 24),
            );
            assert_eq!(
                ui.selected_index,
                Some(1),
                "Enter on the ORCHESTRATION deck must paint the highlight \
                 (set selected_index) on the restored role, not leave it None"
            );
        }

        // ---- Dashboard deck (must hold the SAME — unified behavior). ----
        {
            let pc = Arc::new(OpenTabPC::new());
            let mut tab_manager = TabManager::new(pc.clone());
            let snapshot = dashboard_snapshot(3);
            let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
            let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
            filtered.sort_by(|a, b| a.0.cmp(b.0));

            let mut ui = default_ui();
            ui.selected_index = None;
            ui.last_active_selection = Some(1);

            let target = dashboard_focus_target(&ui, filtered.len());
            let selected_id: Option<String> = target
                .and_then(|i| filtered.get(i))
                .map(|(id, _)| (*id).clone());

            dispatch_action(
                Action::Focus,
                &mut ui,
                &*pc,
                &state,
                &mut tab_manager,
                &snapshot,
                &filtered,
                selected_id.as_deref(),
                Rect::new(0, 0, 80, 24),
            );
            assert_eq!(
                ui.selected_index,
                Some(1),
                "Enter on the DASHBOARD deck must paint the highlight on the restored card"
            );
        }
    }

    /// Scenario: PR #151 manual-test regression (UNIFIED deck behavior, Issue 2) —
    /// after a tab round-trip with a remembered selection, the previously-selected
    /// deck's PANE must be re-focused on return (so the remembered region is
    /// shown) while the highlight stays clear (`selected_index == None`) until
    /// Enter. Today the DASHBOARD re-focuses nothing on return (its
    /// `selected_session_id` is cleared on leave and `restore_focus_on_switch_in`
    /// returns `None`), reverting to the first card; the Orchestration deck
    /// already re-focuses its remembered role pane. Pins the unified fix: the
    /// Dashboard leave/return is symmetric with Orchestration. Consistent with
    /// selection_013 (focused pane present on return, highlight `None`). RED today
    /// for the Dashboard (no pane re-focused).
    #[spec("dashboard/selection/018")]
    #[test]
    fn selection_018_return_refocuses_remembered_pane_both_decks() {
        // ---- Dashboard (currently RED: focuses nothing on return). ----
        {
            let pc = Arc::new(OpenTabPC::new());
            let mut tab_manager = TabManager::new(pc.clone());
            let (mode_idx, _side_ids) = tab_manager
                .open_mode_tab(
                    &mode_config_local("m", 1),
                    "/work",
                    "agent-m".to_string(),
                    (24, 80),
                )
                .expect("open a mode tab");
            assert!(tab_manager.switch_to(0)); // start on the Dashboard

            let snapshot = dashboard_snapshot(3);

            let mut ui = default_ui();
            // Arm the dashboard on a non-first card (index 1 -> s1 -> pane p1).
            ui.selected_index = Some(1);
            if let Tab::Dashboard {
                selected_session_id,
            } = tab_manager.active_tab_mut()
            {
                *selected_session_id = Some("s1".to_string());
            }

            // Round-trip Dashboard -> Mode -> Dashboard.
            switch_tab_with_focus(&mut tab_manager, mode_idx, &*pc, &snapshot, &mut ui);
            switch_tab_with_focus(&mut tab_manager, 0, &*pc, &snapshot, &mut ui);

            // The remembered card's pane is re-focused on return (symmetry with
            // the Orchestration deck), so the remembered region is shown.
            assert_eq!(
                pc.focused.lock().unwrap().last().map(String::as_str),
                Some("p1"),
                "the Dashboard must re-focus the remembered card's pane (p1) on return, \
                 like the Orchestration deck"
            );
            // Highlight stays inactive on return (SC1; consistent with
            // selection_011/013/015).
            assert_eq!(
                ui.selected_index, None,
                "the highlight stays inactive on return (no re-arm)"
            );
        }

        // ---- Orchestration (already satisfies it — the target the Dashboard
        // must match). ----
        {
            let pc = Arc::new(OpenTabPC::new());
            let mut tab_manager = TabManager::new(pc.clone());
            let (orch_idx, role_pane_ids) = tab_manager
                .open_orchestration_tab(&orch_config_local("orch"), "/work", None, None, (24, 80))
                .expect("open an orchestration tab");
            let (mode_idx, _side_ids) = tab_manager
                .open_mode_tab(
                    &mode_config_local("m", 1),
                    "/work",
                    "agent-m".to_string(),
                    (24, 80),
                )
                .expect("open a mode tab");
            assert!(tab_manager.switch_to(orch_idx)); // start on the Orchestration deck

            let snapshot = AppState::default();

            let mut ui = default_ui();
            ui.selected_index = Some(1);
            let r1 = role_pane_ids[1].clone();
            if let Tab::Orchestration {
                focused_role_pane_id,
                ..
            } = tab_manager.active_tab_mut()
            {
                *focused_role_pane_id = Some(r1.clone());
            }

            switch_tab_with_focus(&mut tab_manager, mode_idx, &*pc, &snapshot, &mut ui);
            switch_tab_with_focus(&mut tab_manager, orch_idx, &*pc, &snapshot, &mut ui);

            assert_eq!(
                pc.focused.lock().unwrap().last().map(String::as_str),
                Some(r1.as_str()),
                "the Orchestration deck already re-focuses its remembered role pane on return"
            );
            assert_eq!(
                ui.selected_index, None,
                "the highlight stays inactive on return"
            );
        }
    }

    /// Scenario: PRD #113 / PR #151 real-app regression — the blue highlight must
    /// NOT reappear after a Dashboard → Mode → Dashboard round-trip when the
    /// restored focus is steady. A Mode tab's AGENT pane is also a Dashboard card
    /// (only its side panes are in `all_managed_pane_ids`), and switching to a
    /// Mode tab focuses that agent pane while the return to the Dashboard restores
    /// nothing — so the agent pane STAYS focused. This drives the REAL per-frame
    /// `reconcile_dashboard_selection` on both the mode frame and the return
    /// dashboard frame with that SAME focused pane id (no transition); because the
    /// focus did not change, the inactive selection stays inactive (`None`).
    /// `selection_005`/`selection_011` cannot catch this (they drive
    /// `dispatch_action` directly or pass `focused = None`, so the per-frame
    /// reconcile never sees a steady-state focused dashboard card).
    #[spec("dashboard/selection/013")]
    #[test]
    fn selection_013_steady_state_focus_does_not_reactivate() {
        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        // A Mode tab whose AGENT pane id ("agent-m") is also a Dashboard card.
        let (mode_idx, _side_ids) = tab_manager
            .open_mode_tab(
                &mode_config_local("m", 1),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open a mode tab");
        // open_mode_tab leaves the mode tab active — start on the Dashboard.
        assert!(tab_manager.switch_to(0));

        let snapshot = AppState::default();
        // Dashboard cards INCLUDE the Mode agent pane "agent-m" (card index 1) —
        // exactly the real-app condition (the agent pane isn't filtered out).
        let dash_filtered: [(&str, Option<&str>); 3] = [
            ("s0", Some("p0")),
            ("agent-sess", Some("agent-m")),
            ("s2", Some("p2")),
        ];

        let mut ui = default_ui();
        // Arm the highlight on the Dashboard (what the user sees before leaving).
        ui.selected_index = Some(1);
        if let Tab::Dashboard {
            selected_session_id,
        } = tab_manager.active_tab_mut()
        {
            *selected_session_id = Some("agent-sess".to_string());
        }

        // Switch Dashboard → Mode (leaving the Dashboard deactivates the highlight).
        switch_tab_with_focus(&mut tab_manager, mode_idx, &*pc, &snapshot, &mut ui);
        // Mode frame: the agent pane "agent-m" is focused; the per-frame reconcile
        // records it as the focus baseline (a Mode tab doesn't touch the dashboard
        // selection).
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            Some("agent-m"),
            &dash_filtered,
        );

        // Switch Mode → Dashboard. Nothing is restored, so "agent-m" STAYS focused.
        switch_tab_with_focus(&mut tab_manager, 0, &*pc, &snapshot, &mut ui);
        // Return dashboard frame: SAME focused pane "agent-m" as the mode frame —
        // no focus TRANSITION — so the highlight must NOT reappear.
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            Some("agent-m"),
            &dash_filtered,
        );

        assert_eq!(
            ui.selected_index, None,
            "a steady-state restored focus (no transition) must not reactivate the highlight on tab return"
        );
    }

    /// Scenario: PRD #113 M4 guard — the focus-transition fix must NOT
    /// over-suppress legitimate reactivation. From an inactive selection, hold a
    /// non-card pane focused across two frames (a steady-state baseline that does
    /// NOT reactivate), then TRANSITION the focus to a dashboard card and confirm
    /// the highlight reactivates on that card. This is distinct from
    /// `selection_009` (which reactivates on the very first reconcile, a
    /// transition from the `None` baseline): here the transition happens AFTER an
    /// established steady-state baseline, proving the per-frame baseline tracking
    /// still admits a genuine focus change.
    #[spec("dashboard/selection/014")]
    #[test]
    fn selection_014_genuine_transition_after_steady_state_reactivates() {
        let mut ui = default_ui();
        ui.selected_index = None; // inactive
        let mut tab = Tab::Dashboard {
            selected_session_id: None,
        };
        let filtered: [(&str, Option<&str>); 3] =
            [("s0", Some("p0")), ("s1", Some("p1")), ("s2", Some("p2"))];

        // Steady-state baseline: a non-card pane ("side-x") stays focused across
        // two frames — it never maps to a card, so the selection stays inactive.
        reconcile_dashboard_selection(&mut ui, &mut tab, Some("side-x"), &filtered);
        assert_eq!(ui.selected_index, None);
        reconcile_dashboard_selection(&mut ui, &mut tab, Some("side-x"), &filtered);
        assert_eq!(
            ui.selected_index, None,
            "a steady-state non-card focus must not reactivate the highlight"
        );

        // Genuine transition: focus moves to a dashboard card → reactivate.
        reconcile_dashboard_selection(&mut ui, &mut tab, Some("p0"), &filtered);
        assert_eq!(
            ui.selected_index,
            Some(0),
            "a genuine focus transition to a card still reactivates the highlight (M4 preserved)"
        );
    }

    /// Scenario: PRD #113 design revision (2026-06-13) Change 1 — SYMMETRIC
    /// clearing across ALL tab switches, INCLUDING orchestration-to-orchestration.
    /// The Orchestration deck shares `ui.selected_index` (derived from
    /// `Tab::Orchestration.focused_role_pane_id` via `sync_and_derive_selection`),
    /// and `reconcile_dashboard_selection`'s guard
    /// (`focus_reactivates = focus_maps_to_card && focus_changed`) covers
    /// `Tab::Dashboard | Tab::Orchestration`. Part 1 pins the orch → Dashboard →
    /// orch round-trip (the destination restores the SAME role pane — a
    /// steady-state focus, no transition). Part 2 (PR #151 follow-up) pins orch A
    /// → orch B: the destination tab restores focus to a DIFFERENT role pane than
    /// the source, so the first reconcile frame reads it as a focus TRANSITION
    /// and re-arms the highlight unless the switch pre-seeds the focus baseline.
    /// Both cases must leave `selected_index == None` on the destination.
    #[spec("tabs/orchestration/003")]
    #[test]
    fn orchestration_003_highlight_clears_on_tab_round_trip() {
        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let (orch_a_idx, roles_a) = tab_manager
            .open_orchestration_tab(&orch_config_local("orch-a"), "/work", None, None, (24, 80))
            .expect("open orchestration tab A");
        let (orch_b_idx, roles_b) = tab_manager
            .open_orchestration_tab(&orch_config_local("orch-b"), "/work", None, None, (24, 80))
            .expect("open orchestration tab B");

        let snapshot = AppState::default();
        let a1 = roles_a[1].clone();
        // Tab B's start role — what `restore_focus_on_switch_in` focuses on
        // switch-in (a DIFFERENT pane than tab A's `a1`).
        let b_start = roles_b[0].clone();
        // Each orchestration deck's filtered list maps each role to its pane id.
        let orch_a_filtered: Vec<(&str, Option<&str>)> = roles_a
            .iter()
            .map(|p| (p.as_str(), Some(p.as_str())))
            .collect();
        let orch_b_filtered: Vec<(&str, Option<&str>)> = roles_b
            .iter()
            .map(|p| (p.as_str(), Some(p.as_str())))
            .collect();

        let mut ui = default_ui();

        // ---- Part 1: orch A → Dashboard → orch A clears the highlight. ----
        assert!(tab_manager.switch_to(orch_a_idx));
        ui.selected_index = Some(1);
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = tab_manager.active_tab_mut()
        {
            *focused_role_pane_id = Some(a1.clone());
        }
        // Establish the focus baseline on tab A (focused = role 1).
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            Some(a1.as_str()),
            &orch_a_filtered,
        );
        // The restored steady-state focus on return must NOT re-arm it.
        switch_tab_with_focus(&mut tab_manager, 0, &*pc, &snapshot, &mut ui);
        switch_tab_with_focus(&mut tab_manager, orch_a_idx, &*pc, &snapshot, &mut ui);
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            Some(a1.as_str()),
            &orch_a_filtered,
        );
        assert_eq!(
            ui.selected_index, None,
            "orch → Dashboard → orch must clear the highlight (symmetric with the Dashboard)"
        );

        // ---- Part 2 (PR #151 follow-up): orch A → orch B clears the highlight. ----
        // Re-arm on tab A and re-establish the baseline (focused = A's role 1).
        assert!(tab_manager.switch_to(orch_a_idx));
        ui.selected_index = Some(1);
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = tab_manager.active_tab_mut()
        {
            *focused_role_pane_id = Some(a1.clone());
        }
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            Some(a1.as_str()),
            &orch_a_filtered,
        );
        // Switch DIRECTLY to ANOTHER orchestration tab. `restore_focus_on_switch_in`
        // focuses tab B's start role (`b_start`) — a DIFFERENT pane than tab A's
        // `a1`, so the reconcile reads it as a genuine focus transition and re-arms
        // the highlight unless the switch pre-seeds the focus baseline to the
        // restored pane.
        switch_tab_with_focus(&mut tab_manager, orch_b_idx, &*pc, &snapshot, &mut ui);
        reconcile_dashboard_selection(
            &mut ui,
            tab_manager.active_tab_mut(),
            Some(b_start.as_str()),
            &orch_b_filtered,
        );
        assert_eq!(
            ui.selected_index, None,
            "orch A → orch B must clear the highlight (a different restored role pane must not re-arm it)"
        );
    }

    /// Scenario: PRD #113 design revision (2026-06-13) Change 2 — Enter restores
    /// the previously-selected role on the Orchestration deck. The orchestration
    /// deck routes Enter through the same `dashboard_focus_target` SSOT as the
    /// Dashboard. Arm role 1, drive a REAL round-trip via a Mode tab (a non-deck
    /// intermediate: Orchestration → Mode → Orchestration) which clears the live
    /// highlight but must REMEMBER role 1, then assert the Enter focus target is
    /// the remembered role (index 1), NOT role 0.
    #[spec("tabs/orchestration/004")]
    #[test]
    fn orchestration_004_enter_restores_previous_role() {
        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let (orch_idx, role_pane_ids) = tab_manager
            .open_orchestration_tab(&orch_config_local("orch"), "/work", None, None, (24, 80))
            .expect("open an orchestration tab");
        let (mode_idx, _side_ids) = tab_manager
            .open_mode_tab(
                &mode_config_local("m", 1),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open a mode tab");
        // Start on the Orchestration deck.
        assert!(tab_manager.switch_to(orch_idx));

        let snapshot = AppState::default();
        let r1 = role_pane_ids[1].clone();
        let role_count = role_pane_ids.len();

        let mut ui = default_ui();
        // Arm the orchestration highlight on the 2nd role (index 1).
        ui.selected_index = Some(1);
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = tab_manager.active_tab_mut()
        {
            *focused_role_pane_id = Some(r1.clone());
        }

        // Round-trip via a Mode tab (a non-deck intermediate, so only the
        // Orchestration-leave records the prior selection): Orch → Mode → Orch.
        switch_tab_with_focus(&mut tab_manager, mode_idx, &*pc, &snapshot, &mut ui);
        switch_tab_with_focus(&mut tab_manager, orch_idx, &*pc, &snapshot, &mut ui);

        // SC1 still holds for the orchestration deck: no live highlight on return.
        assert_eq!(
            ui.selected_index, None,
            "the orchestration highlight is cleared on return"
        );

        // THE REVISION: Enter on the orchestration deck with an inactive
        // selection RESTORES the previously-selected role (index 1), NOT role 0.
        assert_eq!(
            dashboard_focus_target(&ui, role_count),
            Some(1),
            "Enter restores the previously-selected role (index 1), not role 0"
        );
    }

    /// Scenario: Per-deck independence of the Enter-restore state. Open an
    /// Orchestration deck with THREE roles, arm it on role 1, then leave to the
    /// Dashboard (which records role 1 as the remembered selection) and arm the
    /// Dashboard on card 2. Returning to the (now inactive) Orchestration deck,
    /// Enter must restore the Orchestration's OWN previous role (index 1), NOT
    /// the Dashboard's index 2. Today this FAILS: `last_active_selection` is a
    /// single shared `UiState` field, so leaving the Dashboard clobbers the
    /// orchestration's remembered 1 with the dashboard's 2, and
    /// `dashboard_focus_target` returns the leaked index 2. Pins per-deck
    /// storage of the remembered selection.
    #[spec("tabs/orchestration/005")]
    #[test]
    fn orchestration_005_enter_restore_is_per_deck_not_leaked_from_dashboard() {
        // A THREE-role orchestration so the dashboard's leaked index 2 is a
        // valid role index — otherwise `dashboard_focus_target`'s clamp to
        // `role_count - 1` would mask the leak by coincidentally yielding 1.
        let three_role_orch = OrchestrationConfig {
            name: "orch".to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "orchestrator".to_string(),
                    command: "echo orch".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    name: "coder".to_string(),
                    command: "echo coder".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    name: "reviewer".to_string(),
                    command: "echo reviewer".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
            ],
        };

        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        // TabManager::new seeds the Dashboard at index 0.
        let dashboard_idx = 0;
        let (orch_idx, role_pane_ids) = tab_manager
            .open_orchestration_tab(&three_role_orch, "/work", None, None, (24, 80))
            .expect("open an orchestration tab");
        let role_count = role_pane_ids.len();
        assert_eq!(
            role_count, 3,
            "test needs three roles for a distinguishable leak"
        );

        let snapshot = AppState::default();
        let mut ui = default_ui();

        // Start on the Orchestration deck and arm it on role 1.
        assert!(tab_manager.switch_to(orch_idx));
        ui.selected_index = Some(1);
        if let Tab::Orchestration {
            focused_role_pane_id,
            ..
        } = tab_manager.active_tab_mut()
        {
            *focused_role_pane_id = Some(role_pane_ids[1].clone());
        }

        // Leave to the Dashboard (records the orchestration's role 1), then arm
        // the Dashboard on card 2 (a DIFFERENT index).
        switch_tab_with_focus(&mut tab_manager, dashboard_idx, &*pc, &snapshot, &mut ui);
        ui.selected_index = Some(2);

        // Return to the Orchestration deck. Leaving the Dashboard records its
        // card 2 — into the SAME shared field, clobbering the orchestration's 1.
        switch_tab_with_focus(&mut tab_manager, orch_idx, &*pc, &snapshot, &mut ui);

        // The orchestration highlight is inactive on return (SC1).
        assert_eq!(
            ui.selected_index, None,
            "the orchestration highlight is cleared on return"
        );

        // PER-DECK CONTRACT: Enter on the Orchestration deck restores the
        // orchestration's OWN remembered role (index 1), NOT the Dashboard's
        // leaked index 2.
        assert_eq!(
            dashboard_focus_target(&ui, role_count),
            Some(1),
            "Enter must restore the Orchestration deck's OWN previous role (1), \
             not the Dashboard's selection (2) leaked through the shared \
             last_active_selection field"
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
            display_name: None,
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
        // Spacious=10, Normal=8, Compact=5

        // 1 session, 1 col, plenty of height -> Spacious
        assert_eq!(choose_density(1, 1, 20, true), CardDensity::Spacious);

        // 2 sessions, 2 cols = 1 row, height 10 -> Spacious (1*10=10)
        assert_eq!(choose_density(2, 2, 10, true), CardDensity::Spacious);

        // 2 sessions, 2 cols = 1 row, height 9 -> Normal (1*8=8 fits)
        assert_eq!(choose_density(2, 2, 9, true), CardDensity::Normal);

        // 4 sessions, 2 cols = 2 rows, height 16 -> Normal (2*8=16)
        assert_eq!(choose_density(4, 2, 16, true), CardDensity::Normal);

        // 4 sessions, 2 cols = 2 rows, height 15 -> Compact (2*5=10 fits)
        assert_eq!(choose_density(4, 2, 15, true), CardDensity::Compact);

        // Many sessions, small screen -> Compact
        assert_eq!(choose_density(10, 1, 20, true), CardDensity::Compact);

        // Edge: 0 sessions -> Spacious (0 rows needed)
        assert_eq!(choose_density(0, 1, 10, true), CardDensity::Spacious);
    }

    #[test]
    fn test_choose_density_narrow() {
        // Narrow layout: each mode needs 1 extra row for stats line
        // Spacious=11, Normal=9, Compact=6

        // 1 session, height 11 -> Spacious (1*11=11)
        assert_eq!(choose_density(1, 1, 11, false), CardDensity::Spacious);

        // 1 session, height 10 -> Normal (1*9=9 fits)
        assert_eq!(choose_density(1, 1, 10, false), CardDensity::Normal);

        // 2 sessions, 1 col, height 18 -> Normal (2*9=18)
        assert_eq!(choose_density(2, 1, 18, false), CardDensity::Normal);

        // 2 sessions, 1 col, height 17 -> Compact (2*6=12 fits)
        assert_eq!(choose_density(2, 1, 17, false), CardDensity::Compact);
    }

    /// Acceptance criterion 1: `card_height` is derived from rendered content,
    /// so it returns exactly Compact=5 / Normal=8 / Spacious=10 (wide) and
    /// 6 / 9 / 11 (narrow). Locks the six values against future drift.
    #[test]
    fn card_height_001_content_derived_values() {
        // Wide layout (no inline stats line).
        assert_eq!(CardDensity::Compact.card_height(true), 5);
        assert_eq!(CardDensity::Normal.card_height(true), 8);
        assert_eq!(CardDensity::Spacious.card_height(true), 10);

        // Narrow layout (+1 row for the inline stats line on every tier).
        assert_eq!(CardDensity::Compact.card_height(false), 6);
        assert_eq!(CardDensity::Normal.card_height(false), 9);
        assert_eq!(CardDensity::Spacious.card_height(false), 11);
    }

    /// Review finding S1: the card grid must re-clamp a stale scroll offset
    /// when a resize grows `visible_rows`. While content still overflows the
    /// window a valid offset is left alone; once everything (or more) fits, the
    /// offset is reduced so no rows scroll off the top leaving a blank tail.
    #[test]
    fn clamp_scroll_offset_reclamps_on_resize_grow() {
        // Overflow, offset still valid (7 rows, window 3, max offset 7-3=4):
        // scrolled to 4 stays 4.
        assert_eq!(clamp_scroll_offset(4, 7, 3), 4);

        // Resize grew the window to 10 so all 7 rows fit: the stale offset 4
        // must clamp back to 0 (7.saturating_sub(10) == 0).
        assert_eq!(clamp_scroll_offset(4, 7, 10), 0);
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
            display_name: None,
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
            display_name: None,
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
            display_name: None,
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
            seed_prompt: None,
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
        // PRD #127 M3.2: + the built-in "schedule" authoring option.
        assert_eq!(f.mode_option_count(), 3); // "No mode" + 1 mode + "schedule"

        let f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![],
            vec![],
        );
        assert_eq!(f.mode_option_count(), 2); // "No mode" + "schedule"
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
        // PRD #127 M3.2: index 3 is the built-in "schedule" authoring option.
        f.select_next_mode();
        assert_eq!(f.selection_index, 3);
        assert!(f.is_schedule_selected());

        // Can't go past last (schedule)
        f.select_next_mode();
        assert_eq!(f.selection_index, 3);
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
        assert_eq!(f.mode_option_name(f.selection_index), "No mode");

        f.selection_index = 1;
        assert_eq!(f.selected_mode().unwrap().name, "k8s");
        assert_eq!(f.mode_option_name(f.selection_index), "k8s");

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
        assert_eq!(f.mode_option_name(f.selection_index), "Orch: tdd");

        f.selection_index = 3;
        assert_eq!(f.selected_orchestration().unwrap().name, "review");
        assert_eq!(f.mode_option_name(f.selection_index), "Orch: review");
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
        // 0=No mode, 1=dev, 2=tdd, 3=schedule (PRD #127 M3.2 built-in).
        assert_eq!(f.mode_option_count(), 4);

        f.select_next_mode();
        f.select_next_mode();
        assert_eq!(f.selection_index, 2);
        assert_eq!(f.selected_orchestration().unwrap().name, "tdd");

        // Index 3 is the built-in "schedule" authoring option.
        f.select_next_mode();
        assert_eq!(f.selection_index, 3);
        assert!(f.is_schedule_selected());
        assert!(f.selected_orchestration().is_none());

        // Can't go past last (schedule)
        f.select_next_mode();
        assert_eq!(f.selection_index, 3);
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

    // PRD #127 M3.2: with the built-in "schedule" authoring option, the Mode
    // field is always present even when the project declares no modes — so the
    // field cycle always includes Mode.
    #[test]
    fn unified_form_tab_cycles_with_builtin_schedule_when_no_project_modes() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![],
            vec![],
        );
        assert!(f.has_mode_field);
        assert_eq!(f.focused, FormField::Mode);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Name);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Command);

        f.focused = f.next_field();
        assert_eq!(f.focused, FormField::Mode); // wraps

        f.focused = f.prev_field();
        assert_eq!(f.focused, FormField::Command);
    }

    // --- PRD #127 M3.3: "Scheduled Tasks" manager dialog pure-data helpers ---

    fn make_scheduled_task(name: &str, enabled: bool) -> config::ScheduledTask {
        config::ScheduledTask {
            name: name.to_string(),
            cron: "0 9 * * *".to_string(),
            working_dir: "/tmp".to_string(),
            command: Some("cat".to_string()),
            prompt: format!("{name}-prompt-marker"),
            new_tab_per_fire: false,
            enabled,
            issue_dispatch: None,
        }
    }

    #[test]
    fn manager_status_label_derivation() {
        assert_eq!(schedule_status_label(false, false), "disabled");
        assert_eq!(schedule_status_label(false, true), "disabled"); // disabled wins
        assert_eq!(schedule_status_label(true, true), "live");
        assert_eq!(schedule_status_label(true, false), "idle");
    }

    #[test]
    fn manager_next_fire_display_disabled_is_placeholder() {
        let disabled = make_scheduled_task("paused", false);
        assert_eq!(schedule_next_fire_display(&disabled), "\u{2014}"); // —

        // An enabled task with a valid cron renders a concrete next-fire.
        let enabled = make_scheduled_task("digest", true);
        let next = schedule_next_fire_display(&enabled);
        assert_ne!(next, "\u{2014}");
        assert!(
            next.contains("09:") || next.contains(" 9:"),
            "next-fire should reflect the 09:00 cron, got {next}"
        );
    }

    #[test]
    fn manager_add_authoring_mode_is_blank_base_seed() {
        let mode = build_schedule_authoring_mode(None, std::path::Path::new("/tmp/picked"));
        assert_eq!(mode.name, SCHEDULE_MODE_NAME);
        let seed = mode.seed_prompt.as_deref().unwrap();
        // Add starts from the base seed (invokes `schedule add`, no edit block) —
        // PRD #170 appends the picked-dir working_dir DEFAULT line.
        assert!(
            seed.starts_with(SCHEDULE_AUTHORING_SEED_PROMPT),
            "add seed must begin with the base authoring seed"
        );
        assert!(seed.contains("schedule add"));
        // PRD #170: the picked dir is threaded in as the working_dir DEFAULT.
        assert!(
            seed.contains("working_dir DEFAULT: /tmp/picked"),
            "add seed must carry the picked dir as the working_dir default, got:\n{seed}"
        );
    }

    #[test]
    fn manager_edit_authoring_mode_prefills_and_forbids_rename() {
        // PRD #170 finding 3: the row's stored dir (A) and the re-picked dir (B)
        // are distinct, non-overlapping paths so the assertions below can tell the
        // stale current-value from the picked default.
        let mut existing = make_scheduled_task("digest", true);
        existing.working_dir = "/row/dir/alpha".to_string();
        let mode =
            build_schedule_authoring_mode(Some(&existing), std::path::Path::new("/pick/dir/bravo"));
        assert_eq!(mode.name, SCHEDULE_MODE_NAME);
        let seed = mode.seed_prompt.as_deref().unwrap();
        // PRD #170: the picked dir is threaded in as the working_dir DEFAULT.
        assert!(
            seed.contains("working_dir DEFAULT: /pick/dir/bravo"),
            "edit seed must carry the picked dir as the working_dir default, got:\n{seed}"
        );
        // PRD #170 finding 3: the re-picked dir wins — the row's stale stored
        // working_dir must NOT survive as a conflicting current value.
        assert!(
            !seed.contains("/row/dir/alpha"),
            "the re-picked working_dir must win; the row's stale dir must not appear, got:\n{seed}"
        );
        // Pre-fill: the existing entry's distinctive prompt + name reach the seed.
        assert!(
            seed.contains("digest-prompt-marker"),
            "edit seed must carry the row's current prompt"
        );
        assert!(
            seed.contains("digest"),
            "edit seed must carry the row's name"
        );
        // Edit drives `schedule update`, never `add`-as-rename, and forbids rename.
        assert!(
            seed.contains("schedule update"),
            "edit seed must instruct `schedule update`"
        );
        assert!(
            seed.to_lowercase().contains("rename is forbidden"),
            "edit seed must forbid renaming (name is the reuse key)"
        );
    }

    // PRD #127 N2 — the scroll window keeps the selected row visible.
    #[test]
    fn manager_visible_window_scrolls_selection_into_view() {
        // Fewer rows than the window → show all.
        assert_eq!(visible_window(3, 0, 5), (0, 3));
        assert_eq!(visible_window(3, 2, 5), (0, 3));

        // More rows than the window: selection at the top stays at offset 0.
        assert_eq!(visible_window(10, 0, 4), (0, 4));
        // Selection beyond the first window scrolls so it's the last visible row.
        assert_eq!(visible_window(10, 5, 4), (2, 6));
        // Selection at the very end pins the window to the bottom.
        assert_eq!(visible_window(10, 9, 4), (6, 10));

        // Degenerate inputs.
        assert_eq!(visible_window(0, 0, 4), (0, 0));
        assert_eq!(visible_window(5, 0, 0), (0, 0));
    }

    // PRD #127 N2 — the manager dialog stays OPEN after run-now and after a
    // confirmed delete, so the user can act on multiple rows (the action is
    // dispatched by the main loop, which refreshes the list in place).
    #[test]
    fn manager_run_now_and_delete_keep_dialog_open() {
        let mut ui = default_ui();
        ui.mode = UiMode::ScheduledTasks;
        ui.scheduled_tasks = vec![make_scheduled_task("a", true)];
        ui.scheduled_selected = 0;

        // run-now → emits ScheduleRunNow and leaves the dialog open.
        let r = handle_scheduled_tasks_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut ui,
        );
        assert!(matches!(r, Action::ScheduleRunNow(ref n) if n == "a"));
        assert_eq!(ui.mode, UiMode::ScheduledTasks);

        // d → confirmation (dialog stays open), then y → ScheduleDelete, still open.
        let r = handle_scheduled_tasks_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut ui,
        );
        assert!(matches!(r, Action::Continue));
        assert!(ui.scheduled_delete_confirm);
        assert_eq!(ui.mode, UiMode::ScheduledTasks);

        let r = handle_scheduled_tasks_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut ui,
        );
        assert!(matches!(r, Action::ScheduleDelete(ref n) if n == "a"));
        assert_eq!(ui.mode, UiMode::ScheduledTasks);
        assert!(!ui.scheduled_delete_confirm);
    }

    // PRD #127 M3.2: the new-deck dialog's Mode cycler always ends with a
    // built-in "schedule" authoring option (after the project modes and
    // orchestrations). PRD #170 finding 7: the seed no longer rides on the
    // synthetic mode (that field is dead data, left `None`) — it is derived at
    // submit time by `build_schedule_authoring_mode`, so the SPAWN REQUEST is what
    // carries it. This test pins both: the option is last/selectable, and
    // submitting it produces a seeded request.
    #[test]
    fn unified_form_builtin_schedule_option_is_last_and_seeded() {
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![make_mode("build")],
            vec![make_orchestration("review")],
        );
        // 0=No mode, 1=build, 2=review, 3=schedule.
        assert_eq!(f.schedule_index(), 3);
        assert_eq!(f.mode_option_count(), 4);

        // Cycling Right to the cap lands on the schedule option.
        for _ in 0..10 {
            f.select_next_mode();
        }
        assert!(f.is_schedule_selected());

        // It is a real (synthetic) mode named `schedule`, NOT misread as an
        // orchestration. Finding 7: the synthetic mode no longer carries the seed.
        let seeded = f.selected_mode().expect("schedule yields a mode");
        assert_eq!(seeded.name, "schedule");
        assert!(
            seeded.seed_prompt.is_none(),
            "finding 7: the synthetic mode's seed_prompt is dead data — left None"
        );
        assert!(f.selected_orchestration().is_none());

        // The seed is delivered through the spawn request derived at submit time.
        let req = build_new_pane_request(&f, "claude");
        let seed = req
            .seed_prompt
            .as_deref()
            .expect("submitting the schedule option carries the authoring seed");
        assert!(seed.contains("schedule add"), "seed must invoke the CLI");
        assert!(
            seed.to_lowercase().contains("confirm"),
            "seed must require confirm-before-write"
        );
    }

    // PRD #170 M2.1 (was PRD #127 Part 4) — a blank Command on the "schedule"
    // authoring option must default to the configured `default_command` (not the
    // formerly-hardcoded `claude`, and never a bare $SHELL), for BOTH submit
    // doors: the [Submit] button path (which calls `build_new_pane_request`
    // directly) and the Enter-on-final-field key path.
    #[test]
    fn schedule_blank_command_defaults_to_configured_default_command_both_doors() {
        const DEFAULT_CMD: &str = "configured-agent";

        // Door 1: the [Submit] button path calls build_new_pane_request directly.
        let mut f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(), // blank command
            vec![],
            vec![],
        );
        f.select_next_mode(); // index 0 -> 1 = the built-in "schedule" option
        assert!(f.is_schedule_selected());
        let req = build_new_pane_request(&f, DEFAULT_CMD);
        assert_eq!(
            req.command, DEFAULT_CMD,
            "[Submit] door must default a blank schedule command to default_command, not $SHELL"
        );
        assert!(
            req.seed_prompt.is_some(),
            "authoring seed prompt is carried"
        );

        // Door 2: the Enter-on-final-field key path resolves default_command
        // from `ui.config`.
        let mut ui = default_ui();
        ui.config.default_command = DEFAULT_CMD.to_string();
        ui.mode = UiMode::NewPaneForm;
        ui.new_pane_form = Some(NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(), // blank command
            vec![],
            vec![],
        ));
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_new_pane_form_key(right, &mut ui); // select the schedule option
        assert!(ui.new_pane_form.as_ref().unwrap().is_schedule_selected());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_new_pane_form_key(enter, &mut ui); // Mode -> Name
        handle_new_pane_form_key(enter, &mut ui); // Name -> Command
        let result = handle_new_pane_form_key(enter, &mut ui); // submit
        match result {
            Action::SpawnPane(req) => assert_eq!(
                req.command, DEFAULT_CMD,
                "Enter door must default a blank schedule command to default_command, not $SHELL"
            ),
            other => panic!("expected SpawnPane, got {other:?}"),
        }
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

    // PRD #127 M3.2: the built-in "schedule" authoring option makes the Mode
    // field always present, so the form opens focused on Mode even with no
    // project modes.
    #[test]
    fn unified_form_initial_focus_without_project_modes() {
        let f = NewPaneFormState::new(
            PathBuf::from("/tmp"),
            String::new(),
            String::new(),
            vec![],
            vec![],
        );
        assert_eq!(f.focused, FormField::Mode);
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

    // PRD #107 / orchestration-identity fix: the user-entered name rides on
    // `req.name` (which `dispatch_action` routes to the tab TITLE via
    // `display_title`), while `req.orchestration_config.name` stays the
    // canonical config name (the IDENTITY). The form must NOT clobber the
    // config name — the override that used to do that was removed. The
    // title/identity decoupling itself is covered end-to-end by
    // `identity_001` (orchestration/identity/001); this test only pins the
    // form handler's output.
    #[test]
    fn orchestration_form_user_name_rides_request_not_config() {
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

        // The form carries the user's input in req.name (later routed to the
        // tab TITLE via display_title).
        assert_eq!(req.name, "user-typed-name");

        // The orchestration config name is left UNTOUCHED — it remains the
        // canonical identity the daemon's delegate lookup compares. The form
        // no longer overrides it (that override was removed by the
        // orchestration-identity fix).
        let orch = req.orchestration_config.expect("orchestration selected");
        assert_eq!(
            orch.name, "config-name",
            "form must not clobber the config name; the user name rides req.name (the TITLE), \
             while orch_config.name stays the canonical IDENTITY"
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

        // With an empty form name there is no display title, so the tab falls
        // back to the canonical config name. Either way the config name on the
        // request is preserved — the form never overrides it.
        let orch = req.orchestration_config.expect("orchestration selected");
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
        let submit_hint = new_pane_form_footer_hint(true, true, false);
        assert!(
            submit_hint.contains("Enter: submit"),
            "expected submit hint, got {submit_hint:?}"
        );

        // Sanity checks: every other focus/visibility combination keeps the
        // legacy 'Enter: next' wording.
        let next_hint = new_pane_form_footer_hint(true, false, false);
        assert!(
            next_hint.contains("Enter: next") && !next_hint.contains("submit"),
            "expected next hint, got {next_hint:?}"
        );
        let no_mode_hint = new_pane_form_footer_hint(false, false, false);
        assert!(
            no_mode_hint.contains("Enter: next/confirm"),
            "expected next/confirm hint when there's no mode field, got {no_mode_hint:?}"
        );

        // PRD #170 finding 6: the mode-locked schedule form has a single
        // navigable field (Command), so it drops the misleading "Tab: switch
        // field" wording for a Command-only confirm/cancel hint.
        let locked_hint = new_pane_form_footer_hint(false, false, true);
        assert!(
            locked_hint.contains("Enter: confirm")
                && locked_hint.contains("Esc: cancel")
                && !locked_hint.contains("Tab"),
            "expected a Command-only locked hint with no Tab wording, got {locked_hint:?}"
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

    // -----------------------------------------------------------------------
    // PRD #107 regression — orchestration IDENTITY must be the canonical
    // config name, NOT the new-pane form's display title (which defaults to
    // the worktree dir basename). See orchestration/identity/001.
    // -----------------------------------------------------------------------

    /// A `PaneController` that records the `TabMembership` handed to every
    /// `create_pane_with_options` call, so a test can read back the
    /// orchestration IDENTITY that `open_orchestration_tab` stamped onto each
    /// role pane — the exact string the daemon later compares in
    /// `state::lookup_orchestration_role`.
    struct CapturingPaneController {
        next: std::sync::Mutex<u32>,
        memberships: std::sync::Mutex<Vec<Option<TabMembership>>>,
    }

    impl CapturingPaneController {
        fn new() -> Self {
            Self {
                next: std::sync::Mutex::new(0),
                memberships: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// The `name` of every `TabMembership::Orchestration` recorded —
        /// i.e. the orchestration identity stamped on each role pane.
        fn recorded_orchestration_names(&self) -> Vec<String> {
            self.memberships
                .lock()
                .unwrap()
                .iter()
                .filter_map(|m| match m {
                    Some(TabMembership::Orchestration { name, .. }) => Some(name.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    impl PaneController for CapturingPaneController {
        fn create_pane_with_options(
            &self,
            _command: Option<&str>,
            _cwd: Option<&str>,
            opts: AgentSpawnOptions<'_>,
        ) -> Result<(String, String), PaneError> {
            self.memberships
                .lock()
                .unwrap()
                .push(opts.tab_membership.clone());
            let mut n = self.next.lock().unwrap();
            let id = format!("pane-{n}");
            *n += 1;
            let resolved = opts.display_name.unwrap_or("role").to_string();
            Ok((id, resolved))
        }
        fn focus_pane(&self, _pane_id: &str) -> Result<(), PaneError> {
            Ok(())
        }
        fn close_pane(&self, _pane_id: &str) -> Result<(), PaneError> {
            Ok(())
        }
        fn list_panes(&self) -> Result<Vec<crate::pane::PaneInfo>, PaneError> {
            Ok(Vec::new())
        }
        fn resize_pane(
            &self,
            _pane_id: &str,
            _direction: crate::pane::PaneDirection,
            _amount: u16,
        ) -> Result<(), PaneError> {
            Ok(())
        }
        fn rename_pane(&self, _pane_id: &str, name: &str) -> Result<RenameOutcome, PaneError> {
            Ok(RenameOutcome::applied(name))
        }
        fn toggle_layout(&self) -> Result<(), PaneError> {
            Ok(())
        }
        fn write_to_pane(&self, _pane_id: &str, _text: &str) -> Result<(), PaneError> {
            Ok(())
        }
        fn name(&self) -> &str {
            "capturing"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// Scenario: Submit the new-pane form for an orchestration opened in a
    /// worktree whose dir basename (`dot-agent-deck-prd-113-foo`) differs from
    /// the TOML config orchestration name (`dot-agent-deck`) — so the form's
    /// Name field defaults to the basename while the loaded config still
    /// carries the canonical name. Dispatch the real `Action::SpawnPane` that
    /// the form produces and assert the IDENTITY stamped onto each role pane's
    /// `TabMembership::Orchestration` (the value the daemon's delegate role
    /// lookup compares) is the CANONICAL config name, not the basename, while
    /// the tab TITLE still shows the basename. Pins the PRD #107 regression
    /// fix: today the SpawnPane override stuffs the basename into
    /// `orch_config.name`, so the identity is the basename and
    /// `lookup_orchestration_role` misses (the `clear = true` respawn is
    /// silently dropped in every worktree).
    #[spec("orchestration/identity/001")]
    #[test]
    fn identity_001_orchestration_identity_is_config_name_not_form_title() {
        const CONFIG_NAME: &str = "dot-agent-deck";
        const FORM_TITLE: &str = "dot-agent-deck-prd-113-foo"; // worktree basename

        // A worktree-like cwd whose basename differs from the config name.
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join(FORM_TITLE);
        std::fs::create_dir_all(&cwd).expect("create cwd");

        // Canonical orchestration config as loaded from disk: name =
        // "dot-agent-deck", a worker role `coder` with clear = true (the
        // respawn the bug silently drops on each delegate).
        let config = OrchestrationConfig {
            name: CONFIG_NAME.to_string(),
            roles: vec![
                OrchestrationRoleConfig {
                    name: "orchestrator".to_string(),
                    command: "echo orch".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                },
                OrchestrationRoleConfig {
                    name: "coder".to_string(),
                    command: "echo coder".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: true,
                },
            ],
        };

        // The submitted new-pane form: its Name field defaults to the dir
        // basename (transition_after_dir_pick), so req.name = FORM_TITLE while
        // orchestration_config still carries the canonical CONFIG_NAME.
        let req = NewPaneRequest {
            dir: cwd.clone(),
            name: FORM_TITLE.to_string(),
            command: String::new(),
            mode_config: None,
            orchestration_config: Some(config),
            seed_prompt: None,
        };

        let pc = Arc::new(CapturingPaneController::new());
        let mut tm = TabManager::new(pc.clone());
        let mut ui = default_ui();
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let snapshot = AppState::default();

        let _ = dispatch_action(
            Action::SpawnPane(Box::new(req)),
            &mut ui,
            pc.as_ref(),
            &state,
            &mut tm,
            &snapshot,
            &[],
            None,
            Rect::new(0, 0, 200, 50),
        );

        // IDENTITY: every role pane's TabMembership must carry the canonical
        // config name — the string the daemon's lookup_orchestration_role
        // compares against the on-disk config's `o.name` to find the role
        // (and thus honor clear = true). The form/basename title must NOT
        // leak into this identity.
        let identities = pc.recorded_orchestration_names();
        assert!(
            !identities.is_empty(),
            "expected the orchestration to stamp a TabMembership identity on each role pane"
        );
        for identity in &identities {
            assert_eq!(
                identity, CONFIG_NAME,
                "orchestration IDENTITY must be the canonical config name so \
                 lookup_orchestration_role resolves and clear=true fires — not \
                 the form/basename title {FORM_TITLE:?}"
            );
        }

        // TITLE: the tab label still shows the user's form name (PRD #107).
        match tm.active_tab() {
            Tab::Orchestration { name, .. } => assert_eq!(
                name, FORM_TITLE,
                "tab TITLE should preserve the form/basename name (PRD #107)"
            ),
            _ => panic!("expected an Orchestration tab to be active after SpawnPane"),
        }
    }

    // -----------------------------------------------------------------------
    // PRD #154 — a single-agent dashboard CARD (no mode, no orchestration)
    // belongs to the Dashboard (tab 0). Creating one from a non-Dashboard tab
    // must switch the active tab back to the Dashboard so the new card isn't
    // stranded on a tab the user isn't viewing. See tabs/spawn/001-003.
    // -----------------------------------------------------------------------

    /// Build the `Action::SpawnPane` request for a *plain single-agent card* —
    /// no mode, no orchestration — i.e. the exact shape the new-pane form
    /// produces for the "regular dashboard pane" branch under test (PRD #154).
    fn plain_card_request(dir: &str) -> NewPaneRequest {
        NewPaneRequest {
            dir: std::path::PathBuf::from(dir),
            name: "card".to_string(),
            command: "echo hi".to_string(),
            mode_config: None,
            orchestration_config: None,
            seed_prompt: None,
        }
    }

    /// The pane id OpenTabPC most recently handed out (its `mock-pane-{n}`
    /// counter minus one) — the brand-new card's pane, since the dashboard
    /// branch creates exactly one pane and focuses it last.
    fn last_created_pane(pc: &OpenTabPC) -> String {
        format!("mock-pane-{}", *pc.next.lock().unwrap() - 1)
    }

    /// Scenario: Open a REAL orchestration tab and leave it active (the
    /// "launched from a non-Dashboard tab" precondition), then dispatch the
    /// single-agent-card `Action::SpawnPane` (no mode, no orchestration). The
    /// card belongs to the Dashboard (tab 0), so afterward the active tab must
    /// be the Dashboard with the new card selected and its pane focused — not
    /// left stranded on the orchestration tab the user launched from (PRD #154).
    #[spec("tabs/spawn/001")]
    #[test]
    fn spawn_001_card_from_orchestration_lands_on_dashboard() {
        use tokio::sync::RwLock;

        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let (orch_idx, _role_ids) = tab_manager
            .open_orchestration_tab(&orch_config_local("orch"), "/work", None, None, (24, 80))
            .expect("open a real orchestration tab");
        // open_orchestration_tab leaves the orchestration tab active — this is
        // exactly the "launched from a non-Dashboard tab" precondition.
        assert_eq!(
            tab_manager.active_index(),
            orch_idx,
            "precondition: the orchestration tab is active before the card spawn"
        );
        assert_ne!(
            orch_idx, 0,
            "precondition: the orchestration tab is not the Dashboard"
        );

        let snapshot = dashboard_snapshot(2);
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        let _ = dispatch_action(
            Action::SpawnPane(Box::new(plain_card_request("/work/card"))),
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            Rect::new(0, 0, 80, 24),
        );

        assert_eq!(
            tab_manager.active_index(),
            0,
            "a single-agent card belongs to the Dashboard (tab 0): the active tab \
             must switch back to the Dashboard, not stay on the orchestration tab"
        );
        assert_eq!(
            ui.selected_index,
            Some(filtered.len()),
            "the new card must be the active selection on the Dashboard"
        );
        let new_card = last_created_pane(&pc);
        assert_eq!(
            pc.focused.lock().unwrap().last().map(String::as_str),
            Some(new_card.as_str()),
            "the new card's pane must be focused after the spawn"
        );
    }

    /// Scenario: Open a REAL mode tab and leave it active, then dispatch the
    /// single-agent-card `Action::SpawnPane` (no mode, no orchestration).
    /// Afterward the active tab must be the Dashboard (tab 0) with the new card
    /// selected and focused — the card must not land on the mode tab the user
    /// launched from (PRD #154; same rule as the orchestration case).
    #[spec("tabs/spawn/002")]
    #[test]
    fn spawn_002_card_from_mode_lands_on_dashboard() {
        use tokio::sync::RwLock;

        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        tab_manager
            .open_mode_tab(
                &mode_config_local("m", 1),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open a real mode tab");
        // open_mode_tab leaves the mode tab active — the non-Dashboard launch
        // precondition.
        assert_ne!(
            tab_manager.active_index(),
            0,
            "precondition: the mode tab is active (not the Dashboard) before the spawn"
        );

        let snapshot = dashboard_snapshot(3);
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        let _ = dispatch_action(
            Action::SpawnPane(Box::new(plain_card_request("/work/card"))),
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            Rect::new(0, 0, 80, 24),
        );

        assert_eq!(
            tab_manager.active_index(),
            0,
            "a single-agent card belongs to the Dashboard (tab 0): the active tab \
             must switch back to the Dashboard, not stay on the mode tab"
        );
        assert_eq!(
            ui.selected_index,
            Some(filtered.len()),
            "the new card must be the active selection on the Dashboard"
        );
        let new_card = last_created_pane(&pc);
        assert_eq!(
            pc.focused.lock().unwrap().last().map(String::as_str),
            Some(new_card.as_str()),
            "the new card's pane must be focused after the spawn"
        );
    }

    /// Scenario: With only the Dashboard tab present (already active), dispatch
    /// the single-agent-card `Action::SpawnPane`. This is the no-regression
    /// guard: the active tab must stay on the Dashboard (tab 0) and the new
    /// card must still be selected and focused (PRD #154). Expected to pass
    /// even pre-fix — it pins that the tab switch never moves off the Dashboard.
    #[spec("tabs/spawn/003")]
    #[test]
    fn spawn_003_card_from_dashboard_stays_on_dashboard() {
        use tokio::sync::RwLock;

        let pc = Arc::new(OpenTabPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        // A fresh TabManager has only the Dashboard, already active.
        assert_eq!(
            tab_manager.active_index(),
            0,
            "precondition: the Dashboard is the active tab"
        );

        let snapshot = dashboard_snapshot(2);
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        let _ = dispatch_action(
            Action::SpawnPane(Box::new(plain_card_request("/work/card"))),
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            Rect::new(0, 0, 80, 24),
        );

        assert_eq!(
            tab_manager.active_index(),
            0,
            "creating a card while already on the Dashboard must leave the \
             Dashboard active (no regression)"
        );
        assert_eq!(
            ui.selected_index,
            Some(filtered.len()),
            "the new card must be the active selection on the Dashboard"
        );
        let new_card = last_created_pane(&pc);
        assert_eq!(
            pc.focused.lock().unwrap().last().map(String::as_str),
            Some(new_card.as_str()),
            "the new card's pane must be focused after the spawn"
        );
    }

    /// Pane controller like `OpenTabPC` (unique `mock-pane-N` ids, records every
    /// `focus_pane`) but it ALSO reports the last-focused pane back through
    /// `focused_pane_id()` — the live process-wide focus a real controller
    /// exposes. `OpenTabPC` leaves `focused_pane_id()` at the trait default
    /// (`None`), which makes `TabManager::capture_focus_on_switch_out` a no-op
    /// (it returns early on `None`), so it can't exercise the switch-out focus
    /// capture under test in `tabs/spawn/004`. This mock can.
    struct FocusEchoPC {
        next: std::sync::Mutex<u32>,
        focused: std::sync::Mutex<Vec<String>>,
    }
    impl FocusEchoPC {
        fn new() -> Self {
            Self {
                next: std::sync::Mutex::new(0),
                focused: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl crate::pane::PaneController for FocusEchoPC {
        fn create_pane(
            &self,
            _cmd: Option<&str>,
            _cwd: Option<&str>,
        ) -> Result<String, crate::pane::PaneError> {
            let mut n = self.next.lock().unwrap();
            let id = format!("mock-pane-{n}");
            *n += 1;
            Ok(id)
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
        /// Echo the last `focus_pane` target as the live focused pane — the
        /// signal `capture_focus_on_switch_out` reads on the way out of a tab.
        fn focused_pane_id(&self) -> Option<String> {
            self.focused.lock().unwrap().last().cloned()
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
            "focus-echo-mock"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// Scenario: Open a REAL Mode tab, focus one of its side panes, then create
    /// a single-agent card from that Mode tab (the plain-card `Action::SpawnPane`
    /// that switches to the Dashboard). Switch back to the Mode tab and restore
    /// its focus: the side pane that was focused at create-time must be
    /// re-focused. Without `capture_focus_on_switch_out()` before the
    /// switch-to-Dashboard, the Mode tab's `focused_pane_id` is never captured,
    /// so restore falls back to the agent pane and the user's prior focus is
    /// lost (PRD #154 follow-up; mirrors the round-trip in `tabs/selection/002`).
    #[spec("tabs/spawn/004")]
    #[test]
    fn spawn_004_card_from_mode_preserves_mode_focus_on_return() {
        use tokio::sync::RwLock;

        let pc = Arc::new(FocusEchoPC::new());
        let mut tab_manager = TabManager::new(pc.clone());
        let (mode_idx, side_ids) = tab_manager
            .open_mode_tab(
                &mode_config_local("m", 2),
                "/work",
                "agent-m".to_string(),
                (24, 80),
            )
            .expect("open a real mode tab");
        // open_mode_tab leaves the mode tab active — the non-Dashboard launch
        // precondition.
        assert_eq!(
            tab_manager.active_index(),
            mode_idx,
            "precondition: the mode tab is active before the spawn"
        );
        assert!(
            side_ids.len() >= 2,
            "precondition: the mode tab has at least two managed side panes"
        );

        // The user focuses a specific (non-default) side pane on the mode tab —
        // this is the live focus that must survive the round-trip.
        let target = side_ids[1].clone();
        pc.focus_pane(&target).unwrap();

        let snapshot = dashboard_snapshot(2);
        let state: SharedState = Arc::new(RwLock::new(snapshot.clone()));
        let mut filtered: Vec<(&String, &SessionState)> = snapshot.sessions.iter().collect();
        filtered.sort_by(|a, b| a.0.cmp(b.0));

        let mut ui = default_ui();
        let _ = dispatch_action(
            Action::SpawnPane(Box::new(plain_card_request("/work/card"))),
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &filtered,
            None,
            Rect::new(0, 0, 80, 24),
        );

        // The card spawn switched the active tab to the Dashboard (tabs/spawn/002)
        // — i.e. we genuinely LEFT the mode tab, which is what should have
        // captured its focus.
        assert_eq!(
            tab_manager.active_index(),
            0,
            "the single-agent card spawn must switch to the Dashboard (so the \
             mode tab is left behind)"
        );

        // Return to the mode tab and restore its remembered focus.
        assert!(tab_manager.switch_to(mode_idx));
        tab_manager.restore_focus_on_switch_in();

        assert_eq!(
            pc.focused.lock().unwrap().last().map(String::as_str),
            Some(target.as_str()),
            "switching back to the mode tab must restore the side pane that was \
             focused when the card was created; without \
             `capture_focus_on_switch_out()` before the switch-to-Dashboard, the \
             mode tab's focused_pane_id was never captured, so restore falls back \
             to the agent pane (`agent-m`) and the user's prior focus is lost"
        );
    }
}
