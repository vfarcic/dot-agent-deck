# PRD #37: Star Repo Reminder Dialog

**Status**: Done
**Priority**: Low
**Created**: 2026-04-03
**GitHub Issue**: [#37](https://github.com/vfarcic/dot-agent-deck/issues/37)

## Problem Statement

Users who enjoy dot-agent-deck may not think to star the GitHub repo. Stars increase project visibility, attract contributors, and signal community trust. Without a gentle reminder, many satisfied users never star — not because they don't want to, but because it doesn't cross their mind.

## Solution Overview

Show a non-intrusive dialog after every N app launches encouraging users to star the repo. The dialog offers two dismissal options:

- **Remind me later** — suppresses the dialog for the next N launches, then shows it again
- **Don't ask again** — permanently hides the dialog

A local state file tracks the launch count and user preference. The dialog follows existing overlay patterns (dir picker, help, new pane form).

## Scope

### In Scope
- Track app launch count in a local state file
- Show a centered dialog overlay after every N launches (default: 10)
- "Remind me later" resets the counter to show again in N more launches
- "Don't ask again" permanently suppresses the dialog
- Dialog includes the repo URL and brief message
- Dialog appears at startup before the main loop takes over
- Keyboard handling: `y` opens the repo URL (if possible) or shows it, `l` for later, `d` for don't ask again, `Esc` for later

### Out of Scope
- Checking whether the user has actually starred the repo (requires GitHub API auth)
- Configuring the launch interval via config.toml (hardcode to a sensible default)
- Analytics or tracking of star dialog interactions
- Showing the dialog mid-session (only at startup)

## Technical Approach

### State File (`~/.config/dot-agent-deck/star-prompt-state.json`)

A small JSON file tracking dialog state:

```json
{
  "launch_count": 0,
  "permanently_dismissed": false,
  "last_prompt_at_launch": 0
}
```

- **`launch_count`**: Incremented on every app launch
- **`permanently_dismissed`**: Set to `true` when user chooses "Don't ask again"
- **`last_prompt_at_launch`**: The launch count when the dialog was last shown

### State Management (`src/config.rs`)

Add a `StarPromptState` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StarPromptState {
    pub launch_count: u64,
    pub permanently_dismissed: bool,
    pub last_prompt_at_launch: u64,
}
```

Methods:
- `load()` — reads from the state file, returns default if missing
- `save()` — writes to the state file
- `increment_and_check()` — bumps `launch_count`, returns `true` if dialog should show (i.e., not permanently dismissed AND `launch_count - last_prompt_at_launch >= PROMPT_INTERVAL`)
- `snooze()` — sets `last_prompt_at_launch = launch_count`
- `dismiss_permanently()` — sets `permanently_dismissed = true`

`PROMPT_INTERVAL` constant: `10` launches.

### Dialog UI (`src/ui.rs`)

Add a new `UiMode::StarPrompt` variant. The dialog follows the existing overlay pattern:

1. Centered rectangle (~50 wide, ~10 tall)
2. `Clear` widget to blank background
3. `Block` with border and title "⭐ Enjoying dot-agent-deck?"
4. Body text: brief message with repo URL
5. Footer with key hints: `[s] Star on GitHub  [l] Later  [d] Don't ask again`

Key handling:
- `s` — opens `https://github.com/vfarcic/dot-agent-deck` via the `open` crate (cross-platform), then dismisses permanently (trusts the user starred)
- `l` or `Esc` — snooze (show again in N launches)
- `d` — dismiss permanently

### Startup Integration (`src/ui.rs` or `src/main.rs`)

Before entering the main event loop:
1. Load `StarPromptState`
2. Call `increment_and_check()`
3. If `true`, set `UiMode::StarPrompt` as the initial UI mode
4. Save state after the dialog is handled

### URL Opening

Use the `open` crate for cross-platform browser URL opening:

```rust
let _ = open::that("https://github.com/vfarcic/dot-agent-deck");
```

## Success Criteria

- Dialog appears on every 10th launch for new users
- "Remind me later" resets the counter correctly
- "Don't ask again" permanently hides the dialog
- Dialog does not appear on launches between intervals
- State persists across app restarts
- Dialog follows existing visual style and overlay patterns
- Pressing `s` opens the GitHub repo URL in the default browser

## Milestones

- [x] Star prompt state file: load/save/increment logic with unit tests
- [x] Star prompt dialog rendering following existing overlay patterns
- [x] Keyboard handling for star/snooze/dismiss actions with URL opening
- [x] Startup integration to check state and show dialog when appropriate
- [x] Tests for state transitions (snooze resets counter, dismiss is permanent, interval logic)
- [x] Manual end-to-end validation across multiple launches
