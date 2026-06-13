<!-- Source of truth for the harness Test-Case Catalog. Parsed by
     `cargo xtask linkage-check` and `cargo xtask docs` (PRD #77
     Decision 7 / Decision 30). Relocated here from
     prds/77-tui-testing-harness.md so the tooling no longer depends on a
     PRD's location/lifecycle. Entry format: `##### <area>/<sub>/<NNN> — <headline>`
     followed by `- **Layer:** …` bullets; the `## Test Case Catalog`
     heading is the section the parser keys on — keep it. -->

# Test-Case Catalog

## Test Case Catalog

This is the authoritative list of test cases the harness must cover. IDs are stable per Decision 7; tests reference them via `#[spec("…")]` annotations once the harness exists in M2. Coverage is enumerated from the code as it ships today (Decision 27 — "code is authoritative"); documented behaviors with no catalog entry are listed as deliberate skips at the end of this section.

Platform coverage column shorthand: **mac+linux** = macOS and Linux (Windows once the harness's Windows path is ready per Decision 4); **mac+linux+windows** = portable from day one.

### Dashboard panes

#### dashboard/pane

##### dashboard/pane/001 — A pane appears in the next free layout region when an agent is started.
- **Layer:** L2 (PTY end-to-end).
- **Agent:** none (synthetic — `StartAgent` over the daemon protocol with a `sleep infinity` stub).
- **Asserts:** rendered card grid shows one new card; the corresponding pane region is visible on the right column.
- **Does not assert:** card text content beyond the display name, color of the status badge, exact pixel coordinates.
- **Platform coverage:** mac+linux.

##### dashboard/pane/002 — Closing a pane via `Ctrl+w` removes its card from the dashboard.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** card count decreases by one; the focused card index stays within bounds.
- **Does not assert:** which card receives focus next (`dashboard/selection/*` covers selection-after-close).
- **Platform coverage:** mac+linux.

##### dashboard/pane/003 — The dashboard pane (tab 0) is never closable.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** `Ctrl+w` from the dashboard tab with no card selected is a no-op (no panic, dashboard still rendered, tab count unchanged).
- **Does not assert:** any status-line text.
- **Platform coverage:** mac+linux.

##### dashboard/pane/004 — Card title row carries card number, display name, and a status badge.
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** rendered card buffer matches the committed snapshot for a single Working session in the Normal density.
- **Does not assert:** pane content; this is a card layout snapshot only.
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/005 — Dashboard card highlight follows the stable `selected_session_id`, not card 0 (PRD #83 M3).
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** with three session cards and a `Tab::Dashboard` whose `selected_session_id` points at the second card (`sess-beta`), `ui::sync_and_derive_selection` derives index 1 (not 0); the rendered snapshot shows the `▸` selection marker and highlighted border on the second card while the first and third stay unselected.
- **Does not assert:** keyboard-driven selection movement (`dashboard/selection/*`); absolute-time clocks (`Last:` is rendered against a fixed test clock).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/006 — Card row shows `Dir:` (working directory basename), `Last:` (elapsed since last activity), `Tools:` (tool count), `Prmt:` (latest user prompts).
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** rendered card snapshot has all four labels in order with the supplied fixture data.
- **Does not assert:** absolute-time clocks (`Last:` is rendered against a fixed test clock).
- **Platform coverage:** mac+linux+windows.

#### dashboard/density

##### dashboard/density/001 — Spacious density shows up to 3 prompts and 3 tool calls per card.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot rendered with one card in a wide viewport carries the 3+3 capacity.
- **Does not assert:** behavior on Compact / Normal (covered by separate entries).
- **Platform coverage:** mac+linux+windows.

##### dashboard/density/002 — Normal density shows 1 prompt and up to 3 tool calls per card.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot rendered with a card count that lands in the Normal-density tier.
- **Does not assert:** the exact boundary card count between tiers — picked by the layout helper.
- **Platform coverage:** mac+linux+windows.

##### dashboard/density/003 — Compact density shows 1 prompt and 1 tool call per card.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot rendered with a card count that lands in Compact density.
- **Does not assert:** card visual style beyond the rendered character buffer.
- **Platform coverage:** mac+linux+windows.

##### dashboard/density/004 — A rendered card has no trailing blank rows below its content at any density tier (PRD #147).
- **Layer:** L1 (ratatui `TestBackend`, buffer inspection).
- **Agent:** none.
- **Asserts:** a fully-populated session card (3 prompts + 3 tools) rendered at each tier's own `rendered_height` in an 80-column wide viewport has zero blank inner rows between its last content line and the bottom border on Compact, Normal, and Spacious — reserved card height equals rendered content height.
- **Does not assert:** the exact `card_height` value per tier (covered by `card_height_001_content_derived_values`); the mid-card blank separator line on Normal/Spacious (intentional content, not a trailing row).
- **Platform coverage:** mac+linux+windows.

#### dashboard/selection

##### dashboard/selection/001 — `j` / `Down` selects next card; wraps at end.
- **Layer:** L2.
- **Agent:** none (3 synthetic panes).
- **Asserts:** selection indicator moves through cards in order and wraps to the first card after the last.
- **Does not assert:** how the selection indicator is drawn beyond "present at card N".
- **Platform coverage:** mac+linux.

##### dashboard/selection/002 — `k` / `Up` selects previous card; wraps at start.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** selection moves backwards and wraps from card 0 to the last card.
- **Does not assert:** rendering of inactive cards.
- **Platform coverage:** mac+linux.

##### dashboard/selection/003 — `1`–`9` jumps to card N and focuses its pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** keystroke `3` (with 3+ cards) selects card index 2 and the corresponding agent pane gains the focus border.
- **Does not assert:** what `0` or digits past the card count do (kept open until catalogued).
- **Platform coverage:** mac+linux.

##### dashboard/selection/004 — `Esc` clears an active filter.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with the filter dialog populated, pressing `Esc` returns the visible cards to the unfiltered set.
- **Does not assert:** filter dialog dismissal animation.
- **Platform coverage:** mac+linux.

#### dashboard/filter

##### dashboard/filter/001 — `/` opens the filter input; typing narrows visible cards by display-name substring.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after typing two characters that match one of three cards, only that card is rendered.
- **Does not assert:** case-sensitivity flag (covered separately when committed).
- **Platform coverage:** mac+linux.

##### dashboard/filter/002 — `Enter` accepts the filter and leaves the dashboard in the filtered view.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** filter dialog closes; the filtered card list remains; `Esc` then clears it.
- **Does not assert:** subsequent re-open behavior of the filter dialog with prior input restored — not yet specified.
- **Platform coverage:** mac+linux.

#### dashboard/rename

##### dashboard/rename/001 — `r` on the selected card opens a rename input pre-filled with the current name.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** rename input appears with the current display name shown; pressing `Esc` cancels without persisting.
- **Does not assert:** which keystrokes are valid in the input box (covered by `pane/rename/*` validators in the lib pure-data tier).
- **Platform coverage:** mac+linux.

##### dashboard/rename/002 — Confirming a valid new name updates the card title and is mirrored via the daemon `SetAgentLabel` request.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the card title row shows the new name; a subsequent `list_agents` from a parallel daemon client returns the same `display_name`.
- **Does not assert:** persistence across daemon restart (covered by `session/restore/*`).
- **Platform coverage:** mac+linux.

#### dashboard/help

##### dashboard/help/001 — `?` toggles the help overlay; pressing `?`, `Esc`, or `q` dismisses it.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the overlay region is rendered on `?` and removed on dismissal.
- **Does not assert:** the exact list of keys shown in the overlay (compared against a snapshot under `dashboard/help/002`).
- **Platform coverage:** mac+linux.

##### dashboard/help/002 — Help overlay content matches the committed snapshot.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** `insta` file snapshot of the overlay buffer.
- **Does not assert:** dynamic content (none today).
- **Platform coverage:** mac+linux+windows.

#### dashboard/config-gen

##### dashboard/config-gen/001 — `g` on a card opens the Generate Config dialog with options Yes / No / Never.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** dialog region appears; arrow keys move between Yes / No / Never; `Enter` on No dismisses without side effects.
- **Does not assert:** what Yes injects into the agent (covered by `orchestration/delegate/*` for delegate-driven prompt injection, and elsewhere if a non-orchestration path emerges).
- **Platform coverage:** mac+linux.

##### dashboard/config-gen/002 — Picking Never adds the cwd to the suppression list and the prompt does not re-open for that directory.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after Never, re-opening the new-pane flow for the same cwd does not surface the auto-prompt.
- **Does not assert:** filesystem path of the suppression list (an implementation detail).
- **Platform coverage:** mac+linux.

### Statuses

#### status/transition

##### status/transition/001 — Session status transitions to Thinking on `UserPromptSubmit`.
- **Layer:** L2.
- **Agent:** none (synthetic hook event written to the per-test hook socket).
- **Asserts:** card status badge reads Thinking after the hook delivery.
- **Does not assert:** the previous status (covered by predecessor tests).
- **Platform coverage:** mac+linux.

##### status/transition/002 — Session status transitions to Working on `PreToolUse`, carrying the tool name.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads Working; the card's tool row shows the tool's name (e.g. `Read`).
- **Does not assert:** tool-detail formatting beyond presence of the tool name.
- **Platform coverage:** mac+linux.

##### status/transition/003 — Session status transitions to Idle on `Stop`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** card status reads Idle.
- **Does not assert:** flashing-dot animation cadence.
- **Platform coverage:** mac+linux.

##### status/transition/004 — Session status transitions to Error on a hook-reported error.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads Error.
- **Does not assert:** error text content (the hook payload is opaque).
- **Platform coverage:** mac+linux.

##### status/transition/005 — Session status transitions to WaitingForInput on `PermissionRequest`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads WaitingForInput; the card surfaces a `y`/`n` affordance.
- **Does not assert:** tool-detail of the permission (covered under `prompt/permission/*`).
- **Platform coverage:** mac+linux.

##### status/transition/006 — Session status transitions to Compacting on `PreCompact`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads Compacting.
- **Does not assert:** status reverts on `PostCompact` — covered by a follow-up entry.
- **Platform coverage:** mac+linux.

##### status/transition/007 — A `PreToolUse` arriving while WaitingForInput does not override the WaitingForInput badge.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** WaitingForInput sticks until the matching `PostToolUse` or permission resolution.
- **Does not assert:** other badges' precedence rules — covered separately as each is added.
- **Platform coverage:** mac+linux.

#### status/badge

##### status/badge/001 — Status badge color and label render per palette for each session status.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot per status enum value renders the expected label and palette entry.
- **Does not assert:** the dot animation frame.
- **Platform coverage:** mac+linux+windows.

### Prompts

#### prompt/permission

##### prompt/permission/001 — `y` approves the pending permission request and clears the WaitingForInput status.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge transitions away from WaitingForInput; the daemon receives the approval over its protocol channel.
- **Does not assert:** how the daemon routes the approval to the agent process (out-of-scope at the TUI layer).
- **Platform coverage:** mac+linux.

##### prompt/permission/002 — `n` denies the pending permission request.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge transitions away from WaitingForInput; daemon receives a denial.
- **Does not assert:** retry behavior.
- **Platform coverage:** mac+linux.

##### prompt/permission/003 — `y`/`n` are no-ops when no session is waiting for input.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** keystroke produces no protocol traffic and leaves card status unchanged.
- **Does not assert:** any beep or visual ack.
- **Platform coverage:** mac+linux.

#### prompt/pane-input

##### prompt/pane-input/001 — `Enter` on a focused side pane enters PaneInput mode.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the mode line / focus indicator updates to indicate PaneInput mode; a subsequent letter keystroke is forwarded to the side pane's PTY.
- **Does not assert:** the side pane's command output (depends on the fixture shell).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/002 — `Ctrl+d` from PaneInput returns to Normal mode without writing the keystroke to the PTY.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** mode flips back to Normal; the PTY's parsed grid does not gain a stray `^D`.
- **Does not assert:** any toast / status-line message.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/003 — `Ctrl+c` in PaneInput delivers SIGINT (0x03) to the pane's process.
- **Layer:** L2.
- **Agent:** none (fixture: `sh -c 'trap "echo INT" INT; sleep 5'`).
- **Asserts:** the pane PTY shows `INT` after the keystroke, confirming the signal was delivered.
- **Does not assert:** signal handling in the dashboard tab itself (covered by `dashboard/quit/*`).
- **Platform coverage:** mac+linux.

#### prompt/quit

##### prompt/quit/001 — `Ctrl+c` from command mode opens the quit confirmation dialog with three options: **Detach** (default), **Stop**, **Cancel**.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** dialog appears; option list reads `Detach / Stop / Cancel` in that order; the selection cursor starts on Detach (index 0).
- **Does not assert:** local-vs-remote rendering — the dialog is identical (`Detach` is the daemon-attach-aware option in both cases since every pane is daemon-backed).
- **Platform coverage:** mac+linux.

##### prompt/quit/002 — `Ctrl+c` again while the quit dialog is open exits the TUI without sending an explicit `KIND_DETACH` frame.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the harness's spawned binary exits; daemon and managed agents stay alive; no detach frame was observed on the daemon socket.
- **Does not assert:** daemon's eventual idle exit (covered by `lifecycle/daemon-idle/*`).
- **Platform coverage:** mac+linux.

##### prompt/quit/003 — Selecting **Detach** from the quit dialog sends an explicit `KIND_DETACH` frame to the daemon, then exits.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** dialog yields a `KIND_DETACH` frame on the daemon's attach socket before the TUI exits; managed agents stay alive afterwards.
- **Does not assert:** any difference between local and remote daemons — the frame and exit behavior are identical; the observable difference (daemon-side log line) is daemon-side, not deck-side.
- **Platform coverage:** mac+linux.

##### prompt/quit/004 — Selecting **Stop** with managed agents alive opens a secondary confirm dialog (`No` / `Yes`, `No` default) naming the agent count.
- **Layer:** L2.
- **Agent:** none (synthetic — one running stub agent).
- **Asserts:** the secondary dialog appears with header containing `1 managed agent will be terminated`; options read `No / Yes` in that order with `No` selected; pressing `No` returns to the primary `Detach / Stop / Cancel` dialog; pressing `Yes` performs StopAndQuit (daemon and agents terminate).
- **Does not assert:** the singular/plural agent-count wording (loose substring match on the count).
- **Platform coverage:** mac+linux.

##### prompt/quit/005 — Selecting **Stop** with zero managed agents skips the secondary confirm and terminates the daemon directly.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** no secondary dialog appears; the TUI exits and the daemon socket disappears within the grace window.
- **Does not assert:** SIGTERM vs SIGKILL escalation (covered by `lifecycle/stop/003`).
- **Platform coverage:** mac+linux.

#### prompt/dir-picker

##### prompt/dir-picker/001 — `Ctrl+n` opens the new-pane flow; the directory picker is the first step and lists the start directory's entries.
- **Layer:** L2.
- **Agent:** none (fixture with a small directory tree at the harness's redirected `HOME`).
- **Asserts:** the picker appears with the fixture's root entries rendered; the selection cursor starts on the first entry (`..` parent is visible but not selected).
- **Does not assert:** sort order beyond "directories before files" (covered if needed).
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/002 — `j` / `Down` / `k` / `Up` cycle the selected directory; selection wraps end-to-end.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** selection cursor advances through entries; pressing `Up` on the first entry jumps to the last (and vice versa).
- **Does not assert:** rendering of inactive entries beyond presence.
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/003 — `l` / `Right` / `Enter` descend into the selected directory; `h` / `Left` / `Backspace` ascend.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after descending, the picker shows the child directory's contents; after ascending, it shows the parent's contents again.
- **Does not assert:** any breadcrumb / path rendering beyond directory contents.
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/004 — `Space` confirms the current directory and advances to the new-pane form.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the directory picker closes; the new-pane form appears with the chosen directory pre-filled.
- **Does not assert:** the form's default field values (covered by `prompt/new-pane/*`).
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/005 — `/` opens filter mode; typing narrows directories case-insensitively; the `..` parent stays visible.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** filter accepts a substring; only matching directories remain; `..` is rendered regardless of filter.
- **Does not assert:** filter regex syntax (it is plain substring matching).
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/006 — `Esc` clears the active filter; pressing `Esc` again closes the picker.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** first `Esc` empties the filter and restores the full directory list; second `Esc` returns control to the dashboard.
- **Does not assert:** filter input box visibility between key presses.
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/007 — `q` cancels the picker and returns to the dashboard without spawning a pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the picker closes; no new pane appears; daemon `list_agents` is unchanged.
- **Does not assert:** rendering of any toast / status-line message.
- **Platform coverage:** mac+linux.

#### prompt/new-pane

##### prompt/new-pane/001 — The new-pane form opens after the directory picker with three fields visible (Name, Command, Mode) and the initial focus on Name.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the form renders all three field labels; the focus indicator is on the Name field; Mode is set to the default.
- **Does not assert:** the default command string (a configurable `default_command`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/002 — `Tab` and `Shift+Tab` cycle focus forward and backward between fields.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** `Tab` from Name moves focus to Command; another `Tab` moves to Mode; `Shift+Tab` from Mode moves back to Command; cycling wraps at both ends.
- **Does not assert:** which field accepts which input (text vs cycle).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/003 — On the Mode field, `Left` / `Right` / `h` / `l` cycle through the available modes including the default and any project-defined modes / orchestrations.
- **Layer:** L2.
- **Agent:** none (fixture `.dot-agent-deck.toml` defines one mode and one orchestration).
- **Asserts:** cycling from the default shows the mode name, then the orchestration name, then wraps back; the rendered Mode field text follows the cycle.
- **Does not assert:** what happens to other fields while the Mode cycles (Command may be hidden when an orchestration is selected — covered by `prompt/new-pane/004`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/004 — Selecting an orchestration hides the Command field (each role's command is supplied by the config).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with the Mode cycled to an orchestration, the Command label is not rendered; cycling back to a non-orchestration Mode re-renders Command.
- **Does not assert:** what content `Command` had before being hidden (no data loss expected, but not pinned here).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/005 — `Enter` submits the form; the resulting pane (or mode / orchestration tab) is created.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after submit, a card / tab appears that matches the form inputs.
- **Does not assert:** post-submit focus location (covered by `lifecycle/start/*`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/006 — `Esc` cancels the form and returns to the dashboard.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** form closes; no new pane appears; daemon `list_agents` is unchanged.
- **Does not assert:** the dashboard's selection cursor location on return.
- **Platform coverage:** mac+linux.

##### prompt/new-pane/007 — The new-deck dialog surfaces a built-in `schedule` authoring option, visually separated from the workload modes (PRD #127 M3.2).
- **Layer:** L2 (re-sequenced from L1: the dialog renderer + `NewPaneFormState` are private and there is no public L1 render seam, so the real dialog is driven via PTY keystrokes and asserted on the rendered vt100 grid).
- **Agent:** none (drives Ctrl+n → dir-picker → new-pane form, then cycles the Mode field).
- **Asserts:** after cycling the Mode field to the end, the dialog's authoring-session affordance — the `↳`-marked hint that separates `schedule` from the workload modes — renders its FULL text (normalized for grid padding) as exactly `↳ authoring (one-off)` AND stays fully contained within the new-pane modal border (its tail is followed by padding before the right `│`, not clipped by it).
- **Does not assert:** the authoring seed-prompt delivery (covered by `tabs/mode/005`); the manager dialog's add/edit path (Phase 3B-ii); the leading-pad width that aligns the hint under the mode chips.
- **Platform coverage:** mac+linux.

##### prompt/new-pane/008 — Submitting the built-in `schedule` authoring option opens a single-agent dashboard card, not a 50/50 mode tab (PRD #127 bug fix).
- **Layer:** L2 (no public L1 render seam for the dialog or the post-submit layout — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid).
- **Agent:** none (the schedule option's Command field is empty, so the spawn falls back to `$SHELL`; the card-vs-mode-tab layout renders independent of the agent).
- **Asserts:** after cycling the Mode field to the `schedule` option and submitting, the rendered grid shows the dashboard-with-card layout — the dashboard's `dot-agent-deck — N session(s)` title is present (it renders only on the Dashboard tab) AND no `×` tab-close glyph appears — proving the authoring session stayed a single-agent card rather than opening as a separate 50/50 mode tab.
- **Does not assert:** the authoring seed-prompt delivery (covered by `tabs/mode/005`); the exact mode-tab split geometry; the spawned agent's command behavior.
- **Platform coverage:** mac+linux.

##### prompt/new-pane/009 — The built-in `[schedule]` Mode chip stays fully visible inside the modal even when the chip row is wider than the modal (overflow regression guard).
- **Layer:** L2 (no public L1 render seam for the dialog — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid).
- **Agent:** none (drives Ctrl+n → dir-picker → new-pane form, then cycles the Mode field to the `schedule` option).
- **Asserts:** with a fixture defining a workload mode (`build`) plus an orchestration (`ci-deployment`) — so the Mode chip row `  Mode: [No mode] [build] [Orch: ci-deployment] [schedule]` is wider than the capped modal — cycling to and selecting the trailing built-in `[schedule]` option leaves that `[schedule]` chip rendered FULLY between some row's modal borders (`│ … │`), not clipped at the right edge. Approach-agnostic: passes whether the renderer wraps the chip row or windows/scrolls the cycler, as long as the selected chip ends up visible inside the modal.
- **Does not assert:** the exact layout used to keep the chip visible (wrap vs. window/scroll); the visibility of the non-selected chips when the row overflows; the authoring hint text (covered by `prompt/new-pane/007`).
- **Platform coverage:** mac+linux.

### Focus / navigation

#### focus/dashboard

##### focus/dashboard/001 — From command mode, `j` / `k` cycle the selected card; `Enter` is a no-op on the dashboard tab (selection is the source of truth).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** selection moves; pressing `Enter` does not switch tabs or open any dialog from a selected card.
- **Does not assert:** the broken `Enter`-to-jump behavior tracked in [#68](https://github.com/vfarcic/dot-agent-deck/issues/68); see deliberate skips.
- **Platform coverage:** mac+linux.

#### focus/mode-tab

##### focus/mode-tab/001 — `j` / `k` cycle focus through agent → side panes → agent on a mode tab.
- **Layer:** L2.
- **Agent:** none (two persistent side panes from a fixture mode).
- **Asserts:** the cyan focus border moves through panes in order and wraps.
- **Does not assert:** focus during PaneInput mode (PaneInput pins focus on the active pane).
- **Platform coverage:** mac+linux.

##### focus/mode-tab/002 — `Esc` from a focused side pane returns focus to the agent pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** focus indicator jumps to the agent pane region.
- **Does not assert:** focus persistence across tab switches.
- **Platform coverage:** mac+linux.

#### focus/orchestration

##### focus/orchestration/001 — `1`–`9` on an orchestration tab jumps to role pane N and focuses it.
- **Layer:** L2.
- **Agent:** none (orchestration fixture with stub role commands).
- **Asserts:** focused pane index matches the keystroke; the sidebar role-card highlight follows.
- **Does not assert:** what happens beyond the available role count.
- **Platform coverage:** mac+linux.

##### focus/orchestration/002 — Sidebar role cards reflect each role's live status (Thinking / Working / WaitingForInput / Idle / Error).
- **Layer:** L2.
- **Agent:** none (synthetic events targeting two roles).
- **Asserts:** distinct sidebar entries show distinct statuses after distinct hook deliveries.
- **Does not assert:** sidebar layout pixel dimensions.
- **Platform coverage:** mac+linux.

### Modes / tabs

#### tabs/navigation

##### tabs/navigation/001 — `Ctrl+PageDown` / `Ctrl+PageUp` switch tabs from any mode (including from inside a focused pane).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** active tab index advances / retreats; the keystroke is not delivered to the focused pane's PTY.
- **Does not assert:** the tab bar's exact label widths under truncation (covered by `tab_layout` pure-data tests in the lib tier).
- **Platform coverage:** mac+linux.

##### tabs/navigation/002 — `Tab` / `Shift+Tab` switch tabs only in command mode; in PaneInput mode the keystroke reaches the agent PTY.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with PaneInput active, `Tab` is delivered to the pane (parsed grid grows); with command mode active, the tab index advances.
- **Does not assert:** `Left` / `Right` / `h` / `l` aliases — covered by `tabs/navigation/003`.
- **Platform coverage:** mac+linux.

##### tabs/navigation/003 — `Left` / `Right` / `h` / `l` alias `Shift+Tab` / `Tab` in command mode.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** each alias keystroke moves the active tab one step in the documented direction.
- **Does not assert:** any aliases under PaneInput mode (those go to the pane).
- **Platform coverage:** mac+linux.

#### tabs/mode

##### tabs/mode/001 — Selecting a mode on the new-pane form opens a mode tab with the agent pane on the left and persistent side panes stacked on the right.
- **Layer:** L2.
- **Agent:** none (fixture `.dot-agent-deck.toml` with one persistent pane).
- **Asserts:** new tab appears in the tab bar; agent pane is in the left half; side pane region renders on the right.
- **Does not assert:** the side pane's command output content beyond non-empty PTY bytes.
- **Platform coverage:** mac+linux.

##### tabs/mode/002 — `Ctrl+w` on a mode tab tears down the entire workspace (agent + all side panes).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** tab disappears; the daemon's `list_agents` no longer returns the agent that lived in the tab.
- **Does not assert:** side panes' shells receive SIGTERM vs SIGKILL (an implementation detail).
- **Platform coverage:** mac+linux.

##### tabs/mode/003 — Reactive rule routes a matching agent bash command to a reactive side pane.
- **Layer:** L2.
- **Agent:** none (synthetic `PostToolUse` event for a `Bash` tool whose command matches a rule's pattern).
- **Asserts:** the reactive side pane is populated; its title reflects the matched command.
- **Does not assert:** the rule's regex internals (covered by `config_validation` pure-data tests).
- **Platform coverage:** mac+linux.

##### tabs/mode/004 — Once all reactive slots are full, the next match reuses the oldest slot (circular pool).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** three distinct matches against a 2-slot pool leave the second and third matches visible; the first is gone.
- **Does not assert:** slot reuse ordering beyond "oldest first".
- **Platform coverage:** mac+linux.

##### tabs/mode/005 — A `[[modes]]` mode carrying a `seed_prompt` auto-delivers it to the agent pane once the agent is ready (gated, like orchestrations); a mode without one delivers nothing (PRD #127 M3.1).
- **Layer:** L2.
- **Agent:** none — a fixture "recorder" agent that self-posts `SessionStart` (the readiness signal) via the real `dot-agent-deck hook` path, then records every prompt written into its PTY stdin.
- **Asserts:** spawning the seeded mode via the new-pane dialog delivers the configured `seed_prompt` into the agent pane after the agent signals readiness (the marker is recorded); spawning a mode without a `seed_prompt` starts the agent but records no auto-delivered prompt.
- **Does not assert:** which gate path fires (SessionStart fast path vs the slow-path fallback) — only that delivery is gated on readiness, not ungated/immediate; the serde round-trip of `seed_prompt` (covered by a coder unit test).
- **Platform coverage:** mac+linux.

#### tabs/orchestration

##### tabs/orchestration/001 — Selecting an orchestration on the new-pane form opens one pane per role with the orchestrator's pane in focus.
- **Layer:** L2.
- **Agent:** none (orchestration fixture with three stub-command roles, one with `start = true`).
- **Asserts:** the new tab contains three panes; the focused pane is the `start = true` role.
- **Does not assert:** what command is rendered in each pane (the stub fixture is opaque to the harness).
- **Platform coverage:** mac+linux.

##### tabs/orchestration/002 — `Ctrl+w` on an orchestration tab closes the tab and stops every role pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** tab disappears; the daemon no longer carries the role agents.
- **Does not assert:** the order in which roles are closed.
- **Platform coverage:** mac+linux.

#### tabs/selection

##### tabs/selection/001 — Each tab remembers its own selection by stable id across switch-away/switch-back (PRD #83 M1).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none (mock `PaneController`).
- **Asserts:** stamping a distinct stable id on the Dashboard (`selected_session_id`), a Mode tab (`focused_pane_id`), and an Orchestration tab (`focused_role_pane_id`), then switching through every tab and back, leaves each tab holding its own id unchanged — selection is per-tab, not a single global value.
- **Does not assert:** rendering of the selection; focus restore (covered by `tabs/selection/002`).
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/002 — `switch_to` focus restore + capture round-trips a Mode tab's focused pane (PRD #83 M2).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none (mock `PaneController` records `focus_pane` calls).
- **Asserts:** focusing side pane #2 then switching out captures that pane id into the Mode tab; switching back calls `focus_pane` with the stored id; with the field cleared to `None`, switch-in instead focuses the agent pane.
- **Does not assert:** Dashboard focus restore (keyed by session id, handled in the UI loop, not `TabManager`).
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/003 — Dashboard `selected_index` is derived from `selected_session_id`; the sync is gated to the active tab (PRD #83 M3).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none.
- **Asserts:** `ui::sync_and_derive_selection` resolves a Dashboard `selected_session_id` to its card index, and adopts a focused pane that maps to a visible card; running the same sync against a Mode tab returns `None` and never rewrites the Dashboard's stored id (no cross-tab leak).
- **Does not assert:** the per-frame call site in `run_tui` (exercised by the L1 render test `dashboard/pane/005`).
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/004 — Stale-id fallback clears the field and defaults; reactive-pane recreation remaps focus (PRD #83 M4).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none (mock `PaneController`).
- **Asserts:** a remembered session/role id no longer in the filtered list is cleared and the selection falls back to index 0; `remap_focus_after_reactive_change` follows a `(closed_id, new_id)` pair to the successor pane on BOTH the active tab (returning its new id for re-focus) and a background (non-active) Mode/Orchestration tab, and clears the field on either when a focused pane vanished with no successor.
- **Does not assert:** the controller-level resize that follows a reactive swap.
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/005 — Multi-tab walkthrough: each switch-in restores that tab's own deck/pane (PRD #83 M2/M6).
- **Layer:** L1 (in-process integration test; `src/tab.rs`).
- **Agent:** none (mock `PaneController` records `focus_pane` calls).
- **Asserts:** across a Dashboard, two Mode tabs, and one Orchestration tab, focusing a side pane on each Mode tab and switching between tabs restores each destination tab's own remembered pane (or its default agent / start-role pane) via a `focus_pane` call.
- **Does not assert:** rendering; this drives the `TabManager` capture/restore path directly.
- **Platform coverage:** mac+linux+windows.

### Embedded pane attach

#### embed/attach

##### embed/attach/001 — Starting an agent attaches a live PTY stream to the embedded pane region; its output renders into the parsed grid.
- **Layer:** L2.
- **Agent:** none (fixture stub command writes a fixed banner).
- **Asserts:** the banner string appears in the parsed grid for the agent pane region within a `wait_until_quiescent` window.
- **Does not assert:** byte-level timing of the stream.
- **Platform coverage:** mac+linux.

##### embed/attach/002 — Reattach replays the daemon's per-agent scrollback snapshot.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after detaching and reattaching, a banner that was emitted before the detach is still in the parsed grid.
- **Does not assert:** the full scrollback length (the snapshot is bounded).
- **Platform coverage:** mac+linux.

##### embed/attach/003 — Mouse scroll forwards to the focused embedded pane when the pane reports mouse-mode support.
- **Layer:** L2.
- **Agent:** none (fixture: a pane that enables mouse tracking and echoes wheel events).
- **Asserts:** the parsed grid shows the wheel-event echo after a simulated scroll.
- **Does not assert:** scroll velocity / acceleration.
- **Platform coverage:** mac+linux.

##### embed/attach/004 — Scrollback navigation (Page Up / Down) does not corrupt the live region.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after scrolling back and returning to the bottom, the parsed grid still tracks new bytes.
- **Does not assert:** the exact scroll keymap on every platform.
- **Platform coverage:** mac+linux.

##### embed/attach/005 — `AgentRecord.tab_membership` returned by the daemon's `list_agents` is sanitized on hydration; hostile fields (ANSI escapes, NUL bytes, control chars, oversized cwd/role_name) do not corrupt the rebuilt tab bar.
- **Layer:** L2.
- **Agent:** none (fixture forces a daemon to advertise an `AgentRecord` whose `tab_membership` carries `\x1b[31m`, an embedded NUL, and an over-cap role name; harness exposes a helper to override the daemon's outgoing record).
- **Asserts:** after reattach, the rebuilt tab bar contains no raw ANSI / control bytes in any rendered cell; the offending agent either appears under a sanitized label or is bucketed back to the dashboard (per `validate_tab_membership`'s policy).
- **Does not assert:** the exact sanitization output beyond "no raw control bytes survive into the rendered grid" (the pure-data `validate_tab_membership_*` tests pin the per-field policy).
- **Platform coverage:** mac+linux.

### Hook delivery

#### hooks/delivery

##### hooks/delivery/001 — A Claude Code `SessionStart` hook arriving at the daemon's hook socket creates a session entry on the dashboard.
- **Layer:** L2.
- **Agent:** none (write JSON directly to the per-test hook socket).
- **Asserts:** a card appears for the new `session_id`; status is the post-`SessionStart` resting state per the `state` module.
- **Does not assert:** card position in the grid (covered by `dashboard/pane/001`).
- **Platform coverage:** mac+linux.

##### hooks/delivery/002 — A `PreToolUse` hook updates the right session's card by `pane_id`/`session_id` correlation.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with two synthetic sessions present, only the targeted card transitions to Working.
- **Does not assert:** how `pane_id` is propagated through the env var (a hooks-install concern covered by `hooks/install/*`).
- **Platform coverage:** mac+linux.

##### hooks/delivery/003 — An OpenCode `tool.execute.before` hook updates the right session's card.
- **Layer:** L2.
- **Agent:** none (synthetic OpenCode-format payload).
- **Asserts:** correct OpenCode session transitions to Working with the right tool name.
- **Does not assert:** Claude-vs-OpenCode card visual differentiation.
- **Platform coverage:** mac+linux.

##### hooks/delivery/004 — A malformed hook payload is dropped without disrupting the deck.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** sending invalid JSON to the hook socket leaves all cards and statuses unchanged; the deck does not exit.
- **Does not assert:** error logging content (best-effort logging path).
- **Platform coverage:** mac+linux.

##### hooks/delivery/005 — Hook events survive a TUI detach/reattach cycle (daemon buffers).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** an event sent while the TUI is detached is reflected in the card status on reattach.
- **Does not assert:** how the daemon buffers (snapshot vs queue).
- **Platform coverage:** mac+linux.

##### hooks/delivery/006 — `DOT_AGENT_DECK_PANE_ID` is scrubbed and re-set per-agent so hooks from agent A never carry agent B's `pane_id`.
- **Layer:** L2.
- **Agent:** none (two synthetic agents started under the same daemon; each invokes the bundled `hook` subcommand and the daemon's env-scrub is what isolates them).
- **Asserts:** with two cards alive, a hook emitted from agent A updates only A's card; a subsequent hook from agent B updates only B's card; neither hook's payload arrives carrying the other agent's `pane_id`.
- **Does not assert:** the absolute env-scrub call sites (covered by `agent_pty` pure-data tests `spawn_scrubs_via_daemon_env_from_child`, `spawn_scrubs_pane_id_env_from_child`, `spawn_opts_env_overrides_pane_id_scrub` — moved to `tmp/legacy-tests/`; this catalog entry replaces that lost end-to-end signal).
- **Platform coverage:** mac+linux.

#### hooks/install

##### hooks/install/001 — Launching the deck with `~/.claude/` present writes hook entries into `~/.claude/settings.json` idempotently.
- **Layer:** L2.
- **Agent:** none (fixture redirects `HOME`).
- **Asserts:** after first launch, `settings.json` contains the expected hook list; a second launch leaves it byte-identical.
- **Does not assert:** other unrelated keys in `settings.json` (must be preserved verbatim).
- **Platform coverage:** mac+linux.

##### hooks/install/002 — Launching the deck with `~/.opencode/` present writes the JS plugin to `~/.opencode/plugin/dot-agent-deck/index.js`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** plugin file exists; its content equals the bundled template with `BINARY_PATH` interpolated.
- **Does not assert:** the plugin runs (verified end-to-end by `hooks/delivery/003`).
- **Platform coverage:** mac+linux.

##### hooks/install/003 — Missing agent directories result in a silent skip — no error path.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** launching with neither `~/.claude/` nor `~/.opencode/` does not write any settings file and the TUI starts normally.
- **Does not assert:** the (absence of a) tracing log line.
- **Platform coverage:** mac+linux.

### Pane / agent lifecycle

#### lifecycle/start

##### lifecycle/start/001 — Starting an agent via the new-pane form creates one card and one PTY in the daemon registry.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon's `list_agents` returns one entry whose `pane_id_env` matches what the TUI assigned.
- **Does not assert:** PTY size at spawn (covered by `resize/sigwinch/*`).
- **Platform coverage:** mac+linux.

##### lifecycle/start/002 — An invalid command field shows an inline form error and does not spawn an agent.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the form gains an error message; no new agent appears in `list_agents`.
- **Does not assert:** the error message wording (loose substring match).
- **Platform coverage:** mac+linux.

#### lifecycle/stop

##### lifecycle/stop/001 — `Ctrl+w` on a focused dashboard card stops the agent and removes the card.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** daemon-side `list_agents` shrinks; the card disappears.
- **Does not assert:** filesystem cleanup of the agent's scratch dir.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/002 — `dot-agent-deck daemon stop` with managed agents alive exits non-zero without killing them (data-loss guard).
- **Layer:** L2.
- **Agent:** none (the harness runs the `daemon stop` subcommand).
- **Asserts:** subprocess exits non-zero; the daemon and managed agents are still alive afterwards.
- **Does not assert:** stderr content beyond mentioning `--force`.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/003 — `daemon stop --force` kills the daemon and any managed agents, then exits zero.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon socket disappears within the grace window; managed agents are reaped.
- **Does not assert:** SIGTERM-vs-SIGKILL escalation timing (covered indirectly by the lib's terminate tests now living in `tmp/legacy-tests/`).
- **Platform coverage:** mac+linux.

##### lifecycle/stop/004 — `daemon stop` with no daemon running is idempotent (exit 0).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** subprocess exits 0; no daemon spawned by the call.
- **Does not assert:** stdout content (loose contains-check).
- **Platform coverage:** mac+linux.

#### lifecycle/restart

##### lifecycle/restart/001 — `daemon restart` reuses the next-launch lazy-spawn — a subsequent `dot-agent-deck` launch comes up against a fresh daemon process.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon PID before and after a restart cycle differ; the deck still attaches.
- **Does not assert:** any timing characteristics of the restart.
- **Platform coverage:** mac+linux.

#### lifecycle/daemon-idle

##### lifecycle/daemon-idle/001 — The daemon exits after the idle window elapses with no TUI and no managed agents.
- **Layer:** L2.
- **Agent:** none (tunable idle window via `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS`).
- **Asserts:** the daemon socket disappears within the window plus a small jitter budget.
- **Does not assert:** behavior with the env var set to `0` (covered by `lifecycle/daemon-idle/002`).
- **Platform coverage:** mac+linux.

##### lifecycle/daemon-idle/002 — Setting `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` disables the idle shutdown.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after a window comfortably longer than the default, the daemon still answers.
- **Does not assert:** indefinite lifetime (capped by the test timeout).
- **Platform coverage:** mac+linux.

##### lifecycle/daemon-idle/003 — A registered enabled schedule keeps the daemon alive past the idle window (PRD #127 M1.4 carve-out); removing it lets the daemon idle-exit.
- **Layer:** L2.
- **Agent:** none (a global `schedules.toml` with one enabled task; fast `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS`).
- **Asserts:** with zero clients and zero live agents the daemon survives well past the idle window while an enabled schedule is registered (covers the before-first-fire and after-agent-exit gaps); after the schedule is cleared and reloaded the daemon exits within the window plus margin.
- **Does not assert:** any fire behavior of the schedule, nor reuse-tab semantics.
- **Platform coverage:** mac+linux.

#### lifecycle/orphan-exit

##### lifecycle/orphan-exit/001 — An idle-disabled daemon with `DOT_AGENT_DECK_EXIT_WHEN_ORPHANED=1` self-exits gracefully once its parent dies (orphaned to init), instead of leaking to PID 1.
- **Layer:** L2.
- **Agent:** none (the daemon runs under a short-lived intermediate `sh` parent the test can kill without killing itself).
- **Asserts:** after SIGKILLing the intermediate parent, the daemon process terminates within a few seconds, even though idle shutdown is disabled so only the orphan watchdog can end it.
- **Does not assert:** the max-lifetime backstop (`DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`, covered by the daemon pure-data unit tests) or production daemons (the watchdog is OFF unless the env var is set).
- **Platform coverage:** mac+linux.

#### lifecycle/handshake

##### lifecycle/handshake/001 — Build-version match on attach proceeds silently into the dashboard.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** no mismatch prompt is rendered; the dashboard appears.
- **Does not assert:** any tracing log line.
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/002 — Build-version mismatch in a TTY context renders the interactive prompt; pressing `S` terminates the old daemon and lazy-spawns a fresh one.
- **Layer:** L2.
- **Agent:** none (uses `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` to simulate skew).
- **Asserts:** the rendered prompt contains both build IDs; after pressing `S` the dashboard appears against a daemon at the laptop's build.
- **Does not assert:** exact prompt-text character matching (already pinned in lib pure-data tests).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/003 — Build-version mismatch with live agents requires two consecutive `S` presses.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** one `S` does not terminate; two consecutive `S` presses do.
- **Does not assert:** the warning string wording (loose substring match).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/004 — Build-version mismatch on a non-TTY exits non-zero with a stderr recovery hint and no prompt.
- **Layer:** L2.
- **Agent:** none (run with stdout redirected to a pipe).
- **Asserts:** exit code is non-zero; stderr mentions `dot-agent-deck daemon stop`.
- **Does not assert:** exact stderr wording (pinned in lib pure-data tests).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/005 — Build-version mismatch prompt: pressing `Q` / `Ctrl+C` / `Ctrl+D` / `Esc` aborts startup with a non-zero exit and leaves the stale daemon running.
- **Layer:** L2.
- **Agent:** none (uses `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` to simulate skew).
- **Asserts:** for each abort keystroke (`Q`, `q`, `Ctrl+C`, `Ctrl+D`, `Esc`): the TUI exits non-zero; the daemon socket is still answering after the exit; no fresh daemon was spawned.
- **Does not assert:** any rendered error message after abort (the prompt itself is the user-visible artifact).
- **Platform coverage:** mac+linux.

### Resize

#### resize/sigwinch

##### resize/sigwinch/001 — Resizing the outer terminal mid-run propagates a SIGWINCH and the dashboard re-renders to the new dimensions.
- **Layer:** L2.
- **Agent:** none (Decision 20 requires at least one catalog test here).
- **Asserts:** after `deck.resize(80, 24)`, the rendered grid is 80 columns wide; cards reflow accordingly.
- **Does not assert:** font-related metrics.
- **Platform coverage:** mac+linux.

##### resize/sigwinch/002 — Resize of the outer terminal also resizes every managed agent PTY.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon reports each agent's PTY at the new size; agent processes that print `tput cols` see the new column count.
- **Does not assert:** any visual reflow inside the agent (subprocess-dependent).
- **Platform coverage:** mac+linux.

##### resize/sigwinch/003 — Resize coalescing — a rapid sequence of resize events results in one final reflow, not N.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** observed reflow count under a burst of resize events is bounded; final size matches the last input.
- **Does not assert:** the exact debounce window (a harness constant).
- **Platform coverage:** mac+linux.

#### resize/layout

##### resize/layout/001 — `Ctrl+t` toggles stacked / tiled dashboard layout without dropping any agents.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after toggling, all cards are still present; the layout differs across snapshots.
- **Does not assert:** which layout is the "default" (already a settled product call).
- **Platform coverage:** mac+linux.

#### resize/render

##### resize/render/001 — Enlarging the outer terminal fills the new width across an embedded pane — no empty band on the right edge.
- **Layer:** L2.
- **Agent:** none (a long-lived `sleep` pane gives a focusable embedded PTY without LLM credentials).
- **Asserts:** with an embedded pane present, after `deck.resize(W+10, H)` and the deck quiescent, the rendered frame spans the full new width and the pane's bordered region reaches the new right edge — no unfilled column band between the deck's chrome and the new edge.
- **Does not assert:** the pane *program's* own reflow (a non-redrawing `sleep` pane never repaints newly exposed columns — expected terminal behaviour, not the deck bug); exact per-cell colours; the transient single-frame band itself.
- **Platform coverage:** mac+linux.
- **M1 status (PRD #84):** RED side of the M4 chain (`Event::Resize` → recompute layout → resize PTYs → render). The empty-band symptom is a one-frame race the current code self-heals once the resize handler fires, so this is written as an **invariant guard**: it pins "the post-resize frame fills the new width" and currently passes after quiescence. It *flags* (does not hard-fail) because the transient band is not deterministically observable through the PTY+vt100 harness. The widget-level half of the same defect (the `min(area, screen)` col clamp) is covered deterministically by `render/widget/001` and `render/widget/002`. Goes/stays GREEN at M4.
- **Post-M5 resolution (PRD #84):** **GREEN.** After M4 (layout-driven PTY resize) and M5 (1:1 widget render with the contract `debug_assert!` live in debug builds), the enlarge path drives recompute-layout → resize-PTYs-to-match → render, and the settled frame fills the new width. The guard now exercises that contract chain with the col clamp gone, rather than masking a self-healing race. Confirmed green post-M5.

### Render contract (PRD #84)

The rendering-contract reproducers for the PRD #84 (`prds/84-rendering-layer-rework.md`)
rework: one reproducer per known render-path defect, each the RED side of a TDD chain that
goes GREEN at M4 (layout-driven PTY resize) or M5 (1:1 `TerminalWidget`). They target the
`src/terminal_widget.rs` `min(area, screen)` col clamp + cursor-anchored row window (removed
in M5) and the scattered, per-path layout/resize math (unified in M3/M4). `render/widget/*`
are deterministic L1/unit tests over `TerminalWidget` rendered against a `ratatui` buffer;
`render/layout/*` drive the real spawned-binary layout-change pipelines and are invariant
guards where the underlying glitch is transient/race-y (per the PRD's "race-y resize timing"
note).

#### render/widget

##### render/widget/001 — `TerminalWidget` renders the PTY screen 1:1 from row 0 — no cursor-anchored row window that drops or shifts the top rows.
- **Layer:** L1 (in-process `TerminalWidget` rendered into a `ratatui::buffer::Buffer`; no PTY, no subprocess).
- **Agent:** none.
- **Asserts:** given a vt100 screen taller than the widget's inner area with the cursor parked on the bottom row, the widget maps screen cell (r, c) → inner cell (r, c) so the inner top row shows screen row 0 — i.e. the top-of-screen marker is rendered at the top of the pane.
- **Does not assert:** behaviour when the screen fits the area exactly (already correct today); colours / cursor-highlight styling; scrollback.
- **Platform coverage:** mac+linux.
- **M1 status (PRD #84):** **RED.** Current `src/terminal_widget.rs:96-117` anchors a row window on the cursor (`start_row = effective_rows - rows`), so with the cursor low it shows the *bottom* rows and the row-0 marker is absent → assertion fails today. Core gate for M5 (the 1:1 widget maps screen row 0 → area row 0). Deterministic at the widget level — the fixture intentionally violates the (future) upstream size contract to exercise the windowing heuristic M5 removes.
- **Post-M5 resolution (PRD #84):** **GREEN.** M5 removed the cursor-anchored row window (and the `min(area, screen)` col clamp) from `src/terminal_widget.rs`, so the widget now maps screen cell (r, c) → inner cell (r, c) and renders 1:1 from row 0: the inner top row shows screen row 0 (`TOP_ROW_0`) and the assertion passes. Confirmed RED→GREEN post-M5 — the core M5 gate is met.

##### render/widget/002 — `TerminalWidget` tolerates an inner area larger than the PTY screen — falls back to drawing the available cells at the top-left, no panic, no out-of-bounds read.
- **Layer:** unit (in-process `TerminalWidget` rendered into a `ratatui::buffer::Buffer`).
- **Agent:** none.
- **Asserts:** rendering a small (e.g. 3×6) PTY screen into a larger (e.g. 6×12) inner area completes without panicking; the PTY content lands at the top-left and the excess rows/columns stay blank (the `min(area, pty)` fallback).
- **Does not assert:** the debug-build `debug_assert!(pty == inner)` invariant M5 adds (a dev guard, not a runtime assertion — see PRD #84 M5); the single release-mode log line on mismatch.
- **Platform coverage:** mac+linux.
- **M1 status (PRD #84):** **Flag / guard (passes today).** Pins the release-path contract M5 must preserve: area > PTY must fall back to `min` and never panic. Current code already does `min` and does not panic, so this is GREEN now and stays GREEN through M5's release fallback. (M5's debug-only `debug_assert!` is explicitly out of scope here — orchestrator brief: "test the release fallback path".)
- **Post-M5 resolution (PRD #84):** **GREEN (unchanged throughout M1→M5).** M5 preserved the release `min(area, pty)` no-panic fallback (log-once on mismatch) alongside the new debug-build contract `debug_assert!`, so this release-path guard stays green and now pins the fallback the M5 contract intentionally keeps.

#### render/layout

##### render/layout/001 — After a tab/layout switch with N panes the embedded pane's bottom rows show correct (non-stale) content — no off-by-one row shift.
- **Layer:** L2.
- **Agent:** none (long-lived `sleep` panes).
- **Asserts:** with ≥1 embedded pane carrying a known bottom-row marker, after a layout change (`Ctrl+t` toggle) and quiescence, the pane's bottom row still shows its marker — not a stale fragment of the pre-switch layout, and not shifted by a row.
- **Does not assert:** which layout is default; the pane program's own redraw; that the defect reproduces every run.
- **Platform coverage:** mac+linux.
- **M1 status (PRD #84):** **Flag / invariant-check (riskiest entry).** The PRD risk row flags this symptom as possibly a vt100/parser issue below scope. The current code resizes panes on every layout-change path (`Action::ToggleLayout` routes through `resize_*_panes`), so the area/PTY mismatch that would scramble the bottom rows self-heals and is not deterministically observable through the harness. Written as an invariant guard on bottom-row content (PTY size == inner area, observed via rendered content). If it reproduces deterministically after M4+M5, that's follow-up signal — NOT a reason to re-add the clamp.
- **Post-M5 resolution (PRD #84):** **GREEN.** Stays green after M4+M5 and now runs with the M5 contract `debug_assert!` live in debug builds: a layout toggle that left a pane's PTY out of step with its rect would trip the debug assert instead of self-healing, so the guard exercises the layout-driven resize + 1:1 render contract rather than masking the race. No deterministic bottom-row scramble survived M4+M5 — no below-scope (vt100/parser) follow-up signal, and the clamp stays removed.

##### render/layout/002 — Reactive pane recreation/replace leaves no scrambled fragments — the replacement pane renders cleanly.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after a pane is recreated/replaced in place (open a second pane, close the first), the rendered grid contains the surviving pane's content and no leftover fragment of the removed pane at a stale position.
- **Does not assert:** the exact recreation trigger internals; per-cell colours.
- **Platform coverage:** mac+linux.
- **M1 status (PRD #84):** **Flag / invariant-check.** Pane open/close and reactive recreation (`src/ui.rs:1510`, `:2147` areas) currently resize the affected PTYs on the spot, so any scramble is transient. Invariant guard on "no stale fragment after replace". GREEN target at M4/M5.
- **Post-M5 resolution (PRD #84):** **GREEN.** Stays green after M4+M5 and now exercises the pane open/close replace through layout-driven resize + 1:1 widget render with the M5 contract `debug_assert!` live in debug builds — asserting the replace contract rather than masking a self-healing race.

##### render/layout/003 — A mode switch (the `render_mode_tab` path) leaves no short-lived render artefacts after the transition settles.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after switching into a mode tab and quiescence, the rendered grid shows the destination layout cleanly with no leftover fragment from the dashboard/source layout.
- **Does not assert:** mode-tab content semantics; the transient mid-transition frame.
- **Platform coverage:** mac+linux.
- **M1 status (PRD #84):** **Flag / invariant-check.** Mode switch (`src/ui.rs:2828` area) resizes panes through `resize_mode_tab_panes`, so artefacts are transient. Invariant guard on post-transition cleanliness. GREEN target at M4/M5.
- **Post-M5 resolution (PRD #84):** **GREEN.** Stays green after M4+M5 and now exercises the `render_mode_tab` switch through layout-driven resize + 1:1 widget render with the M5 contract `debug_assert!` live in debug builds — asserting the mode-switch contract rather than masking a self-healing race.

### Keybindings (PRD #40)

Keybindings resolve **client-side**: the config file lives on the machine
running the TUI (`$HOME/.config/dot-agent-deck/keybindings.toml`, mirroring
the `config.toml` path), the TUI event loop reads it and matches each
keypress to a semantic action, and the daemon never sees raw command-mode
keystrokes — it stays binding-agnostic. The L2 tests below are
interface-agnostic: each stages a `keybindings.toml` under the per-test
HOME (harness `TuiDeckBuilder::with_keybindings_toml`) and asserts on the
rendered grid, so they exercise the full client-side resolution path
without depending on the config struct API.

#### keybindings/remap

##### keybindings/remap/001 — A config remap of a **global** action (`toggle_layout` → `Alt+Shift+l`) takes effect on the new combo and the old default stops toggling.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with a `keybindings.toml` rebinding `[global] toggle_layout = "Alt+Shift+l"`, pressing `Alt+Shift+l` toggles the dashboard layout (the `Layout: …` status message appears in the bottom bar); the old default toggle key (`Ctrl+t`) no longer toggles. The remap is resolved **client-side** — the file is read on the TUI side, the TUI matches the keypress to the action, and the daemon stays binding-agnostic.
- **Does not assert:** which layout (stacked vs tiled) is the default, exact status-message wording beyond the `Layout:` prefix, daemon-side behaviour (there is none — binding resolution is entirely client-side).
- **Platform coverage:** mac+linux.

##### keybindings/remap/002 — A config remap of a **dashboard** action (`help` `?` → `F1`) opens the help overlay on the new key.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with a `keybindings.toml` rebinding `[dashboard] help = "F1"`, pressing `F1` opens the help overlay (the "Create new pane" line is rendered).
- **Does not assert:** that the old `?` still opens help (the action was remapped, not added), help-overlay content beyond one anchor line.
- **Platform coverage:** mac+linux.

#### keybindings/safety

##### keybindings/safety/001 — `Ctrl+C` always opens the quit modal, even when another action is bound to `Ctrl+C`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with a `keybindings.toml` that tries to hijack `Ctrl+C` for another action (`[global] new_pane = "Ctrl+C"`), pressing `Ctrl+C` still opens the quit/detach modal ("Quit dot-agent-deck?"). `Ctrl+C` is a non-overridable safety net — quit is not a configurable action (it is hardcoded in the event loop), so no action bound to `Ctrl+C` can hijack it. Exercises the GLOBAL-block `Ctrl+C` exclusion path. Guard test — must stay green so config can never disable emergency quit.
- **Does not assert:** which quit option is selected by default, the dialog layout.
- **Platform coverage:** mac+linux.

##### keybindings/safety/002 — `Ctrl+C` always opens the quit modal, even when a tab-navigation action is bound to `Ctrl+C`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with a `keybindings.toml` that binds both `[dashboard] move_left = "Ctrl+C"` and `move_right = "Ctrl+C"`, pressing `Ctrl+C` still opens the quit/detach modal ("Quit dot-agent-deck?"). Complements safety/001 by covering the Normal-mode tab-cycle dispatch path: `Ctrl+C` is never routed through the configurable `move_left`/`move_right` matching, so it can't be turned into a tab switch. `Ctrl+C` is non-overridable. Regression guard for the `!is_ctrl_c` gate on that dispatch path.
- **Does not assert:** tab-switch behaviour for non-`Ctrl+C` `move_left`/`move_right` bindings, conflict-resolution warning wording.
- **Platform coverage:** mac+linux.

#### keybindings/unbind

##### keybindings/unbind/001 — An empty-string binding (`new_pane = ""`) makes the default key a no-op.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with a `keybindings.toml` setting `[global] new_pane = ""`, pressing the default `Ctrl+n` does nothing — the directory picker / new-pane flow ("Select Directory") never opens. The deck stays in Normal mode (a following `?` still opens help).
- **Does not assert:** behaviour of other unbound actions, that the new-pane flow can be re-bound to a different key (separate concern).
- **Platform coverage:** mac+linux.

#### keybindings/fallback

##### keybindings/fallback/001 — A malformed `keybindings.toml` falls back to defaults and warns on stderr.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with an unparseable `keybindings.toml`, the deck still launches to its empty dashboard, default bindings still work (`?` opens help), and a warning mentioning "keybindings" is emitted on stderr (observed in the merged PTY byte stream, which retains it after the TUI clears the screen).
- **Does not assert:** the exact warning wording beyond the "keybindings" substring, per-entry vs whole-file fallback granularity.
- **Platform coverage:** mac+linux.

#### keybindings/help

##### keybindings/help/001 — The help overlay is generated from the active keybinding config and shows remapped keys.
- **Layer:** L1 (ratatui `TestBackend` + `insta` file snapshot).
- **Agent:** none.
- **Asserts:** rendered against a `KeybindingConfig` that remaps `toggle_layout` → `Alt+Shift+l` and `help` → `F1`, the help-overlay buffer shows those custom notations (the snapshot — and a substring guard on `Alt+Shift+l` / `F1`) rather than the defaults, proving the overlay is generated from the active config, not hardcoded strings. The defaults-content guard lives separately at `dashboard/help/002` and stays untouched.
- **Does not assert:** the overlay's exact column layout or footer wording beyond what the committed snapshot pins; behaviour with the *default* config (that is `dashboard/help/002`'s job).
- **Platform coverage:** mac+linux+windows.

#### keybindings/hints

##### keybindings/hints/001 — The hints bar is generated from the active keybinding config and shows remapped keys.
- **Layer:** L1 (ratatui `TestBackend` + `insta` file snapshot).
- **Agent:** none.
- **Asserts:** rendered against a `KeybindingConfig` that remaps `toggle_layout` → `Alt+Shift+l`, the hints-bar buffer shows the custom layout-toggle notation (the snapshot — and a substring guard on `Alt+Shift+l`) rather than the default `Ctrl+t`, proving the hints bar is generated from the active config.
- **Does not assert:** the full set of actions shown in the bar or their order beyond what the committed snapshot pins; truncation behaviour at narrow widths.
- **Platform coverage:** mac+linux+windows.

##### keybindings/hints/002 — An unbound action is rendered as `(unbound)` in the hints bar, never as a bare `: <label>`.
- **Layer:** L1 (ratatui `TestBackend`; asserts on buffer text, no `insta` snapshot).
- **Agent:** none.
- **Asserts:** rendered against a default `KeybindingConfig` with `new_pane` unbound (empty notation), the hints-bar text substitutes `(unbound)` for the empty key (matching the help overlay) and renders `(unbound): new`; it never emits a bare `: new` with an empty key column (no leading `: <label>` and no mid-string `  : <label>`). Greptile P2 regression guard.
- **Does not assert:** the exact placeholder wording beyond `(unbound)`, behaviour of other simultaneously-unbound actions, snapshot of the full bar.
- **Platform coverage:** mac+linux+windows.

#### keybindings/buttons

##### keybindings/buttons/001 — The prd-80 button bar labels are derived from the active keybinding config.
- **Layer:** L1 (ratatui `TestBackend`; asserts on buffer text, no `insta` snapshot).
- **Agent:** none.
- **Asserts:** rendered against a `KeybindingConfig` that remaps `new_pane` → `Alt+P` and `help` → `F1`, the button bar shows the remapped New-pane key `Alt+P` and Help key `F1`, and does NOT show the default New-pane key `Ctrl+N` — proving the button labels are generated from the active config, not hardcoded. Guards against a future refactor silently re-hardcoding the labels.
- **Does not assert:** button positions/ordering, the non-remappable `Quit` button label (fixed `Ctrl+C`), truncation behaviour at narrow widths.
- **Platform coverage:** mac+linux+windows.

#### keybindings/scheduler

##### keybindings/scheduler/001 — The "Scheduled Tasks" dialog open-shortcut is registry-routed: the default lowercase `s` opens it, not uppercase-only `Shift+S` (PRD #127 finding #4).
- **Layer:** L2.
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`).
- **Asserts:** with no `keybindings.toml`, pressing the DEFAULT lowercase `s` from the empty dashboard opens the "Scheduled Tasks" manager dialog (confirmed by the seeded task name appearing in the dialog list) — proving the open-shortcut is routed through the KbAction registry with a case-insensitive default (lowercase `s` as well as `S`, like the registry's `t`/`T` and `l`/`L` pairs) rather than the hardcoded uppercase-only `KeyCode::Char('S')`.
- **Does not assert:** that `S` still works (covered by `scheduler/manager/*`); remappability of the open-shortcut to an arbitrary key; the dialog's list/action contents beyond the seeded task name.
- **Platform coverage:** mac+linux.

### Error paths

#### error/socket

##### error/socket/001 — The deck refuses to attach to a Unix socket owned by another uid.
- **Layer:** L2.
- **Agent:** none (fixture builds a socket whose mode/owner mimic a foreign daemon).
- **Asserts:** the deck exits non-zero with a stderr message; the foreign socket is left intact.
- **Does not assert:** the message wording beyond mentioning the trust failure.
- **Platform coverage:** mac+linux.

##### error/socket/002 — Stale socket file (inode without a listener) is recovered transparently — the next launch unlinks it and lazy-spawns a fresh daemon.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the dashboard appears on second launch; the socket is now a live daemon's.
- **Does not assert:** the time spent in the recovery path.
- **Platform coverage:** mac+linux.

#### error/config

##### error/config/001 — `.dot-agent-deck.toml` with an invalid regex makes the new-pane form refuse the mode and surface a status-line message.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the mode is missing from the **Mode** cycle; a status-line message names the invalid pattern.
- **Does not assert:** message wording exact match.
- **Platform coverage:** mac+linux.

##### error/config/002 — Missing `.dot-agent-deck.toml` results in the **Mode** field showing only the default; the new-pane form still launches a plain pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the form opens with the default mode selectable; submitting creates a dashboard pane (not a mode tab).
- **Does not assert:** the absence-of-config tip rendering (covered by `dashboard/config-gen/001`).
- **Platform coverage:** mac+linux.

#### error/agent-spawn

##### error/agent-spawn/001 — Submitting the new-pane form with a non-existent command produces a card whose status is Error and whose card body names the missing binary.
- **Layer:** L2.
- **Agent:** none (fixture command: `nonexistent-binary-78f3c`).
- **Asserts:** card appears; badge reads Error; card text contains the binary name.
- **Does not assert:** how long the failure takes to surface.
- **Platform coverage:** mac+linux.

### Orchestration delegation

#### orchestration/delegate

##### orchestration/delegate/001 — `dot-agent-deck delegate --to coder --task <text>` from the orchestrator pane writes the task into the target role's pane.
- **Layer:** L2.
- **Agent:** none (synthetic — invoke the delegate subcommand from inside the orchestrator pane via a scripted keystroke).
- **Asserts:** the target role's parsed grid contains the task text; the orchestrator's pane stays clean.
- **Does not assert:** the target agent's response (no real agent in the loop).
- **Platform coverage:** mac+linux.

##### orchestration/delegate/002 — Delegating to a role missing from the config produces a clear error on the orchestrator pane and no other side effects.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the orchestrator pane's parsed grid carries an error mentioning the unknown role; no card statuses change.
- **Does not assert:** the error message text exactly.
- **Platform coverage:** mac+linux.

##### orchestration/delegate/003 — `dot-agent-deck work-done --task <summary>` from a worker pane writes the summary to the orchestrator and to `.dot-agent-deck/work-done-<role>.md`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** orchestrator pane shows the summary; the file exists with the expected contents.
- **Does not assert:** the orchestrator's reply (no real LLM in this synthetic test).
- **Platform coverage:** mac+linux.

##### orchestration/delegate/004 — A worker calling `delegate` is rejected (only the `start = true` role may delegate).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** worker's pane gains an error line; no task is delivered to any role.
- **Does not assert:** the daemon-side log entry.
- **Platform coverage:** mac+linux.

#### orchestration/layout

##### orchestration/layout/001 — Seven decks fit the single-column orchestration card area without scrolling (PRD #147).
- **Layer:** L1 (ratatui `TestBackend`, buffer inspection + capacity math via the public `rendered_height` seam).
- **Agent:** none.
- **Asserts:** in the ~34%-width single-column orchestration card area at a typical ~48-row card height, the renderer's `visible_rows = available / card_height` fits all 7 decks with no scrolling and the 7th deck actually renders in the visible slice; a much larger deck count (20) still engages scrolling, so right-sizing the card height does not remove the scroll fallback.
- **Does not assert:** the full orchestration-tab frame (tab bar, side panes, stats bar); the `ORCHESTRATION_LEFT_PERCENT` width split or `grid_columns` thresholds (out of scope per PRD #147).
- **Platform coverage:** mac+linux+windows.

### Session restore

#### session/restore

##### session/restore/001 — `dot-agent-deck --continue` rehydrates dashboard panes from the saved session.
- **Layer:** L2.
- **Agent:** none (a saved `session.toml` with three panes; fixture redirects `DOT_AGENT_DECK_SESSION`).
- **Asserts:** three cards appear; their display names match the saved session.
- **Does not assert:** the agents' inner state (not preserved per docs).
- **Platform coverage:** mac+linux.

##### session/restore/002 — A saved mode tab is restored as a full mode tab when the project's `.dot-agent-deck.toml` still has the mode.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after `--continue`, a tab with the mode's name appears and contains the persistent side panes.
- **Does not assert:** any reactive pane content.
- **Platform coverage:** mac+linux.

##### session/restore/003 — A saved mode whose `.dot-agent-deck.toml` no longer carries the mode falls back to a plain dashboard pane with a stderr warning.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the saved pane becomes a dashboard card (not a mode tab); the harness's stderr capture contains a warning that names the missing mode.
- **Does not assert:** any rendering of the warning inside the TUI.
- **Platform coverage:** mac+linux.

##### session/restore/004 — A saved pane whose `dir` no longer exists is skipped with a stderr warning; other saved panes still restore.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** N-1 cards restore; stderr names the missing directory.
- **Does not assert:** which other panes survive (deterministic from the file order).
- **Platform coverage:** mac+linux.

### Chain-smoke (real-agent) coverage

#### chain-smoke/claude

##### chain-smoke/claude/001 — A real Claude Code agent run end-to-end emits hook events that drive the card through Thinking → Working → Idle.
- **Layer:** L2.
- **Agent:** Claude Code (`claude-haiku-4-5-20251001` per Decision 8).
- **Asserts:** card status traverses Thinking → Working → Idle within the test budget; tool name appears on the card during Working.
- **Does not assert:** any specific text the agent prints.
- **Platform coverage:** mac+linux (chain-smoke is local-only per Decision 8).
- **Cost note:** one Haiku invocation, ≲500 input + 200 output tokens — well under Decision 23's bound.

#### chain-smoke/opencode

##### chain-smoke/opencode/001 — A real OpenCode agent run end-to-end emits the OpenCode plugin's events and drives the card through Thinking → Working → Idle.
- **Layer:** L2.
- **Agent:** OpenCode (`openrouter/google/gemini-2.5-flash-lite` per Decision 8).
- **Asserts:** card status traverses Thinking → Working → Idle; OpenCode-format tool name appears on the card.
- **Does not assert:** any agent-generated text.
- **Platform coverage:** mac+linux.
- **Cost note:** one Gemini-Flash-Lite invocation via OpenRouter, ≲500 input + 200 output tokens.

### Mouse Parity (PRD #80)

These entries cover PRD #80 (mouse parity for keyboard actions): every keyboard-only TUI action gains a clickable affordance carrying its shortcut inline, funneled through the single `dispatch_action` action layer.

#### mouse/dispatch

##### mouse/dispatch/001 — Ctrl+N (key) and a click on a New-Pane button rect map to the same `Action::NewPane`.
- **Layer:** pure-data (plain logic, no TUI harness).
- **Agent:** none.
- **Asserts:** `global_ctrl_action(Ctrl+N)` and `hit_test_button` on a synthetic New-Pane button rect both yield `Action::NewPane`; a click that misses every rect yields `None`.
- **Does not assert:** rendering or end-to-end dispatch side effects.
- **Platform coverage:** mac+linux+windows.

#### mouse/button

##### mouse/button/001 — The Button widget renders its inline-shortcut label and dims a disabled button.
- **Layer:** L1 (ratatui `TestBackend`).
- **Agent:** none.
- **Asserts:** an enabled button renders `[Label Shortcut]` un-dimmed and returns its `(Action, Rect)` pair; a disabled button renders the label with the DIM modifier.
- **Does not assert:** click dispatch (covered by `mouse/dispatch/001`).
- **Platform coverage:** mac+linux+windows.

#### mouse/buttonbar

##### mouse/buttonbar/001 — At a comfortable width the global bar renders a button per command with its inline shortcut.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** the bottom row shows `[New Pane Ctrl+N]`, `[Close Ctrl+W]`, `[Toggle Layout Ctrl+T]`, `[Help ?]`, and `[Quit Ctrl+C]`.
- **Does not assert:** click behavior (covered by `mouse/buttonbar/003`).
- **Platform coverage:** mac+linux+windows.

##### mouse/buttonbar/002 — On a narrow terminal the global bar degrades to shortcut-only labels.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** the bar shows `[Ctrl+N] [Ctrl+W] [Ctrl+T] [?] [Ctrl+C]` and the full `[New Pane Ctrl+N]` label is absent — graceful degradation, not mid-label truncation.
- **Does not assert:** exact column widths.
- **Platform coverage:** mac+linux+windows.

##### mouse/buttonbar/003 — Clicking the New Pane bar button opens the directory picker, like Ctrl+N.
- **Layer:** L2 (PTY end-to-end).
- **Agent:** none (synthetic — empty dashboard).
- **Asserts:** clicking `[New Pane Ctrl+N]` opens the `Select Directory` picker.
- **Does not assert:** the rest of the new-pane flow (covered by `mouse/form/001`).
- **Platform coverage:** mac+linux.

##### mouse/buttonbar/004 — A Scheduled Tasks bar button is present and clicking it opens the manager dialog (PRD #127 finding #4 — mouse parity).
- **Layer:** L2 (PTY end-to-end).
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`).
- **Asserts:** the bottom button bar renders a Scheduled Tasks button (label starting `[Scheduled …`); clicking it opens the "Scheduled Tasks" manager dialog (confirmed by the seeded task name appearing in the dialog list), the same outcome as the keyboard open-shortcut — proving click→action parity for the open-shortcut, like `[New Pane Ctrl+N]`.
- **Does not assert:** the in-dialog action clicks (covered by `mouse/modal/001`); the exact button label/shortcut beyond the `[Scheduled` prefix; the bar's narrow-width degradation for the new button.
- **Platform coverage:** mac+linux.

##### mouse/buttonbar/005 — The Scheduled Tasks open button is shown on the dashboard even with ZERO schedules configured (fix/scheduler-single-agent-card — the manager is how you create the first one).
- **Layer:** L1.
- **Agent:** none (renders `dashboard_context_buttons` with `has_schedules = false`).
- **Asserts:** at a comfortable 200-column width (so the full global+context bar fits and overflow is not in play), the bottom button bar renders a Scheduled Tasks open button (label starting `[Scheduled`) even though no schedules exist — because that button opens the manager, which is itself the way to CREATE the first schedule.
- **Does not assert:** the exact label/shortcut beyond the `[Scheduled` prefix; click behavior (covered by `mouse/buttonbar/004`); the bar's narrow-width degradation.
- **Platform coverage:** mac+linux+windows.

##### mouse/buttonbar/006 — At the default 120-col PTY width the FULL dashboard button set degrades to shortcut-only chips (PRD #127 — locks in the responsive collapse the L2 mouse specs widen past).
- **Layer:** L1.
- **Agent:** none (renders the full global + dashboard context bar, including the always-shown Scheduled Tasks button).
- **Asserts:** at 120 cols (`DEFAULT_COLS`) the full set (~133 cells) overflows, so the bar shows the shortcut-only `[Ctrl+N]` chip and NOT the full `[New Pane Ctrl+N]` label, while the Scheduled Tasks button stays present and identifiable as `[Scheduled Tasks s]`.
- **Does not assert:** the exact column widths; click behavior; the full-label rendering at roomy widths (covered by `mouse/buttonbar/001` / `005`).
- **Platform coverage:** mac+linux+windows.

#### mouse/tabstrip

##### mouse/tabstrip/001 — Clicking a tab header switches to that tab.
- **Layer:** L2.
- **Agent:** none (synthetic Mode tab).
- **Asserts:** with Dashboard + a Mode tab open, clicking the inactive `Dashboard` header switches to it (the empty-dashboard state returns).
- **Does not assert:** the `[×]` close affordance (covered by `mouse/tabstrip/002`).
- **Platform coverage:** mac+linux.

##### mouse/tabstrip/002 — Mode/Orchestration tabs carry a clickable `[×]` close affordance (Dashboard has none); clicking it closes the tab.
- **Layer:** L1 (glyph presence/absence) + L2 (click-to-close).
- **Agent:** none.
- **Asserts:** the strip renders exactly one `×` per closeable tab and none for the Dashboard; clicking a Mode tab's `[×]` closes it (Ctrl+W teardown semantics).
- **Does not assert:** which tab gets focus after close.
- **Platform coverage:** mac+linux (L1 half: +windows).

#### mouse/dashboard

##### mouse/dashboard/001 — Single-click selects a card; double-click focuses its pane.
- **Layer:** L2.
- **Agent:** none (synthetic hook card + a real `--continue` pane).
- **Asserts:** single-click moves the `▸` selection marker to the clicked card; double-click focuses its pane and enters PaneInput.
- **Does not assert:** selection wrap behavior (keyboard-covered).
- **Platform coverage:** mac+linux.

##### mouse/dashboard/002 — The dashboard exposes clickable Filter / Rename / Generate buttons.
- **Layer:** L2.
- **Agent:** none (synthetic card with cwd).
- **Asserts:** clicking `[Filter /]` enters filter mode (typed text echoes), `[Rename r]` enters rename, `[Generate g]` opens the config-gen prompt.
- **Does not assert:** the downstream filter/rename/generate outcomes (keyboard-covered).
- **Platform coverage:** mac+linux.

#### mouse/modal

##### mouse/modal/001 — Modal dialog buttons fire their action like the keyboard.
- **Layer:** L2.
- **Agent:** none (synthetic card for config-gen; fixture `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES` for the Scheduled Tasks manager).
- **Asserts:** quit-confirm `[Cancel]` dismisses (app stays), config-gen `[Never]` sets the "Config prompt suppressed" status, help `[Close]` closes the overlay, and the "Scheduled Tasks" manager dialog's `[Delete]` button surfaces the definition-only delete-confirmation (`Delete schedule '<name>'?`) like pressing `d` (PRD #127 finding #4 — modal mouse parity).
- **Does not assert:** the destructive quit-confirm `[Detach]`/`[Stop]` (process-exit, keyboard-tested) or the star-prompt (not deterministically triggerable); the manager dialog's other clickable actions — `[Add]`/`[Edit]`/`[Run now]` — which the coder must also wire (and whose click outcomes are deferred).
- **Platform coverage:** mac+linux.

##### mouse/modal/002 — Each modal renders explicit buttons alongside its existing selection list / hint.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** quit-confirm `[Detach] [Stop] [Cancel]`, config-gen `[Yes] [No] [Never]`, star `[Star] [Snooze] [Dismiss]`, and help `[Close]` render while the existing list / hint text is still present (additive).
- **Does not assert:** click outcomes (covered by `mouse/modal/001`).
- **Platform coverage:** mac+linux+windows.

#### mouse/inline

##### mouse/inline/001 — Inline filter/rename rows gain Apply/Save/Cancel buttons; PaneInput gains `[Detach Ctrl+D]`.
- **Layer:** L1 (button render) + L2 (click outcomes).
- **Agent:** none (synthetic card + a real `--continue` pane for detach).
- **Asserts:** the filter row renders `[Apply]`/`[Cancel]` and the rename row `[Save]`/`[Cancel]` alongside the input; clicking them commits/abandons like Enter/Esc; clicking inside the field keeps it focused (typing stays keyboard); `[Detach Ctrl+D]` returns from PaneInput to the dashboard.
- **Does not assert:** cursor pixel position within the field.
- **Platform coverage:** mac+linux (L1 half: +windows).

#### mouse/picker

##### mouse/picker/001 — The directory picker is mouse-operable (rows, parent, Confirm/Cancel/Filter).
- **Layer:** L1 (affordance render) + L2 (click outcomes).
- **Agent:** none.
- **Asserts:** the picker renders `[Confirm]`/`[Cancel]`/`[Filter]`; single-click selects a row, double-click descends, clicking `..` goes up, `[Cancel]` closes to the dashboard, `[Confirm]` opens the new-pane form, `[Filter]` opens the filter input.
- **Does not assert:** filter-narrowing correctness (keyboard-covered).
- **Platform coverage:** mac+linux (L1 half: +windows).

#### mouse/form

##### mouse/form/001 — The new-pane form is mouse-operable (field focus, mode chips, Submit/Cancel).
- **Layer:** L1 (chip + button render) + L2 (click outcomes).
- **Agent:** none (fixture with two modes).
- **Asserts:** the form renders one clickable chip per mode option plus `[Submit]`/`[Cancel]`; clicking a field focuses it (typing lands there), clicking a chip selects that mode (title reflects it), `[Submit]` creates the pane, `[Cancel]` discards.
- **Does not assert:** command-field validation.
- **Platform coverage:** mac+linux (L1 half: +windows).

#### mouse/preserve

##### mouse/preserve/001 — Existing pane mouse behavior survives the button layer.
- **Layer:** L2.
- **Agent:** none (real `--continue` pane).
- **Asserts:** double-click still focuses a card's pane (PaneInput); a non-button click in the pane region is not swallowed into a button action; a scroll in the pane region reaches the scroll path, not the button hit-test.
- **Does not assert:** mode-tab click-to-focus, text-selection drag, Ctrl+click hyperlink, child-app forwarding (deferred in the test body with reasons).
- **Platform coverage:** mac+linux.

##### mouse/preserve/002 — Button clicks short-circuit; misses fall through.
- **Layer:** L2.
- **Agent:** none (synthetic cards).
- **Asserts:** clicking a card (missing every button) falls through to card selection; clicking the `[New Pane Ctrl+N]` bar button fires its action and does NOT also act on the cards underneath.
- **Does not assert:** per-region hit-test internals.
- **Platform coverage:** mac+linux.

#### mouse/help

##### mouse/help/001 — The `?` help overlay documents the canonical post-button-bar shortcut set.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** the overlay documents the global commands the button bar advertises (Ctrl+N / Ctrl+W / Ctrl+T, `?`, Ctrl+C) plus the key dashboard / navigation actions, matched case-insensitively.
- **Does not assert:** exact overlay layout / wording.
- **Platform coverage:** mac+linux+windows.


### Theme contrast

Under PRD #13's terminal-relative color model there is no baked light/dark palette, so the per-theme snapshot *pairs* collapse into structural-property assertions: the dashboard may emit no absolute `Color::Rgb(..)` on any contrast-critical surface — backgrounds resolve to `Color::Reset` (the terminal's own background) and selection/active-tab highlights are cued without an absolute background tint.

#### theme/contrast

##### theme/contrast/001 — Overlay/prompt surfaces render in the terminal's reference frame (Reset background, Reset/ANSI foregrounds, no absolute Rgb).
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none.
- **Asserts:** the five overlay/prompt surfaces (stats bar, Quit-confirm, Stop-confirm, star prompt, config-gen prompt) emit no absolute `Color::Rgb(..)` token (foreground or background) — every cell is `Color::Reset` or a named ANSI color, so the surfaces inherit the terminal's own background and theme.
- **Does not assert:** accent/status colors (Cyan/Green/Yellow/Red/Blue/Magenta), which are named ANSI and remain by design; popup geometry beyond what the buffer captures.
- **Platform coverage:** mac+linux+windows.

#### theme/guard

##### theme/guard/001 — No absolute background on any cheaply-seamable surface; selection is cued by the Cyan+BOLD border, not an absolute fill.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none.
- **Asserts:** rendering the five overlay seams plus a session card in both the unselected and selected states, (a) no cell carries a `Color::Rgb(..)` background — backgrounds must be `Color::Reset`; and (b) the selected card is distinguished from the unselected one by a terminal-relative cue (the `▸ ` title prefix and a Cyan+BOLD border) rather than an absolute `selected_bg` fill.
- **Does not assert:** named-ANSI accents/status colors; the `render_frame` canvas/tab-bar fills (not cheaply reachable through a render seam — guarded by `theme/guard/002`).
- **Platform coverage:** mac+linux+windows.

##### theme/guard/002 — `src/ui.rs` carries no forbidden absolute-background patterns (source lint).
- **Layer:** L1 (source lint — reads `src/ui.rs` from disk; no rendering).
- **Agent:** none.
- **Asserts:** `src/ui.rs` contains none of `bg(Color::Rgb`, `bg(palette.terminal_bg)`, `bg(palette.selected_bg)`, `bg(palette.tab_bar_bg)` — guarding the `render_frame` canvas/tab-bar fills that paint the whole window and aren't cheaply reachable through a render seam.
- **Does not assert:** runtime rendering behavior (covered by `theme/guard/001` and `theme/contrast/001`); absolute colors in other source files.
- **Platform coverage:** mac+linux+windows.


### Scheduled tasks (PRD #127)

#### scheduler/reload

##### scheduler/reload/001 — A `ReloadSchedules` control message re-reads the global config and diff/replaces the registered task set without a daemon restart (PRD #127 M1.3).
- **Layer:** L2.
- **Agent:** none (drives `daemon serve` over the attach socket).
- **Asserts:** after editing the global `schedules.toml` to drop one task and add another and sending `ReloadSchedules`, the response is ok and the registered (enabled) task set contains the added task and not the removed one — with the same daemon process.
- **Does not assert:** persistence across an actual daemon restart (out of scope per PRD #127); the cron-firing behavior of the reloaded tasks.
- **Platform coverage:** mac+linux.

##### scheduler/reload/002 — A prompt-ONLY edit (same name + cron, new `prompt`) followed by `ReloadSchedules` is honored on the next fire: the spawned agent receives the NEW prompt, not the value captured at first registration (PRD #127 finding).
- **Layer:** L2.
- **Agent:** none (rewrites the global `schedules.toml`, sends `ReloadSchedules`, then drives a run-now fire; observes `ListAgents` + the spawned single-agent card's PTY prompt echo).
- **Asserts:** after registering a single-agent task with prompt `PROMPT_ALPHA`, rewriting the file to change ONLY the prompt to `PROMPT_BRAVO`, and reloading, a run-now fire spawns exactly one agent whose PTY echoes `PROMPT_BRAVO` and never the stale `PROMPT_ALPHA`.
- **Does not assert:** cron-change reload behavior (covered by `scheduler/reload/001`); reuse vs new-tab semantics; the exact reload diff mechanism (black-box on delivered prompt only).
- **Platform coverage:** mac+linux.

#### scheduler/cli

##### scheduler/cli/002 — `dot-agent-deck schedule add` from an arbitrary cwd writes the global `schedules.toml` and triggers a live daemon reload (PRD #127 M1.5).
- **Layer:** L2.
- **Agent:** none (runs the `schedule` CLI subprocess against a live `daemon serve`).
- **Asserts:** running `schedule add` from a directory that is not the global config dir writes the entry to the fixed global path (and not under the cwd), and the running daemon registers the new task via the add-triggered reload (probed via `schedule run-now`).
- **Does not assert:** cron validation / rename rejection / atomic-write internals (covered by the pure-data `scheduler/cli/001` unit tests alongside the CLI).
- **Platform coverage:** mac+linux.

##### scheduler/cli/003 — `dot-agent-deck schedule add` rejects a missing `--command` with a non-zero exit and a clear "command required" error (PRD #127 follow-up).
- **Layer:** L2.
- **Agent:** none (runs the `schedule` CLI subprocess against a live `daemon serve`).
- **Asserts:** running `schedule add` with a complete, valid flag set (name/cron/working-dir/prompt/enabled) but no `--command` exits non-zero and prints a stderr error indicating that `--command` is required — so the writer no longer silently accepts a task that would fall back to a bare `$SHELL`.
- **Does not assert:** the exact error wording (loose substring on "command" + "required"); validation of any other field; on-disk write effects.
- **Platform coverage:** mac+linux.

#### scheduler/spawn

##### scheduler/spawn/001 — A fire into a missing working_dir creates it (`mkdir -p`) then spawns; a fire into an uncreatable path surfaces a notification without crashing the daemon, and other tasks keep working (PRD #127 M2.1).
- **Layer:** L2.
- **Agent:** none (run-now drives the fire; observes the daemon registry + on-disk effects + daemon stderr).
- **Asserts:** firing a task whose working_dir does not exist creates the directory and spawns an agent; firing a task whose working_dir is uncreatable (parent is a regular file) leaves the daemon alive, does not create the path, surfaces a failure notification, and a sibling healthy task still spawns afterward.
- **Does not assert:** the exact notification message text (loose substring on the offending path).
- **Platform coverage:** mac+linux.

##### scheduler/spawn/002 — A fire into a dir with `[[orchestrations]]` opens an orchestration tab and delivers the prompt to the `orchestrator` role; a fire into a dir without one opens a single-agent card with the prompt delivered (PRD #127 M2.1).
- **Layer:** L2.
- **Agent:** none (run-now; observes `ListAgents` tab_membership + PTY prompt echo).
- **Asserts:** the orchestration fire registers an agent tagged as the orchestration's `orchestrator` role and the prompt is echoed by its PTY; the plain fire registers a non-orchestration single-agent card and the prompt is echoed by its PTY.
- **Does not assert:** orchestration role layout beyond the orchestrator slot; any LLM behavior (commands are plain `cat`).
- **Note:** every task carries a `command` (required to LOAD even for orchestration targets, whose fire is driven by the target dir's role command — so the task `command` is ignored at fire time).
- **Platform coverage:** mac+linux.

##### scheduler/spawn/003 — A fire spawns the task's configured `command` (its on-disk marker appears) (PRD #127 M2.1; command-required follow-up).
- **Layer:** L2.
- **Agent:** none (run-now; observes the on-disk marker side effect of the spawned command).
- **Asserts:** a task with an explicit `command` runs that command (its marker file appears), proving the scheduler spawns the configured command itself.
- **Does not assert:** any `$SHELL` fallback — `command` is now a required field, so there is no implicit-shell case (the former omitted-command fallback was removed); prompt delivery for this case (covered by spawn/002 + spawn/004).
- **Platform coverage:** mac+linux.

##### scheduler/spawn/004 — A single fire calls spawn exactly once and delivers the configured prompt (no double-spawn, no missed delivery) (PRD #127 M2.3).
- **Layer:** L2.
- **Agent:** none (run-now; observes registry agent count + PTY prompt echo).
- **Asserts:** one run-now spawns exactly one agent (count stays at 1 across a short window) and the configured prompt is echoed by that agent's PTY.
- **Does not assert:** tab-reuse vs `new_tab_per_fire` semantics (Phase 2B).
- **Platform coverage:** mac+linux.

##### scheduler/spawn/005 — A scheduled single-agent fire does NOT deliver its prompt until the agent's `SessionStart` is observed; delivery is gated on readiness, not a flat 300ms timer (PRD #127 scheduled-prompt readiness bug).
- **Layer:** L2.
- **Agent:** none (run-now; observes PTY prompt echo + injects the agent's real `SessionStart` hook carrying the spawned pane's `pane_id` + registry `agent_id`).
- **Asserts:** firing a `cat` task (no hook of its own) leaves the prompt UNDELIVERED for a window well past the old flat 300ms buffer while no matching `SessionStart` has been observed; once the real `SessionStart` hook (pane_id + agent_id) is injected, the prompt IS delivered (echoed by `cat`), well inside the 10s gate fallback so delivery is attributable to readiness, not the timeout.
- **Does not assert:** the 10s fallback-on-timeout delivery path (a separate readiness facet); orchestration-tab delivery gating (covered structurally by spawn/002).
- **Platform coverage:** mac+linux.

#### scheduler/reuse

##### scheduler/reuse/001 — Two fires of a `new_tab_per_fire = false` task reuse one tab and re-deliver the prompt into the same pane (PRD #127 M2.2).
- **Layer:** L2.
- **Agent:** none (run-now ×2; observes registry agent count + PTY prompt-echo occurrence count).
- **Asserts:** across two fires the agent count for the task stays at 1 (never grows to 2), and the prompt marker is echoed twice by the single reused PTY (the second fire delivers into the existing pane).
- **Does not assert:** behavior after the reused tab is closed (stale-entry eviction is unit-tested by the coder).
- **Platform coverage:** mac+linux.

##### scheduler/reuse/002 — Two fires of a `new_tab_per_fire = true` task open two distinct tabs, each receiving the prompt (PRD #127 M2.2).
- **Layer:** L2.
- **Agent:** none (run-now ×2; observes registry agent count + per-pane prompt echo).
- **Asserts:** the agent count goes 1 → 2 (two distinct panes) and each pane receives the prompt.
- **Does not assert:** ordering of the two tabs; tab titles.
- **Platform coverage:** mac+linux.

##### scheduler/reuse/003 — On a reuse fire, a recent user keystroke debounces delivery until the pane goes idle; with no recent input the prompt is delivered immediately (PRD #127 M2.2, Q6).
- **Layer:** L2.
- **Agent:** none (run-now + simulated STREAM_IN keystroke; observes PTY prompt-echo occurrence count over time). Debounce window injected via `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` so the test is fast.
- **Asserts:** after a simulated keystroke, a reuse fire's prompt is NOT delivered within the debounce window and IS delivered into the same pane once the window elapses; a later fire with no recent input is delivered immediately.
- **Does not assert:** the production default debounce duration (the test injects a short one); queue depth beyond the latest prompt.
- **Platform coverage:** mac+linux.

#### scheduler/manager

##### scheduler/manager/001 — The "Scheduled Tasks" manager dialog lists schedules with a live/idle/disabled status indicator and a next-fire time, and its action buttons show their shortcut keys (PRD #127 M3.3).
- **Layer:** L2 (no public L1 dialog render seam — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid). Opened with the `S` keybinding.
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`).
- **Asserts:** pressing `S` opens a "Scheduled Tasks" dialog listing the configured tasks; an enabled-but-not-live task shows an `idle` status; a disabled task shows the `disabled` indicator with a `—` next-fire placeholder; each action button advertises its keyboard shortcut alongside the label (`[Add a]` / `[Edit e]` / `[Delete d]` / `[Run now r]`), mirroring the `[Scheduled Tasks s]` button-bar button.
- **Does not assert:** the exact next-fire timestamp formatting for enabled tasks; live-status rendering when a reused tab exists; the action buttons' click behavior (covered by `mouse/modal/001`).
- **Platform coverage:** mac+linux.

##### scheduler/manager/002 — `a` (add) / `Enter`/`e` (edit) spawn the seeded authoring agent; edit pre-fills the row's current values (PRD #127 M3.3).
- **Layer:** L2 (same no-L1-seam reason). `claude` is shimmed to a recorder agent that posts SessionStart and records its delivered seed.
- **Agent:** the shimmed authoring agent (records the gated-delivered seed, mirroring how `tabs/mode/005` observes seed delivery).
- **Asserts:** pressing `e` on a row spawns the seeded authoring agent and the edit context is pre-filled — the agent receives the authoring seed carrying the row's current prompt value.
- **Does not assert:** the full authoring seed-prompt text; that the agent ultimately calls `schedule update` (covered by the CLI + seed-delivery mechanism); the add (blank) path beyond reuse of the same seam.
- **Platform coverage:** mac+linux.

##### scheduler/manager/003 — `d` + confirm removes the schedule definition but does NOT close an already-open tab for it (PRD #127 M3.3).
- **Layer:** L2 (same no-L1-seam reason). Drives the real dialog + observes the global `schedules.toml` and the daemon registry.
- **Agent:** none (the schedule's own `cat` agent, opened by a prior run-now, stands in for an open tab).
- **Asserts:** after `d` then confirm (`y`), the definition is gone from `schedules.toml`, AND a tab/agent opened for that task before the delete is still live in the registry.
- **Does not assert:** the confirmation dialog's exact wording; rename behavior (forbidden, unit-tested).
- **Platform coverage:** mac+linux.

##### scheduler/manager/004 — `r` on a row triggers an immediate run-now fire of the selected task (PRD #127 M3.3).
- **Layer:** L2 (same no-L1-seam reason). Drives the real dialog + observes the daemon registry.
- **Asserts:** pressing `r` in the manager fires the selected task, which spawns its tab/agent (registered under the task's display name).
- **Does not assert:** prompt delivery content (covered by `scheduler/spawn/004`); reuse vs new-tab on the fire.
- **Platform coverage:** mac+linux.

##### scheduler/manager/005 — The delete confirmation stays contained within the modal even for a long schedule name (PRD #127 finding).
- **Layer:** L2 (same no-L1-seam reason). Drives the real dialog via `S` + `d` and asserts on the rendered vt100 grid.
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`, one enabled task with a deliberately long name).
- **Asserts:** after arming delete (`d`) on a long-named row, the confirmation's trailing `(y/n)` prompt — the only `(y/n)` in the app — still renders, proving the message is contained within the modal (wrapped, name on its own line) instead of overflowing the inner width and clipping the tail off the right border.
- **Does not assert:** the exact wrap points / line count; the modal's precise capped width; the confirmation wording beyond the `(y/n)` tail and `Delete schedule` prefix.
- **Platform coverage:** mac+linux.

##### scheduler/manager/006 — Clicking a schedule row moves the selection to that row (PRD #127 finding — mouse parity).
- **Layer:** L2 (same no-L1-seam reason). Drives the real dialog via `S`, then a left-click SGR mouse report on a row, asserting on the rendered vt100 grid.
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`, two enabled tasks).
- **Asserts:** with two rows (`alpha` auto-selected, `bravo` not), clicking the `bravo` row moves the `▶` selection marker to it (`▶ bravo` renders and `▶ alpha` is gone), proving a row click hit-tests and re-selects.
- **Does not assert:** that the click also fires an action (it only selects); keyboard j/k navigation (the pre-existing selection path); scroll-into-view when the clicked row is off-window.
- **Platform coverage:** mac+linux.

#### scheduler/live

##### scheduler/live/001 — A scheduled fire surfaces its card LIVE to an already-attached TUI, without a disconnect/reconnect (PRD #127 finding #2).
- **Layer:** L2 (real TUI driven via PTY; observed on the rendered vt100 grid — the only surface where the bug shows, since the daemon registry holds the agent in both states). Fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`; fired with the `RunNow` control message over the deck's attach socket.
- **Agent:** none (a plain `cat` command — no hooks — so the only path that could surface a card is a new-agent broadcast, not a hook event).
- **Asserts:** after firing a `cat`-command schedule into the daemon the attached TUI is connected to, the agent is registered in the daemon (precondition), AND a card for it appears on the already-attached dashboard live (the task name renders) — no detach/reattach.
- **Does not assert:** prompt delivery content; the card's status badge / body layout; behavior after a reconnect (which already masks the bug via startup hydration).
- **Platform coverage:** mac+linux.

##### scheduler/live/002 — A scheduled (daemon-spawned) card survives being focused — focus re-hydrates it instead of deleting it (PRD #127 finding #2).
- **Layer:** L2 (real TUI driven via PTY; observed on the rendered vt100 grid). Fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`; fired with `RunNow`. A `SessionStart` hook carrying the daemon-spawned agent's own `DOT_AGENT_DECK_PANE_ID` (read back from the registry) is injected to paint the card — faithfully mirroring what a real agent's hook does.
- **Agent:** none (long-lived `cat`; the hook is injected by the harness with the agent's real pane id so the card is backed by a live daemon agent but not a local TUI pane — the orphan-card condition).
- **Asserts:** the hook paints a card on the attached dashboard (precondition, holds in the broken state too), and pressing the `1` jump key to focus that card keeps it usable — the TUI enters PaneInput mode on the re-hydrated pane (the card is not deleted).
- **Does not assert:** the exact pane contents after focus; the live-surfacing path for the non-hook case (covered by `scheduler/live/001`).
- **Platform coverage:** mac+linux.

##### scheduler/live/003 — A live-surfaced scheduled card's TITLE shows the schedule's friendly name, not the truncated spawn pane-id (PRD #127 finding #2 regression).
- **Layer:** L2 (real TUI driven via PTY; observed on the rendered vt100 grid). Fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`; fired with the `RunNow` control message over the deck's attach socket. The schedule's `working_dir` basename (`runbox`) is deliberately unrelated to its name (`morning-digest`) so the friendly name can only reach the grid through the card title — not the Dir line.
- **Agent:** none (a plain `cat` command — no hooks; the card surfaces via the new-agent broadcast as in `scheduler/live/001`).
- **Asserts:** after a fire into the attached daemon, the agent is registered under its friendly name (precondition) and the card surfaces live (its Dir line shows the cwd basename), AND the card TITLE shows the friendly name `morning-digest` — matching a reconnect — and NOT the truncated spawn pane-id form (`… · sched-morni…`).
- **Does not assert:** the surfacing path itself (covered by `scheduler/live/001`); focus survival (covered by `scheduler/live/002`); the title after a reconnect (which already masks the bug via startup hydration); the card's status badge / body layout.
- **Platform coverage:** mac+linux.

##### scheduler/live/004 — A live-surfaced scheduled card's friendly TITLE SURVIVES being superseded by the agent's real `SessionStart` hook — it does not revert to the session-id hash (PRD #127 finding #2, hook-supersession gap).
- **Layer:** L2 (real TUI driven via PTY; observed on the rendered vt100 grid). Fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`; fired with `RunNow`. The schedule's `working_dir` basename (`runbox`) is deliberately unrelated to its name (`morning-digest`) so the friendly name can only reach the grid through the card title. After the synthetic placeholder surfaces, a real `SessionStart` hook is injected carrying the spawned pane's pane id AND its spawn-injected registry agent id (both read back from the registry) and NO display_name metadata — faithfully reproducing what a hook-emitting claude/opencode agent emits.
- **Agent:** none (a plain `cat` command; the synthetic placeholder surfaces via the new-agent broadcast as in `scheduler/live/001`, then the harness injects the agent's real `SessionStart` hook — a `Some(agent_id)` distinct from the placeholder's `None` — to drive the supersession the primary hook-emitting scheduler case hits).
- **Asserts:** after the placeholder surfaces with the friendly title `morning-digest` and the real hook supersedes it (the "No agent" placeholder becomes a live ClaudeCode card), the card TITLE STILL shows `morning-digest` (matching a reconnect) and has NOT reverted to the session-id hash form (`… · 9f8e7d6c-5b…`).
- **Does not assert:** the surfacing path itself (covered by `scheduler/live/001`); focus survival (covered by `scheduler/live/002`); the no-hook title case (covered by `scheduler/live/003`); the title after a reconnect (which already masks the bug via startup hydration); the card's status badge / body layout.
- **Platform coverage:** mac+linux.


### Experimental feature flag (PRD #139)

#### features/gating

##### features/gating/001 — Dashboard rendered with the experimental flag forced ON shows the `experimental: on` footer.
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** `render_experimental_footer_to_buffer(&Features::test_with(true), 80, 1)` renders a buffer containing the exact label `experimental: on`; the stringified buffer matches the committed snapshot.
- **Does not assert:** the footer's absolute placement within the full dashboard layout (the seam renders the standalone footer region); colour/style of the label.
- **Platform coverage:** mac+linux+windows.

##### features/gating/002 — Dashboard rendered with the experimental flag forced OFF shows NO footer (blank pre-feature baseline).
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** `render_experimental_footer_to_buffer(&Features::test_with(false), 80, 1)` renders a buffer containing no `experimental` text; the stringified buffer matches the committed blank-baseline snapshot — identical to how the region looked before the surface existed.
- **Does not assert:** the ON path (covered by `features/gating/001`); any behavioural difference beyond the rendered footer region.
- **Platform coverage:** mac+linux+windows.

##### features/gating/003 — `DOT_AGENT_DECK_EXPERIMENTAL=1` surfaces the `experimental: on` footer end-to-end; the default (OFF) hides it.
- **Layer:** L2 (real TUI driven via PTY; observed on the rendered vt100 grid). The flag is injected through the spawned binary's env (`with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")`); a control launch sets no env var. The harness `env_clear`s the child env, so the control run is a clean OFF.
- **Agent:** none (`minimal` fixture; empty dashboard).
- **Asserts:** with the env var set, the rendered grid shows the `experimental: on` footer once the dashboard is up; the control launch (no env var) never shows it once the dashboard is up and quiescent.
- **Does not assert:** the TOML-file enable path or env-vs-file precedence (covered by `features/reload/001` and the unit suite); the footer's absolute grid coordinates.
- **Platform coverage:** mac+linux.

#### features/reload

##### features/reload/001 — A live `[features]` flip from OFF to ON re-surfaces the footer on the next render, no restart.
- **Layer:** L1 (in-process `TestBackend` + a synthetic config-file event; PRD #139 M2.2).
- **Agent:** none.
- **Asserts:** starting from a shared `Features` value (M1.2's per-process `Arc<RwLock<Features>>`) with `experimental = false`, the wrapper `features::show_experimental_footer()` reports hidden and the rendered footer is absent; after a synthetic `.dot-agent-deck.toml` change flips `experimental -> true` (modeled via `features::set_for_test(..)`), the wrapper re-evaluates to visible and the next render shows the `experimental: on` footer — with no process restart.
- **Does not assert:** the real file-watcher / debounce mechanics (the synthetic event stands in for the watcher's apply step); env-override precedence; partial/invalid-TOML reload handling (unit-covered).
- **Platform coverage:** mac+linux+windows.

### Docs cross-reference skips

Per Decision 27, documented user-facing behaviors that are deliberately not catalogued at M1:

| Doc behavior | Why skipped |
|---|---|
| Idle ASCII art rendering on cards ([docs/configuration.md#idle-ascii-art](../docs/configuration.md), [docs/configuration.md#standalone-cli](../docs/configuration.md)) | LLM-driven side feature; lives outside the deck/daemon/PTY surface the harness covers. Reconsider in M4+ if the feature warrants its own catalog section. |
| `dot-agent-deck connect <remote>` end-to-end SSH flow ([docs/remote-environments.md](../docs/remote-environments.md), [docs/remote-recipes.md](../docs/remote-recipes.md)) | Requires a remote-harness shape that does not exist yet. Catalogued at M4+ when remote testing lands. Local quit-dialog coverage (`prompt/quit/001`–`005`) already pins the Detach / Stop / Cancel behavior; remote attach adds only the daemon-side log distinction. |
| `dot-agent-deck remote add / list / upgrade / remove` ([docs/remote-environments.md](../docs/remote-environments.md)) | Same — remote-harness territory; the lib already covers the pure-data slices (URL parsing, command construction, error classification) in the kept tests. **Security properties deferred to M4+ end-to-end coverage:** shell-metacharacter quoting on remote-CLI argv assembly (unit-covered by `system_ssh_executor_quotes_arguments_safely`), `remotes.toml` written at mode 0o600 (covered by the now-moved `remotes_toml_written_at_0o600` test — restore at M4+), `DOT_AGENT_DECK_VIA_DAEMON=1` propagation on the remote shell (unit-covered by `build_connect_command_has_t_flag_and_via_daemon_env`). |
| `dot-agent-deck ascii` CLI subcommand ([docs/configuration.md#standalone-cli](../docs/configuration.md)) | Non-TUI subcommand; tested as a CLI smoke in M4+ if it warrants coverage. |
| `dot-agent-deck validate` CLI subcommand ([docs/workspace-modes.md#config-validation](../docs/workspace-modes.md)) | Non-TUI; the underlying validator is exhaustively covered by the pure-data `config_validation` tests. |
| `dot-agent-deck watch` CLI subcommand ([docs/workspace-modes.md#dot-agent-deck-watch](../docs/workspace-modes.md)) | Non-TUI subcommand; an L2 test would only exercise its output formatting against a real shell — low value compared to the deck-rendering surface. |
| `dot-agent-deck config get` / `config set` ([docs/configuration.md](../docs/configuration.md)) | Non-TUI; the underlying config field reflection is covered by pure-data tests (`*_get_set_field`, `*_get_set_fields`). |
| `dot-agent-deck hooks install` / `uninstall` CLI commands ([docs/troubleshooting.md#hooks](../docs/troubleshooting.md)) | Auto-install path is catalogued as `hooks/install/001`–`003`; the explicit subcommand variants share the same install/uninstall code. A targeted L2 test will be added only if a divergence appears. |
| Ghostty-specific Shift+Enter terminal config ([docs/troubleshooting.md#shift-enter-not-working-in-ghostty-terminal](../docs/troubleshooting.md)) | Outer-terminal config; no deck-side surface to test. |
| Mode-tab card jump via `Enter` (broken per docs note → [#68](https://github.com/vfarcic/dot-agent-deck/issues/68)) | Documented as broken. The catalog will gain an entry once the bug is closed; until then leaving it uncovered avoids pinning the broken behavior. |
| `--continue` "dashboard-first landing" detail ([docs/session-management.md#resuming-sessions](../docs/session-management.md)) | Implicit consequence of `session/restore/001`; not separately worth a catalog ID. Reconsider if the landing-tab logic ever has its own surface. |
