# PRD #24: Send Prompt to Agent from Dashboard

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-01
**GitHub Issue**: [#24](https://github.com/vfarcic/dot-agent-deck/issues/24)

## Problem Statement

When users want to send a prompt or command to a Claude Code agent, they must switch to that agent's pane, type the prompt, and then switch back to the dashboard. This breaks the multi-agent workflow — the user loses their overview, especially when managing several agents. The dashboard can monitor agents but cannot direct them, making it a read-only control surface.

## Solution Overview

Add a "send prompt" mode to the dashboard that lets users type text and send it to the selected agent's pane via Zellij's `write-chars` action. The text is injected into the agent's terminal as if the user typed it, followed by a newline to submit. This enables full agent control without leaving the dashboard.

### User Flow

1. User selects an agent card with arrow keys / `j`/`k`
2. User presses `s` to enter Send Prompt mode
3. A text input overlay appears (similar to the existing Rename or Filter overlays)
4. User types the prompt text
5. User presses `Enter` to send — the dashboard writes the text to the agent's pane and submits it
6. Dashboard returns to Normal mode; agent processes the prompt
7. Alternatively, user presses `Esc` to cancel without sending

### Quick Actions (Future Enhancement)

Once the core send mechanism works, quick-action shortcuts could be added:
- Predefined prompts triggered by single keys (e.g., `Ctrl+c` to compact)
- A command palette with searchable preset prompts
- History of recently sent prompts

## Scope

### In Scope
- `write_to_pane(pane_id, text)` method on `PaneController` trait
- Zellij implementation using `zellij action write-chars` + `zellij action write 10` (newline)
- New `UiMode::SendPrompt` with text input field
- `s` keybinding in Normal mode to enter Send Prompt mode (when a card is selected)
- Bottom bar shows input prompt with typed text (matching Filter/Rename pattern)
- `Enter` sends the text; `Esc` cancels
- Visual feedback: brief status message after send (success/failure)
- Help overlay updated with `s` keybinding
- Noop implementation returns `NotAvailable` error (same as other pane operations)

### Out of Scope
- Quick-action shortcuts / command palette (future enhancement)
- Prompt history / recall (future enhancement)
- Multi-line prompt input (future enhancement)
- Sending to multiple agents simultaneously (future enhancement)
- Permission prompt responses — covered by PRD #18

## Technical Approach

### PaneController Extension (`src/pane.rs`)

Add a new method to the `PaneController` trait:

```rust
fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError>;
```

**Zellij implementation:**
1. Focus the target pane: `zellij action focus-next-pane` (cycle to target, using existing `focus_pane` logic)
2. Write the text: `zellij action write-chars "{text}"`
3. Send newline: `zellij action write 10`
4. Return focus to the dashboard pane

**Important**: The dashboard must re-focus itself after sending. This requires knowing the dashboard's own pane ID — store it at startup (from `ZELLIJ_PANE_ID` env var or by diffing the focused pane from `list_panes`).

**Noop implementation** returns `PaneError::NotAvailable`.

### UI Mode (`src/ui.rs`)

Add `SendPrompt` variant to `UiMode` enum:

```rust
enum UiMode {
    Normal,
    Filter,
    Help,
    Rename,
    DirPicker,
    NewPaneForm,
    SendPrompt,  // new
}
```

Add `send_prompt_text: String` to `UiState` (following the same pattern as `filter_text` and `rename_text`).

### Key Handling (`src/ui.rs`)

- **Normal mode**: `s` key enters `UiMode::SendPrompt` when a session is selected and has a `pane_id`
- **SendPrompt mode**:
  - Character keys → append to `send_prompt_text`
  - `Backspace` → pop last character
  - `Enter` → send text to pane via `PaneController::write_to_pane`, return to Normal
  - `Esc` → clear text, return to Normal

### Key Result (`src/ui.rs`)

Add a new `KeyResult` variant:

```rust
KeyResult::SendPrompt(String)  // carries the text to send
```

The main event loop matches on this, calls `pane_controller.write_to_pane(pane_id, &text)`, and handles errors.

### Bottom Bar Rendering (`src/ui.rs`)

In `render_bottom_bar`, add a `UiMode::SendPrompt` arm:
- Display: `> {typed_text}` with cursor
- Style matches existing Filter/Rename input pattern

### Dashboard Pane Identity

To return focus after sending, the dashboard needs to know its own pane ID. Options:
- Read `ZELLIJ_PANE_ID` environment variable at startup (if Zellij sets it)
- On first `list_panes` call, find the focused pane and store its ID
- Store as `dashboard_pane_id: Option<String>` in the pane controller or passed to the TUI

### Communication Flow
```
User presses 's' on a card
    → UiMode::SendPrompt activated
    → User types prompt text
    → User presses Enter
    → KeyResult::SendPrompt(text) returned
    → Main loop resolves selected session's pane_id
    → pane_controller.write_to_pane(pane_id, &text)
        → zellij action write-chars "text"
        → zellij action write 10
        → zellij action focus-next-pane (back to dashboard)
    → UiMode::Normal restored
```

## Success Criteria

- User can type a prompt in the dashboard and it appears in the agent's pane as submitted input
- Agent receives and processes the prompt as if the user typed it directly
- Dashboard returns to Normal mode and retains focus after sending
- Works for agents in any status (Idle, WaitingForInput, etc.)
- Sending to a session without a `pane_id` shows an error, doesn't crash
- `Esc` cancels without sending anything
- All existing tests pass
- NoopController gracefully reports unavailability

## Milestones

- [ ] `write_to_pane` method added to `PaneController` trait with Zellij and Noop implementations (`src/pane.rs`)
- [ ] Dashboard pane identity tracked so focus can be restored after write (`src/pane.rs` or `src/ui.rs`)
- [ ] `UiMode::SendPrompt` with text input, key handling, and bottom bar rendering (`src/ui.rs`)
- [ ] `s` keybinding wired up in Normal mode; `Enter` triggers send; `Esc` cancels (`src/ui.rs`)
- [ ] End-to-end flow: type prompt in dashboard → text appears in agent pane → agent processes it
- [ ] Help overlay updated with send prompt keybinding (`src/ui.rs`)
- [ ] Tests for new mode transitions and key handling (`src/ui.rs`)
- [ ] All existing tests passing

## Key Files

- `src/pane.rs` — `write_to_pane` trait method and Zellij implementation
- `src/ui.rs` — `SendPrompt` mode, key handling, bottom bar rendering, help overlay

## Risks

- **Focus management**: Zellij may not reliably return focus to the dashboard pane after writing to another pane. Needs testing. Fallback: user presses a key to refocus, or use `zellij action focus-pane` with the dashboard's pane ID.
- **Special characters**: Text containing quotes, backslashes, or control characters may need escaping for `write-chars`. Need to verify Zellij's handling.
- **Agent not ready**: If the agent is mid-output or in a state that doesn't accept input, the injected text may appear garbled or be ignored. The dashboard should warn if status is not `WaitingForInput` or `Idle`.
- **Zellij version compatibility**: `write-chars` and `write` actions need to be available in supported Zellij versions.
- **Race condition**: Between focus-switch and write-chars, another pane event could steal focus. Using `--pane-id` flag (if available) would be more reliable than focus-then-write.
