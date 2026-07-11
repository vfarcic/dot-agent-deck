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

##### dashboard/pane/007 — A Pi pane's card renders the Pi agent-type identity (PRD #201 M2.2).
- **Layer:** L1 (ratatui `TestBackend` + `insta`-style buffer text assertion).
- **Agent:** none (a fixture `SessionState` with `agent_type = AgentType::Pi` and no display name).
- **Asserts:** with the experimental flag forced ON (`features::set_for_test(Features::test_with(true))` — the Pi identity is flag-gated at the render seam per M5.1), a live Pi session with no friendly name renders its card title in the `<agent-type> · <session-id>` form showing the Pi identity (`Pi · orch-01`); the fixture's cwd basename and session id carry no capital `Pi`, so the match pins the agent-type Display specifically. The card must NOT show `ClaudeCode` / `OpenCode` / `No agent` — a plain `pi` pane is first-class, not "No agent".
- **Does not assert:** the flag-OFF (hidden) path of the Pi render gate (covered by `features/gating/004`); the status badge color (`status/badge/001`).
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

##### dashboard/selection/001 — While the selection is active, `j` / `Down` selects the next card and wraps at the end.
- **Layer:** L1 (in-process `handle_normal_key` dispatch).
- **Agent:** none (synthetic card count).
- **Asserts:** starting active on card 0, `j` advances 0→1, `Down` advances 1→2, and `j` wraps 2→0; the selection stays active (`Some(idx)`) throughout.
- **Does not assert:** how the highlight is drawn (covered by `dashboard/selection/010`); the inactive-start jump-to-first (`dashboard/selection/006`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/002 — While the selection is active, `k` / `Up` selects the previous card and wraps at the start.
- **Layer:** L1 (in-process `handle_normal_key` dispatch).
- **Agent:** none.
- **Asserts:** starting active on card 0, `k` wraps 0→2 and `Up` retreats 2→1; the selection stays active throughout.
- **Does not assert:** the inactive-start jump-to-last (`dashboard/selection/007`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/003 — `1`–`9` jumps to card N, focuses its pane, and activates the highlight — even when the selection was inactive.
- **Layer:** L1 (in-process `focus_deck` dispatch).
- **Agent:** none (3 synthetic sessions with pane ids).
- **Asserts:** starting from an inactive selection, `focus_deck(1, …)` activates the highlight on index 1 (`Some(1)`), focuses that card's pane, and enters PaneInput mode.
- **Does not assert:** what `0` or digits past the card count do (kept open until catalogued).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/004 — `Esc` clears an active filter.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with the filter dialog populated, pressing `Esc` returns the visible cards to the unfiltered set.
- **Does not assert:** filter dialog dismissal animation.
- **Platform coverage:** mac+linux.

##### dashboard/selection/005 — A tab switch away from the Dashboard and back clears the card highlight.
- **Layer:** L1 (in-process `dispatch_action` tab-switch path + renderer).
- **Agent:** none (a real second Mode tab; 3 synthetic dashboard cards).
- **Asserts:** with the highlight active on card 2, driving `Action::CycleTabNext` then `Action::CycleTabPrev` leaves the dashboard selection inactive (`None`), and `render_dashboard_cards_to_buffer` paints no `▸` selection marker on any card.
- **Does not assert:** the cyan focus border on embedded panes (unaffected); Mode/Orchestration tab side-pane focus (out of scope).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/006 — With the selection inactive, `j` jumps to the first card and activates the highlight.
- **Layer:** L1 (in-process `handle_normal_key` dispatch).
- **Agent:** none.
- **Asserts:** from an inactive selection (`None`), `j` lands the highlight on the first card (`Some(0)`) and the selection becomes active.
- **Does not assert:** the active-state next/wrap behaviour (`dashboard/selection/001`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/007 — With the selection inactive, `k` jumps to the last card and activates the highlight.
- **Layer:** L1 (in-process `handle_normal_key` dispatch).
- **Agent:** none.
- **Asserts:** from an inactive selection (`None`) with 3 cards, `k` lands the highlight on the last card (`Some(2)`) and the selection becomes active.
- **Does not assert:** the active-state prev/wrap behaviour (`dashboard/selection/002`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/008 — With the selection inactive, Enter restores the previously-selected card (not card 0).
- **Layer:** L1 (in-process `switch_tab_with_focus` round-trip + `handle_normal_key` + `dashboard_focus_target`).
- **Agent:** none (3 synthetic dashboard cards; a Mode tab as the round-trip intermediate).
- **Asserts:** with the highlight armed on a non-first card (index 1), a real Dashboard → Mode → Dashboard round-trip clears the live highlight (`selected_index == None`) but the Enter focus target (`dashboard_focus_target`) is the REMEMBERED card (index 1), not card 0; Enter still maps to `Action::Focus`; the active-selection target is the highlighted card and the no-cards target is `None` (both unchanged). Pins the PRD #113 design revision (2026-06-13) Enter-restores-previous behavior.
- **Does not assert:** the pane-focus side effect of `Action::Focus` itself (exercised by `dashboard/selection/003`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/009 — A focused dashboard pane reactivates the highlight on its card.
- **Layer:** L1 (in-process `reconcile_dashboard_selection`).
- **Agent:** none (3 synthetic `(session_id, pane_id)` pairs).
- **Asserts:** from an inactive selection, reconciling with a focused pane that maps to card 1 activates the highlight on `Some(1)`; reconciling with no matching focused pane leaves the selection inactive.
- **Does not assert:** how the focused pane id is obtained from the embedded controller (the per-frame `pane.focused_pane_id()` read).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/010 — Startup default: the dashboard is active on card 0 and paints its highlight.
- **Layer:** L1 (in-process state + renderer).
- **Agent:** none.
- **Asserts:** a freshly-built `UiState` is active on card 0 (`Some(0)`); rendering with that selection paints the `▸` marker on the first card's title row, while rendering with an inactive selection (`None`) paints no marker.
- **Does not assert:** the `Last: … Tools: …` card body (covered by `dashboard/pane/*`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/011 — Switching Dashboard → Orchestration → Dashboard leaves the selection inactive (SC1, any-other-tab path).
- **Layer:** L1 (in-process `switch_tab_with_focus` + per-frame `reconcile_dashboard_selection`).
- **Agent:** none (a real Orchestration tab; 3 synthetic dashboard cards).
- **Asserts:** with the highlight armed on card 2, driving the real switch path to an Orchestration tab and back — running the real per-frame reconcile on each frame — leaves `selected_index == None`. Covers the path `selection/005` cannot (the Orchestration tab shares `selected_index` and its always-active reconcile re-arms `Some(0)` in transit, while deactivation fires only on Dashboard-leave).
- **Does not assert:** Orchestration role-pane selection behaviour itself (covered by `tabs/selection/*`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/012 — An inactive selection makes the close-pane action a no-op (no fall back to card 0).
- **Layer:** L1 (in-process `dispatch_action(Action::CloseSelected)`).
- **Agent:** none (3 synthetic dashboard cards with pane ids).
- **Asserts:** with `selected_index = None` (inactive, nothing armed), dispatching `Action::CloseSelected` issues no `close_pane` call and removes no session — it does NOT close card 0. Encodes the PRD invariant (inactive = nothing armed) alongside `dashboard/pane/003`.
- **Does not assert:** the active-selection close behaviour, or mode/orchestration whole-tab teardown.
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/013 — A steady-state restored focus must not reactivate the highlight after a tab round-trip.
- **Layer:** L1 (in-process `switch_tab_with_focus` + per-frame `reconcile_dashboard_selection`).
- **Agent:** none (a real Mode tab whose agent pane is also a Dashboard card; 3 synthetic cards).
- **Asserts:** driving the real per-frame reconcile across a Dashboard → Mode → Dashboard round-trip, where the Mode agent pane stays focused on both the mode frame and the return dashboard frame (no focus transition), leaves `selected_index == None` — the blue highlight does not reappear. Regression for PR #151; this is the steady-state-focus path `selection_005`/`selection_011` cannot reach.
- **Does not assert:** the cyan controller focus border (driven separately, unaffected).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/014 — A genuine focus transition after a steady-state baseline still reactivates the highlight (M4 not over-suppressed).
- **Layer:** L1 (in-process `reconcile_dashboard_selection`).
- **Agent:** none (3 synthetic `(session_id, pane_id)` pairs).
- **Asserts:** from an inactive selection, holding a non-card pane focused across two frames keeps the selection inactive; then transitioning the focus to a dashboard card reactivates the highlight on that card (`Some(0)`). Guards that the focus-transition fix does not block legitimate M4 reactivation; distinct from `selection_009` (transition from the `None` baseline).
- **Does not assert:** the active-selection derive path (covered by `dashboard/pane/005`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/015 — SC1 against the real binary: the highlight clears on a tab round-trip when the focused pane is a Mode agent pane that is also a dashboard card.
- **Layer:** L2 (real `dot-agent-deck` binary in a PTY; vt100 grid scraping).
- **Agent:** a Mode tab agent (fixture shell script) that self-posts `SessionStart` so its agent pane is also a dashboard card; no LLM tokens.
- **Asserts:** with the highlight armed on the Dashboard (a `▸` marker present), switching away to the Mode tab and back to the Dashboard — where the Mode agent pane stays focused (steady state, no transition) and maps to a card — leaves NO `▸` selection marker on any card. This is the real-binary repro the L1 tests cannot provide (their mocks never restore focus to a Mode agent pane on return); pre-fix the steady-state focus re-armed the highlight.
- **Does not assert:** the cyan controller focus border (driven separately, unaffected); the keyboard nav/wrap semantics (covered by `dashboard/selection/001`–`002`).
- **Platform coverage:** mac+linux.

##### dashboard/selection/016 — The inactive-selection close no-op (012) does NOT suppress closing an active Mode/Orchestration tab via Ctrl+W.
- **Layer:** L1 (in-process `dispatch_action(Action::CloseSelected)` against a recording `PaneController`).
- **Agent:** none (a real Mode tab, then a real Orchestration tab; no dashboard cards armed).
- **Asserts:** with a Mode tab active and `selected_index == None`, dispatching `Action::CloseSelected` closes that tab (tab count drops back to the lone Dashboard); the same holds for an active Orchestration tab. Bounds the `dashboard/selection/012` no-op gate: the inactive-selection guard suppresses closing an unarmed dashboard CARD, but a Mode/Orchestration TAB still closes via Ctrl+W. Regression for the PR #151 e2e failure `e2e_render_contract::layout_002` (keyboard Ctrl+W stopped closing a Mode tab because the close routed through the inactive-selection gate).
- **Does not assert:** the per-pane PTY teardown / role-pane stop (covered by the L2 `tabs/mode/002`, `tabs/orchestration/002`); the dashboard-card close no-op itself (covered by `dashboard/selection/012`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/017 — Enter (Action::Focus) paints the highlight on BOTH decks by setting `selected_index` to the restored target (unified deck behavior).
- **Layer:** L1 (in-process `dispatch_action(Action::Focus)` against a recording `PaneController`).
- **Agent:** none (a real Orchestration tab with placeholder role-pane sessions; 3 synthetic dashboard cards).
- **Asserts:** with the deck inactive (`selected_index == None`) and a remembered selection (`last_active_selection == Some(1)`), dispatching `Action::Focus` (what Enter maps to) sets `ui.selected_index = Some(1)` — so the highlight paints — for the ORCHESTRATION deck AND the Dashboard. Pins the unified fix for the PR #151 manual-test regression where Enter never painted the highlight on the Orchestration deck (the role pane was already focused on return, so the reconcile focus-transition guard never re-armed it). Pre-fix RED: `Action::Focus` only focuses the pane and leaves `selected_index == None`.
- **Does not assert:** the per-frame reconcile reactivation path (`dashboard/selection/009`/`014`); the focus side effect itself (`dashboard/selection/003`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/018 — On tab return, the previously-selected deck's PANE is re-focused while the highlight stays clear — symmetric across BOTH decks (unified deck behavior).
- **Layer:** L1 (in-process `switch_tab_with_focus` round-trip + recording `PaneController`).
- **Agent:** none (a real Mode tab as the round-trip intermediate; an Orchestration tab; 3 synthetic dashboard cards).
- **Asserts:** after a Dashboard → Mode → Dashboard round-trip with a remembered selection (card index 1 → session `s1` → pane `p1`), the controller's last-focused pane is `p1` (the remembered card's pane is re-focused) AND `selected_index == None` (highlight clear). The Orchestration deck already satisfies this (it re-focuses its remembered role pane on return). Pins the unified fix making the Dashboard leave/return symmetric with Orchestration. Pre-fix RED for the Dashboard: it re-focuses nothing on return (its `selected_session_id` is cleared on leave), so the last-focused pane is the Mode pane, not `p1`. Consistent with `dashboard/selection/013` (focused pane present on return, highlight `None`).
- **Does not assert:** the per-frame reconcile staying `None` under steady focus (covered by `dashboard/selection/013`); the scroll/viewport reveal of the remembered region.
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/019 — Enter paints the selection highlight on the Orchestration deck after a tab round-trip (real binary).
- **Layer:** L2 (real `dot-agent-deck` binary in a PTY; vt100 grid scraping; `e2e` feature).
- **Agent:** none (an orchestration with two `cat` role panes that stay alive as deck cards; no LLM tokens).
- **Asserts:** open the orchestration, detach to Normal mode, arm a role with `j` (a `▸` marker appears), round-trip Orchestration → Dashboard → Orchestration (the `▸` clears), then press Enter — the `▸` selection marker must reappear on the restored role. This is the real-binary repro of the PR #151 manual-test regression the L1 mocks missed (they never run the real reconcile + focus-restore on an orchestration tab): pre-fix the role pane is already focused on return, so Enter is not a focus transition and the highlight never repaints (the final wait times out).
- **Does not assert:** which role index is restored; the cyan controller focus border; the Dashboard's own Enter-paint (already worked via the reconcile transition and is covered at L1 by `dashboard/selection/017`).
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

#### status/agent-event

##### status/agent-event/001 — A `dot-agent-deck agent-event --type <state>` frame routes into the existing `AgentEvent` stream and drives the target pane's card status, with NO hook and no `settings.json` mutation (PRD #201 M1.2/M1.3).
- **Layer:** L1 (in-process — resolve the lifecycle state via the production seam `dot_agent_deck::event::agent_event_type_from_state`, build the `AgentEvent` via the agent-agnostic synthetic-agent harness, drive `AppState::apply_event`; no daemon socket, no PTY, no hook).
- **Agent:** none (synthetic — the harness at `AgentType::Pi` identity models the pane's injected `DOT_AGENT_DECK_PANE_ID` / `DOT_AGENT_DECK_AGENT_ID`).
- **Asserts:** `agent-event --type running` maps to an `EventType` via the seam; the built frame carries the pane id, agent id, and the Pi agent type; it serializes as a bare `AgentEvent` with NO `message_type` envelope and does NOT parse as a `DaemonMessage` (it rides the existing raw-event wire, zero new surface); routed through `apply_event` on the registered pane it drives the card to a busy (`Thinking`) status.
- **Does not assert:** the full CLI → daemon-socket → `run_hook_loop` path (real-`pi` e2e, M4); the exact `EventType` chosen for `running` beyond that it yields the `Thinking` badge.
- **Platform coverage:** mac+linux+windows.

##### status/agent-event/002 — The Pi synthetic agent emits `running` → `waiting` → `finished` via `agent-event` and the card badge follows each transition (PRD #201 M1.3).
- **Layer:** L1 (in-process — production state→EventType seam + `AppState::apply_event`, driven by the synthetic-agent harness).
- **Agent:** none (synthetic — the harness at `AgentType::Pi` identity).
- **Asserts:** each lifecycle state resolves through the seam (`running`→`Thinking`, `waiting`→`WaitingForInput`, `finished`→`Idle`) and, routed through `apply_event`, the derived `SessionStatus` (the badge source) moves `Thinking` → `WaitingForInput` → `Idle` in lock-step — with no hook and no `settings.json` mutation.
- **Does not assert:** the TS extension's Pi-event-bus → state mapping (M2.2 TS tests); the rendered badge glyph/color (`status/badge/001`).
- **Platform coverage:** mac+linux+windows.

##### status/agent-event/003 — A Pi pane reports running/waiting/finished HEADLESS/UNATTENDED via `agent-event` against the real `daemon serve`, with NO hook installed and no `~/.claude/settings.json` mutation (PRD #201 M2.2).
- **Layer:** L2 (headless `daemon serve` via the `DaemonProc` harness — no PTY, no attached TUI; spawns the real binary, so the `e2e` tier). The Pi extension is stood in for by the real `dot-agent-deck agent-event --type <state>` CLI subprocess; status is observed via an unattended `SubscribeEvents` consumer and the badge derived locally through `AppState::apply_event` (the same seam the production TUI subscriber uses). Hits no LLM.
- **Agent:** synthetic (the `agent-event` CLI reporting `AgentType::Pi` from a pane carrying the daemon's injected `DOT_AGENT_DECK_PANE_ID` / `DOT_AGENT_DECK_AGENT_ID`).
- **Asserts:** each `agent-event --type running|waiting|finished` exits 0 and is re-broadcast by the daemon as a bare `AgentEvent` carrying the Pi identity + injected ids + the mapped `EventType`; fed through `AppState::apply_event` the unattended badge moves `Thinking` → `WaitingForInput` → `Idle`; and a seeded sentinel `~/.claude/settings.json` (whose presence makes the hook-install guard pass) is byte-for-byte unchanged afterward and never gains a `dot-agent-deck` hook entry — proving the daemon/agent-event path installs no Claude hook.
- **Does not assert:** the real `pi` runtime + bundled extension end to end (real-`pi` e2e, M4.1); the daemon's own internal derived status over the wire (`AgentRecord` carries no status field; the broadcast is the observable).
- **Platform coverage:** linux (headless daemon-serve harness).

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

##### prompt/new-pane/010 — The new-pane Mode cycler offers an experimental `schedule: issues` issue-dispatch authoring option only when the experimental flag is ON; it is hidden when OFF while the plain `[schedule]` option still shows (PRD #120).
- **Layer:** L2 (no public L1 render seam for the dialog — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid).
- **Agent:** none (drives Ctrl+n → dir-picker → new-pane form in two flag states).
- **Asserts:** launched with `DOT_AGENT_DECK_EXPERIMENTAL=1`, opening the new-pane form shows a `schedule: issues` option on the Mode cycler alongside the existing `[schedule]` option; a control launch with no env var (flag OFF) renders the plain `[schedule]` option but NOT `schedule: issues`. RED until the option exists: today no flag state carries `schedule: issues`, so the experimental-ON grid never contains it.
- **Does not assert:** the authoring seed delivered when the option is selected (covered by `scheduler/form/007`); the post-submit layout; the chip's exact position in the cycler.
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

##### tabs/orchestration/003 — Switching tabs clears the Orchestration deck highlight across ALL tab switches, including orchestration-to-orchestration.
- **Layer:** L1 (in-process `switch_tab_with_focus` + per-frame `reconcile_dashboard_selection`).
- **Agent:** none (two real Orchestration tabs, two roles each).
- **Asserts:** with the orchestration highlight armed on role 1 and the focus baseline established, the highlight is inactive (`selected_index == None`) on the destination after a real round-trip plus the real per-frame reconcile, in BOTH cases: (Part 1) Orchestration → Dashboard → Orchestration — the destination restores the SAME role pane (steady-state focus, no transition); and (Part 2, PR #151 follow-up) Orchestration A → Orchestration B — the destination restores a DIFFERENT role pane than the source, which the first reconcile frame would otherwise read as a focus transition and re-arm. Pins the PRD #113 design revision (2026-06-13) Change 1 (symmetric clearing); analog of `dashboard/selection/011`/`013`.
- **Does not assert:** the cyan controller focus border (driven separately, unaffected); the orchestrator's spawn-time role prompt.
- **Platform coverage:** mac+linux+windows.

##### tabs/orchestration/004 — Enter restores the previously-selected role on the Orchestration deck (not role 0).
- **Layer:** L1 (in-process `switch_tab_with_focus` round-trip + `dashboard_focus_target`).
- **Agent:** none (a real Orchestration tab with two roles; a Mode tab as the round-trip intermediate).
- **Asserts:** with the orchestration highlight armed on role 1, a real Orchestration → Mode → Orchestration round-trip clears the live highlight (`selected_index == None`) but the Enter focus target (`dashboard_focus_target`, the same SSOT the Dashboard uses) is the REMEMBERED role (index 1), not role 0. Pins the PRD #113 design revision (2026-06-13) Change 2 (Enter restores previous) for the Orchestration deck.
- **Does not assert:** the pane-focus side effect of activating the role; the active-selection target.
- **Platform coverage:** mac+linux+windows.

##### tabs/orchestration/005 — Enter restore is per-deck: the Orchestration deck restores ITS OWN previous role, not a Dashboard selection leaked through shared state.
- **Layer:** L1 (in-process `switch_tab_with_focus` round-trip + `dashboard_focus_target`).
- **Agent:** none (a real Orchestration tab with three roles; the Dashboard as the round-trip intermediate).
- **Asserts:** arm the Orchestration deck on role 1, leave to the Dashboard, arm the Dashboard on card 2, then return to the (now inactive) Orchestration deck — Enter restores the Orchestration's OWN remembered role (index 1), NOT the Dashboard's leaked index 2. Pins per-deck independence of the Enter-restore state (the remembered selection must be stored per deck, not in a single shared field). Complements `tabs/orchestration/004` (which restores via a non-deck Mode-tab intermediate that can't clobber the shared field).
- **Does not assert:** the pane-focus side effect of activating the role; the Dashboard's own restore (covered by `dashboard/selection/008`).
- **Platform coverage:** mac+linux+windows.

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

#### tabs/spawn

##### tabs/spawn/001 — Creating a single-agent card while an Orchestration tab is active switches the active tab back to the Dashboard with the new card selected and focused (PRD #154).
- **Layer:** L1 (in-process — open a REAL Orchestration tab via `TabManager::open_orchestration_tab`, then dispatch the real `Action::SpawnPane` for a plain single-agent card through `dispatch_action` against a recording `OpenTabPC`; no daemon, no PTY).
- **Agent:** none (mock `PaneController` hands out `mock-pane-N` ids and records `focus_pane` calls).
- **Asserts:** with the orchestration tab active (the non-Dashboard launch precondition), dispatching the no-mode/no-orchestration `SpawnPane` leaves `tab_manager.active_index() == 0` (the Dashboard), sets `ui.selected_index` to the new card's index (`filtered.len()`), and focuses the freshly-created card pane (last `focus_pane` target). A single-agent card belongs to the Dashboard (tab 0), so it must not be stranded on the orchestration tab.
- **Does not assert:** how the highlight is drawn (covered by `dashboard/selection/010`); orchestration/mode tab creation switching to their OWN tab (`open_*_tab` paths, unchanged by PRD #154).
- **Platform coverage:** mac+linux+windows.

##### tabs/spawn/002 — Creating a single-agent card while a Mode tab is active switches the active tab back to the Dashboard with the new card selected and focused (PRD #154).
- **Layer:** L1 (in-process — open a REAL Mode tab via `TabManager::open_mode_tab`, then dispatch the real plain-card `Action::SpawnPane` through `dispatch_action` against a recording `OpenTabPC`; no daemon, no PTY).
- **Agent:** none (mock `PaneController`).
- **Asserts:** with the mode tab active, dispatching the no-mode/no-orchestration `SpawnPane` leaves `tab_manager.active_index() == 0` (the Dashboard), sets `ui.selected_index` to the new card's index, and focuses the new card pane — same "a card always lands on the Dashboard" rule as the orchestration case.
- **Does not assert:** mode-tab geometry / side-pane layout (covered by `tabs/mode/001`); the spawned agent's command behavior.
- **Platform coverage:** mac+linux+windows.

##### tabs/spawn/003 — Creating a single-agent card while already on the Dashboard leaves the Dashboard active with the new card selected and focused (no-regression guard, PRD #154).
- **Layer:** L1 (in-process — dispatch the real plain-card `Action::SpawnPane` through `dispatch_action` against a recording `OpenTabPC` with only the Dashboard tab present).
- **Agent:** none (mock `PaneController`).
- **Asserts:** with the Dashboard already active, dispatching the plain-card `SpawnPane` keeps `tab_manager.active_index() == 0`, sets `ui.selected_index` to the new card's index, and focuses the new card pane. Bounds the `tabs/spawn/001`/`002` switch-to-Dashboard fix so it never moves the active tab off the Dashboard in the common case (Ctrl+N from the Dashboard).
- **Does not assert:** the non-Dashboard launch paths (covered by `tabs/spawn/001`/`002`).
- **Platform coverage:** mac+linux+windows.

##### tabs/spawn/004 — Creating a single-agent card from a Mode tab captures that tab's focused side pane, so it is restored when the user returns to it (PRD #154 follow-up).
- **Layer:** L1 (in-process — open a REAL Mode tab via `TabManager::open_mode_tab`, focus a side pane, dispatch the real plain-card `Action::SpawnPane` through `dispatch_action`, then `switch_to` the Mode tab and `restore_focus_on_switch_in` against a focus-echoing mock; no daemon, no PTY).
- **Agent:** none (mock `PaneController` that, unlike `OpenTabPC`, reports the last `focus_pane` target back through `focused_pane_id()` so the switch-out capture has a live focus to read).
- **Asserts:** after focusing side pane #2 on a Mode tab and creating a single-agent card (which switches to the Dashboard), returning to the Mode tab restores that exact side pane via `focus_pane`. Pins that the plain-card spawn calls `capture_focus_on_switch_out()` before leaving the Mode tab; without it the Mode tab's `focused_pane_id` is never captured and restore falls back to the agent pane (`agent-m`), losing the user's prior focus. (Mode is the genuine regression surface: `sync_and_derive_selection` returns `None` for Mode tabs and never refreshes `focused_pane_id`, unlike the Orchestration branch whose per-frame derive keeps `focused_role_pane_id` fresh regardless of the capture.)
- **Does not assert:** the Orchestration-tab variant (masked by the per-frame `focused_role_pane_id` derive — not a faithful regression surface); the new card's own selection/focus on the Dashboard (covered by `tabs/spawn/002`).
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

##### hooks/delivery/007 — A hook event teaches the daemon an agent's type, so `list_agents` reports it on a fresh reconnect instead of "No agent".
- **Layer:** L2.
- **Agent:** none (synthetic — `StartAgent` over the daemon protocol with a shell command whose `from_command` type is `None`, then a JSON `SessionStart` written directly to the per-test hook socket).
- **Asserts:** an agent started with no inferable type registers with `agent_type == None`; after a `SessionStart` hook carrying `agent_type = claude_code` for that pane's id, a subsequent `ListAgents` (the same call `hydrate_from_daemon` issues on reconnect) reports `agent_type == ClaudeCode`.
- **Does not assert:** the rendered card label (the `AgentRecord`→placeholder→render mapping is covered by `rehydration` + L1 dashboard tests); the live-stream upgrade path while a TUI is already attached.
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

##### lifecycle/handshake/002 — Build-version mismatch with NO running agents restarts the daemon silently and proceeds into the dashboard (PRD #161 Part A).
- **Layer:** L2.
- **Agent:** none (an older external daemon at `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` is reused by a newer TUI to simulate skew).
- **Asserts:** with no agents running, no prompt is shown and no keypress is sent — the dashboard's empty state (`No active sessions`) appears, and the original (older) daemon process exits (the silent restart terminated it; a fresh daemon was lazy-spawned at the new build).
- **Does not assert:** the new daemon's exact build id (covered by the protocol round-trip tests).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/003 — Build-version mismatch with live agents in a TTY renders a consent prompt that names the live agents and states restarting stops them (PRD #161 Part A / M1.1).
- **Layer:** L2.
- **Agent:** one synthetic `sleep`-style agent with a distinctive display name, started over the daemon's attach socket before the TUI attaches.
- **Asserts:** the rendered prompt surfaces the live agent's **display name** (from the handshake reply's `running_agents.names`) together with the stop/restart intent.
- **Does not assert:** exact prompt wording (loose substring match on the agent name + stop/restart intent); the agent's generated id.
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/004 — Build-version mismatch with live agents on a non-TTY (mandatory-restart path) exits non-zero with a stderr recovery hint and no prompt (PRD #161 Part A).
- **Layer:** L2.
- **Agent:** one synthetic `sleep`-style agent (the binary is run directly with stdout redirected to a pipe, so `is_terminal()` is false).
- **Asserts:** exit code is non-zero; stderr carries a clear daemon recovery hint (mentions the daemon and stop/restart) and no prompt is rendered.
- **Does not assert:** exact stderr wording (pinned in lib pure-data tests); the no-agents non-TTY path (which silently restarts).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/005 — Build-version mismatch with live agents in a TTY: a single consent keystroke restarts the daemon (agents stopped) and the dashboard appears (PRD #161 Part A — replaces #103's two-`S` double-confirm).
- **Layer:** L2.
- **Agent:** one synthetic `sleep`-style agent.
- **Asserts:** after the prompt appears, a single `s` consent restarts the daemon — the original daemon process exits and the fresh (now empty) dashboard's `No active sessions` appears.
- **Does not assert:** exact prompt wording; the recovered daemon's build id.
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/006 — Build-version mismatch with live agents in a TTY: declining keeps the EXISTING daemon and lands in a working dashboard with the agents still reachable (PRD #161 D4 never-strand).
- **Layer:** L2.
- **Agent:** one synthetic `sleep`-style agent with a distinctive display name.
- **Asserts:** after the prompt appears, pressing `Esc` does NOT exit — a working dashboard appears against the still-running older daemon (the session is listed), the original daemon process is still alive, and the live agent remains reachable on it (never-strand). This is the key change from #103, where declining exited.
- **Does not assert:** the other decline keystrokes individually (`q` / `Ctrl+C` / `Ctrl+D` — covered by the same decline path); exact prompt wording.
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/007 — Build-version mismatch with a live agent where the daemon OMITS `running_agents` (a pre-#161 daemon predating M1.1): the handshake falls back to `list_agents()` and shows the consent prompt instead of silently restarting over the unseen agent (PRD #161 FIX 1 / D2 / D4 never-strand).
- **Layer:** L2.
- **Agent:** one synthetic `sleep`-style agent, started over the daemon's attach socket; the daemon runs with `DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS` so its `Hello` reply leaves `running_agents = None`, simulating a daemon that predates the M1.1 summary field.
- **Asserts:** the agents-PRESENT consent prompt appears (the TUI did NOT silently restart into the dashboard) — proving the handshake fell back to `list_agents()` rather than treating the absent field as "no agents" and SIGTERM'ing the live agent unseen; then pressing `Esc` declines and a working dashboard appears against the still-running old daemon with the agent still reachable (never-strand).
- **Does not assert:** that the prompt names the agent by its *display* name specifically (loose match — with `running_agents` omitted the label comes from `list_agents()`, so the display name OR a non-zero "(N agent(s) running)" header is accepted); exact prompt wording.
- **Platform coverage:** mac+linux.

#### lifecycle/login-path

##### lifecycle/login-path/001 — A dashboard new-pane whose command is a bare binary living only in the user's login-shell PATH spawns successfully when the daemon was launched without that dir on PATH (PRD #170 M1.3).
- **Layer:** L2 (real `dot-agent-deck` binary in a PTY; the deck lazy-spawns its daemon, which inherits the deck's env).
- **Agent:** none (a synthetic stub binary placed only in a temp dir that is NOT on the inherited PATH; the deck's `$SHELL` is a fake login shell whose `-lc` output adds that dir to PATH, mirroring how `~/.profile` adds `~/.local/bin`). `default_command` is set to the bare stub so the new-pane form pre-fills it.
- **Asserts:** opening the new-pane form (Ctrl+n → confirm dir → Submit) with the bare stub as the command spawns it successfully — the stub writes an on-disk marker that appears within the wait window. RED today: nothing captures the login-shell PATH, so the daemon's PATH lacks the stub dir, the bare command is not found, the spawn fails, and the marker never appears.
- **Does not assert:** the exact spawn-failure error text in the pane; the non-PATH login environment (out of scope per PRD #170).
- **Platform coverage:** mac+linux.

##### lifecycle/login-path/002 — A scheduled-task fire whose command is a bare binary living only in the user's login-shell PATH spawns successfully when the daemon was launched without that dir on PATH (PRD #170 M1.3).
- **Layer:** L2 (headless `dot-agent-deck daemon serve` driven via the `RunNow` control message — no PTY/grid, same shape as `scheduler/spawn/*`).
- **Agent:** none (a synthetic stub binary placed only in a temp dir absent from the daemon's PATH; the daemon's `$SHELL` is a fake login shell whose `-lc` output adds that dir to PATH). The scheduled task's `command` is the bare stub.
- **Asserts:** firing the task via `RunNow` spawns the bare stub successfully — the stub writes an on-disk marker that appears within the wait window. RED today: with no login-shell PATH capture the daemon's PATH lacks the stub dir, the bare command is not found, and the marker never appears.
- **Does not assert:** prompt delivery to the spawned agent (covered by `scheduler/spawn/004`); the orchestration-vs-card branch (covered by `scheduler/spawn/002`).
- **Platform coverage:** mac+linux.

##### lifecycle/login-path/003 — The schedule-authoring helper's bare authoring command (living only in the user's login-shell PATH) resolves and spawns when the daemon was launched without that dir on PATH (PRD #170 M1.3 + M2.1, the originally-motivating bug path).
- **Layer:** L2 (real `dot-agent-deck` binary in a PTY; the deck lazy-spawns its daemon, which inherits the deck's env). Reuses the `login_path_fixture` mechanics (stripped PATH + fake login shell) from `lifecycle/login-path/001`/`002` and the unified dir-picker + mode-locked form Edit flow from `scheduler/manager/002`.
- **Agent:** none (a synthetic stub binary placed only in a temp dir absent from the inherited PATH; the deck's `$SHELL` is a fake login shell whose `-lc` output adds that dir to PATH). `default_command` is the bare stub, so the mode-locked form's pre-filled Command defaults to it. A fixture `schedules.toml` supplies one task to edit (its own `cat` run command is irrelevant — the authoring command comes from `default_command`).
- **Asserts:** opening the Scheduled-Tasks manager (`S`), pressing `e` to edit the auto-selected row opens the directory picker (` Select Directory `); confirming the dir with Space opens the mode-locked ` Edit Schedule ` form (Command pre-filled with the bare authoring command); submitting via `[Submit]` spawns it through the daemon spawn primitive, and the bare command resolves under the daemon's login-shell-enriched PATH — the stub writes an on-disk marker that appears within the wait window. GREEN once M1.3 + M2.1 + the unified flow are merged: pins PRD #170's third spawn path (the schedule-authoring helper), which routes through the same daemon spawn primitive as `001`/`002` plus the configurable-command change of `scheduler/manager/002`.
- **Does not assert:** the authoring seed/prompt delivery to the spawned agent (covered by `scheduler/manager/002`); the dir-picker/form interaction details (covered by `scheduler/form/001`–`003`); the non-PATH login environment (out of scope per PRD #170).
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

##### render/layout/004 — A wrapped button bar costs the dashboard exactly one extra row of its height budget (PRD #144).
- **Layer:** L1 (in-process `TestBackend` via `render_button_bar_with_bindings_to_buffer`; no PTY, no subprocess).
- **Agent:** none (renders the full global + dashboard context bar into a tall area at two widths).
- **Asserts:** at the 120-col reference width the full button set (~133 cells) does not fit one row, so the bar wraps to EXACTLY two rendered rows — meaning the dashboard/pane region above must cede exactly that one extra row (the PRD #144 height-budget contract that keeps a 2-row bar from overlapping / clipping the cards); at a roomy 200-col width the same set fits one row, so the bar occupies exactly one row and the dashboard cedes nothing extra. Complements `mouse/buttonbar/006` (which pins the wrapped bar's label content) by pinning its height.
- **Does not assert:** the card/pane rects themselves (no public full-frame layout seam at L1 — the post-transition card cleanliness is guarded at L2 by `render/layout/001`–`003`); which button lands on which row; the exact column widths.
- **Platform coverage:** mac+linux+windows.

##### render/layout/005 — The new-pane form modal renders without panicking on a wide-but-very-short terminal (PRD #144 bounds-safety guard).
- **Layer:** L1 (in-process `TestBackend` via `render_new_pane_form_to_buffer`; no PTY, no subprocess).
- **Agent:** none (renders the new-pane form with two mode options into an 80×3 buffer).
- **Asserts:** rendering the content-sized new-pane form modal at a wide-but-very-short 80×3 terminal — where the modal is clamped to ~2 rows, far fewer than the form's reserved field rows — completes WITHOUT panicking, and returns a buffer of exactly the requested size so every overlay cell (mode chips, `[Submit]`/`[Cancel]` row, cursor) stayed within the clamped modal/buffer bounds instead of being placed by an absolute line index that runs past the buffer bottom. A TUI must not panic on a small-but-valid terminal.
- **Does not assert:** the exact rows the overlays land on; which overlays are skipped when they don't fit; the modal's content/labels at this degenerate size; behaviour at roomy sizes (covered by `mouse/form/001`).
- **Platform coverage:** mac+linux+windows.

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

##### orchestration/delegate/005 — A Pi-identity orchestrator's `delegate` routes into the worker pane (the synthetic-agent harness proves the delegate contract holds for a Pi identity) (PRD #201 M1.3).
- **Layer:** L1/fast (in-process — the daemon's real `handle_delegate` against a `cat`-stub worker pane; mirrors the fast-tier precedent `delegate_prompt_injection`, no daemon socket, no LLM).
- **Agent:** none (synthetic — the harness at `AgentType::Pi` identity is the orchestrator; the `coder` worker is a `cat` stub whose PTY echoes injected bytes).
- **Asserts:** with a Pi orchestrator (the `start = true` role) and a `coder` worker registered in the same orchestration, calling the harness's `delegate --to coder` routes the single-line task pointer into the worker pane's PTY. Additive Pi coverage of the `orchestration/delegate/001` contract; expected green-on-write because routing keys on pane role, not agent type.
- **Does not assert:** the worker task-file footer / single-line-prompt shape (covered by `delegate_prompt_injection`); the real-agent response (no LLM).
- **Platform coverage:** mac+linux.

##### orchestration/delegate/006 — A Pi-identity WORKER calling `delegate` is rejected by the pane-role guard; no task is delivered (PRD #201 M1.3).
- **Layer:** L1/fast (in-process — the daemon's real `handle_delegate` against a `cat`-stub worker pane; no daemon socket, no LLM).
- **Agent:** none (synthetic — the harness at `AgentType::Pi` identity is a non-orchestrator worker; a `coder` worker `cat` stub shares the orchestration so an orchestrator's delegate WOULD deliver).
- **Asserts:** a Pi worker (registered in `pane_role_map` but deliberately absent from `orchestrator_pane_ids`) calling the harness's `delegate --to coder` is rejected — the `coder` stub's PTY never receives the task pointer within a bounded grace window (rejection is a synchronous early return before any dispatch task spawns). Additive Pi coverage of the `orchestration/delegate/004` guard; expected green-on-write.
- **Does not assert:** the orchestrator pane's error-line rendering (L2 `orchestration/delegate/004`); the daemon-side log entry.
- **Platform coverage:** mac+linux.

#### orchestration/identity

##### orchestration/identity/001 — Opening an orchestration whose form/display name (worktree dir basename) differs from the TOML config orchestration name stamps the CANONICAL config name as the daemon IDENTITY, not the basename (PRD #107 regression).
- **Layer:** L1 (in-process — dispatch the real `Action::SpawnPane` through `dispatch_action` against a recording `PaneController`; no daemon, no PTY).
- **Agent:** none (stub role commands; orchestration_config carries `name = "dot-agent-deck"` with a `coder` role at `clear = true`).
- **Asserts:** when the new-pane form's Name field defaults to the worktree basename (`dot-agent-deck-prd-113-foo`) while the config name is `dot-agent-deck`, every role pane's `TabMembership::Orchestration.name` (the IDENTITY the daemon's `lookup_orchestration_role` compares) equals the canonical config name `dot-agent-deck` — so the role resolves and `clear = true` respawn fires — while the tab TITLE (`Tab::Orchestration.name`) still shows the basename. Pre-fix the PRD #107 SpawnPane override copies the basename into `orch_config.name`, so the identity is the basename and the lookup misses.
- **Does not assert:** the daemon-side `pane_orchestration_map` recording or the live delegate respawn (L2 path); the on-disk config reload inside `lookup_orchestration_role`.
- **Platform coverage:** mac+linux+windows.

#### orchestration/layout

##### orchestration/layout/001 — Seven decks fit the single-column orchestration card area without scrolling (PRD #147).
- **Layer:** L1 (ratatui `TestBackend`, buffer inspection + capacity math via the public `rendered_height` seam).
- **Agent:** none.
- **Asserts:** in the ~34%-width single-column orchestration card area at a typical ~48-row card height, the renderer's `visible_rows = available / card_height` fits all 7 decks with no scrolling and the 7th deck actually renders in the visible slice; a much larger deck count (20) still engages scrolling, so right-sizing the card height does not remove the scroll fallback.
- **Does not assert:** the full orchestration-tab frame (tab bar, side panes, stats bar); the `ORCHESTRATION_LEFT_PERCENT` width split or `grid_columns` thresholds (out of scope per PRD #147).
- **Platform coverage:** mac+linux+windows.

### Session restore

#### session/restore

##### session/restore/001 — No-flag startup auto-restores dashboard panes from the saved session (PRD #89 Phase 2).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path).
- **Agent:** none (a saved `session.toml` with two panes running `sleep 600`; daemon is freshly spawned and empty).
- **Asserts:** launching with NO `--continue` flag against an empty daemon restores both saved panes as dashboard cards, with their saved display names. (Restore is unconditional now — the old `--continue` gate is gone.)
- **Does not assert:** the agents' inner state (not preserved per docs); the daemon-vs-snapshot precedence (deferred to Phase 2 M2.2).
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

##### session/restore/005 — Daemon-with-agents wins over the disk snapshot; snapshot restore is skipped (PRD #89 Phase 2 M2.2).
- **Layer:** pure-data (in-crate integration test on `ui::should_apply_snapshot` over `AppState.managed_pane_ids`; no TUI harness, runs in the fast tier).
- **Agent:** none.
- **Asserts:** with no hydrated managed panes `should_apply_snapshot` returns `true` (daemon empty → apply the disk snapshot); after one or more hydrated `managed_pane_id`s are registered it returns `false` (daemon owns the workspace → skip the snapshot so panes are not double-restored). Pins the M2.2 precedence as a structural decision, not a flag.
- **Does not assert:** the end-to-end cross-deck PTY hydration path (would need a daemon pre-seeded with an agent that a fresh deck hydrates — a harness primitive not yet built); the snapshot-apply mechanics themselves (covered by `session/restore/001`).
- **Platform coverage:** mac+linux+windows.

##### session/restore/006 — Empty daemon + no snapshot + no flag lands on a clean empty dashboard (PRD #89 Phase 2).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path with no file staged).
- **Agent:** none.
- **Asserts:** with both restore sources empty (fresh empty daemon, no snapshot on disk) and no `--continue`, the deck lands on the "No active sessions" dashboard with no restore warning and remains interactive (Ctrl+N opens the new-pane directory picker). Locks the post-Phase-2 invariant that unconditional restore still falls through cleanly when there is nothing to restore.
- **Does not assert:** the daemon-with-agents-wins precedence (deferred to Phase 2 M2.2); the snapshot-restore path (covered by `session/restore/001`).
- **Platform coverage:** mac+linux.

##### session/restore/007 — A warm daemon carrying an orchestration hydrates the orchestrator + role panes in their saved order (PRD #89 Phase 2b M2b.1).
- **Layer:** in-process (real in-process attach daemon over a Unix socket; `EmbeddedPaneController::hydrate_from_daemon`; no real binary, no PTY drive). Runs in the fast tier.
- **Agent:** none (each role agent runs `sh -c 'sleep 30'`; no LLM).
- **Asserts:** spawning three orchestration role agents (orchestrator + coder + reviewer), each tagged with its `TabMembership::Orchestration` `role_index` / `role_name` / `is_start_role`, then hydrating a fresh controller from the warm daemon reproduces every role as a pane; placing each hydrated pane at its `role_index` yields the panes in their saved display order; and the start (orchestrator) role — the `start_role_index` cursor — is recoverable from `is_start_role`. Regression guard that warm-daemon orchestration hydration (PRD #76 M2.12 + #111) survives detach/reattach so M2b.3's snapshot fallback is only needed when the daemon is empty.
- **Does not assert:** the daemon-empty snapshot-fallback rebuild (`session/restore/008`); the orchestrator-prompt replay (intentionally NOT replayed on warm reconnect — `src/tab.rs` design decision 3); the full `OrchestrationConfig` re-resolution (the partition + `resolve_orch_config_for_hydration` path, exercised elsewhere).
- **Platform coverage:** mac+linux (Unix-only; `#![cfg(unix)]`).

##### session/restore/008 — A daemon-empty launch with an orchestration snapshot rebuilds the orchestration tab and replays the orchestrator prompt (PRD #89 Phase 2b M2b.3).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (the orchestration's `coder`/`reviewer` roles run `sleep 600`; the `orchestrator` role runs a recorder shell script that self-posts `SessionStart` and appends its stdin to an absolute `record-orchestrator.log` — no LLM tokens).
- **Asserts:** with a hand-staged `session.toml` whose single pane carries a `[panes.orchestration]` block (`config_name`/`project_path` pointing at a test-owned orchestration config, `orchestrator_prompt = "Build the feature end to end"`, `start_role_index = 0`) and an empty daemon, launching with NO `--continue` REBUILDS the orchestration tab: the `coder` and `reviewer` role panes appear as deck cards in their saved display order, and — unlike warm hydration (`session/restore/007`) — the saved `orchestrator_prompt` is replayed to the start (orchestrator) role and recorded (echo-immune), which also proves the start role was identified from `start_role_index`.
- **Does not assert:** the warm-daemon hydration path (`session/restore/007`); the on-disk capture that produces the snapshot (`session/save/004`); the config-drift fallback (`session/restore/009`); the exact role-card styling / focus border.
- **Platform coverage:** mac+linux.

##### session/restore/009 — An orchestration snapshot whose config no longer resolves falls back to a plain dashboard pane with a `session_warnings` message naming the missing orchestration (PRD #89 Phase 2b M2b.3 drift).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (the fallback pane runs `sleep 600`; no LLM).
- **Asserts:** with a hand-staged `session.toml` whose `[panes.orchestration]` block references `config_name = "tdd-cycle"` while the project config at `project_path` defines only a renamed `renamed-orch` (a re-resolution drift), launching against an empty daemon with no flag restores the saved pane as a PLAIN dashboard card (its saved name `orchestrator`, with no `coder`/`reviewer` role panes — never a half-broken tab) AND surfaces a clear `session_warnings` message naming the missing orchestration (`tdd-cycle`), flushed to stderr on detach-quit. Mirrors the mode-tab drift fallback (`session/restore/003`, PRD #69 Path D/E).
- **Does not assert:** the exact warning wording (only that it names the missing orchestration); the successful rebuild path (`session/restore/008`); which other panes survive when multiple are staged (only one is here).
- **Platform coverage:** mac+linux.

##### session/restore/010 — A snapshot re-resolving to a zero-role orchestration falls back to a plain dashboard pane with a warning, never panicking at startup (PRD #89 review-fix F2).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (the fallback pane runs `sleep 600`; no LLM).
- **Asserts:** with a project config that still names `tdd-cycle` but whittled to an EXPLICIT empty role set (`roles = []`, which `load_project_config` accepts since it runs no `config_validation`) and a hand-staged snapshot whose saved role set is also empty (so the name+order drift guard passes — `[] == []`) with a `start_role_index` of 0 that is out of range, launching against an empty daemon with no flag does NOT panic/crash-loop: the saved pane restores as a PLAIN dashboard card (`orchestrator`) and a `session_warnings` message naming the orchestration (`tdd-cycle`) is flushed to stderr on a clean detach-quit. Pins that an empty/no-start-role re-resolution is treated as drift, never indexed unguarded at the start cursor.
- **Does not assert:** the exact warning wording (only that it names the orchestration); the successful rebuild path (`session/restore/008`); the non-empty role-set drift fallback (`session/restore/009`).
- **Platform coverage:** mac+linux.

##### session/restore/011 — A saved `start_role_index` that differs from the config default is honored on restore: the orchestrator prompt lands on the role at the saved index (PRD #89 review-fix F3).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (both roles run a recorder shell script that self-posts `SessionStart` and appends its stdin to an absolute `record-<role>.log` — no LLM tokens).
- **Asserts:** with a `tdd-cycle` config whose default start role is `orchestrator` (index 0, `start = true`) and a recorder on BOTH roles, a hand-staged snapshot saving `start_role_index = 1` (`coder`) makes the replayed `orchestrator_prompt` land on and be recorded by the role at the SAVED index (`coder`, index 1) — and NOT by the config-default start role (`orchestrator`, index 0). Pins that restore reads `snap.start_role_index` rather than recomputing the start cursor from the live config's `start` flag.
- **Does not assert:** the drift/bounds handling when the saved index is out of range (`session/restore/010`); `started_role_indices` replay (captured but has no reader); the exact role-card styling / focus border.
- **Platform coverage:** mac+linux.

##### session/restore/012 — A snapshot whose `project_path` diverges from the saved pane `dir` does not auto-run the config planted at `project_path` (PRD #89 review-fix F1).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (roles run `sleep 600`; no LLM).
- **Asserts:** with the saved pane `dir` pointing at a legitimate working dir (no orchestration config) while the `[panes.orchestration]` `project_path` points at a SEPARATE planted dir whose config defines a uniquely-named `phantom-reviewer` role, launching against an empty daemon with no flag does NOT execute the planted config — `phantom-reviewer` never materializes as a deck card — while the saved pane still restores as a PLAIN card (`orchestrator`). Pins that the un-cross-checked `project_path` cannot auto-run a config from an unexpected directory (capture always writes `project_path == saved_pane.dir`, so divergence only arises via tampering).
- **Does not assert:** which fix shape the coder chooses (drift fallback vs. re-resolving from `saved_pane.dir`) — only that the divergent config is not executed; path canonicalization edge cases (symlinks, `..`).
- **Platform coverage:** mac+linux.

##### session/restore/013 — A custom orchestration tab `display_title` saved in the snapshot is preserved on restore (PRD #89 review-fix F4, RED-pending-schema).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (roles run `sleep 600`; no LLM).
- **Asserts:** with a hand-staged snapshot carrying a custom `display_title` (`MYDECKTITLE`) distinct from the canonical config name, the daemon-empty rebuild shows the user's saved title in the tab bar, not the canonical `tdd-cycle` config/cwd name. RED-pending-schema: `OrchestrationSnapshot` has no `display_title` field yet (the staged key parses but is dropped on load, since the struct sets no `deny_unknown_fields`) and restore passes `None` to `open_orchestration_tab`, so the tab comes back titled `tdd-cycle`; goes GREEN once the coder adds the field + capture + restore threading.
- **Does not assert:** the live-path title plumbing (already covered by the new-pane orchestration flow); the serde round-trip of the new field in isolation (a unit test the coder adds with the field).
- **Platform coverage:** mac+linux.

### Live session status on reconnect (PRD #162)

These entries cover PRD #162: on TUI reconnect the daemon's `ListAgents` must attach the live, event-derived session state (a `SessionSnapshot` on each `AgentRecord`) so reconnected cards show real status instead of `Idle`/"No agent". The data already exists in `AppState.sessions` (built by `apply_event`, unchanged); this PRD only exposes it. The wire field `live: Option<SessionSnapshot>` is additive/optional — no `PROTOCOL_VERSION` bump.

#### session/live

##### session/live/001 — `SessionSnapshot` serde round-trips every `SessionStatus` and an older `AgentRecord` without the field decodes to `live == None` (PRD #162 M1.1).
- **Layer:** pure-data (serde round-trip; no daemon/TUI harness; runs in the fast tier).
- **Agent:** none.
- **Asserts:** a `SessionSnapshot` carrying each `SessionStatus` variant (Idle/Working/Thinking/WaitingForInput/Compacting/Error) round-trips through JSON with the status (and agent_type/active_tool/tool_count/prompts) preserved; an `AgentRecord` carrying `live = Some(snapshot)` round-trips with the snapshot intact; and a hand-crafted older-daemon `AgentRecord` JSON with no `live` key decodes via `#[serde(default)]` to `live == None` (back-compat, no protocol bump).
- **Does not assert:** the `ListAgents` join (session/live/002); newest-wins tie-break (session/live/003); the TUI-side seeding of the hydrated session (Phase 2).
- **Platform coverage:** mac+linux+windows.

##### session/live/002 — The `ListAgents` handler attaches the live event-derived snapshot; the dummy-state path yields `None` (PRD #162 M1.2).
- **Layer:** in-crate integration (in-process attach daemon over a Unix socket; fast tier; spawns a `sleep` PTY only to populate the registry record, does not drive vt100).
- **Agent:** none.
- **Asserts:** with a registry agent whose spawn-time `agent_type` is `None` and a live `AppState` session (same `agent_id` + `pane_id`) driven via `apply_event` to `Working` with an active tool, `tool_count > 0`, an event-derived `agent_type` (ClaudeCode) and a first prompt, the `ListAgents` response carries `AgentRecord.live = Some` with that status, the event-derived `agent_type` (even though the registry record's spawn-time `agent_type` is `None`), the active tool name, the tool count, and the first/last prompt. The empty dummy-state `serve_attach` path returns the same record with `live == None` — no harness regression and the older-daemon fallback shape.
- **Does not assert:** the pure serde shape (session/live/001); newest-wins (session/live/003); the TUI-side seeding (Phase 2).
- **Platform coverage:** mac+linux.

##### session/live/003 — When two sessions map to the same agent, the join attaches the newest-`last_activity` snapshot (PRD #162 M1.2 newest-wins).
- **Layer:** in-crate integration (in-process attach daemon over a Unix socket; fast tier; spawns a `sleep` PTY only to populate the registry record, does not drive vt100).
- **Agent:** none.
- **Asserts:** with two hand-built `SessionState`s in `AppState.sessions` that both map to the same agent (same `agent_id` + `pane_id`, e.g. a `/clear` restart leaving a stale entry) but different `last_activity` and distinguishing status/prompt, the `ListAgents` join attaches the snapshot from the entry with the most-recent `last_activity` (the live session), not the dead predecessor.
- **Does not assert:** the pure serde shape (session/live/001); the populated-vs-dummy contrast (session/live/002); the TUI-side seeding (Phase 2).
- **Platform coverage:** mac+linux.

##### session/live/004 — Hydrating a fresh controller seeds the reconnected card from the daemon's live snapshot (status/agent_type/active_tool/tool_count/prompts), and falls back to the bare placeholder when no snapshot is present (PRD #162 M2.1/M2.2).
- **Layer:** in-process (real in-process attach daemon over a Unix socket; `EmbeddedPaneController::hydrate_from_daemon`; spawns two `sleep` PTYs only to populate the registry, does not drive vt100). Runs in the fast tier.
- **Agent:** none.
- **Asserts:** a warm daemon carries agent A (spawn-time `agent_type = None`, the "No agent" case) driven via `apply_event` to a live `Working` session with an active `Edit` tool, `tool_count > 0`, an event-derived `ClaudeCode` type and a first prompt, plus agent B (spawn-time `OpenCode`) with NO live session. Hydrating a fresh controller threads the live `SessionSnapshot` through `HydratedPane.live` (`Some` for A, `None` for B); seeding each hydrated session via `AppState::seed_hydrated_session` — exactly as the `ui.rs` hydration loop does — makes agent A's card carry the snapshot's `status` (Working, not Idle) / `agent_type` (ClaudeCode, overriding the `None` spawn-time value, not "No agent") / `active_tool` / `tool_count` / `first_prompts` / `last_user_prompt`, with the PRD #110 `agent_id` minted on the card; agent B's snapshot-absent card falls back to today's bare placeholder (Idle, spawn-time `OpenCode`, no active tool). Each pane seeds exactly one card (no duplicate).
- **Does not assert:** the pure serde shape (session/live/001); the `ListAgents` join in isolation (session/live/002); newest-wins (session/live/003); the post-reconnect remap (session/live/005); the rendered-grid reconnect against a real daemon (session/live/006).
- **Platform coverage:** mac+linux.

##### session/live/005 — A post-reconnect `SessionStart` from the same agent remaps onto the snapshot-seeded card instead of spawning a duplicate (PRD #162 M2.2, PRD #110 property preserved).
- **Layer:** pure-state (in-process `AppState`; `seed_hydrated_session` + `apply_event`; no daemon/TUI harness). Runs in the fast tier.
- **Agent:** none.
- **Asserts:** after `AppState::seed_hydrated_session` seeds a card from a live `SessionSnapshot` (Working/ClaudeCode/active tool/prompts) with the PRD #110 `agent_id` minted on it, a subsequent `SessionStart` event carrying the SAME `pane_id` + `agent_id` but a distinct `session_id` remaps onto the hydrated card — exactly one session/pane survives for that agent (no duplicate) and the minted `agent_id` is preserved through the remap.
- **Does not assert:** the snapshot-seeding of the card's fields (session/live/004); the daemon-side join (session/live/002, session/live/003); the rendered-grid reconnect (session/live/006); the clear=true respawn (different `agent_id`) duplicate-retire path (PRD #110 tests).
- **Platform coverage:** mac+linux+windows.

##### session/live/006 — A fresh TUI reconnecting to a real daemon renders the live `Working` status on the rebuilt card immediately, not the `Idle`/"No agent" placeholder (PRD #162 M2.1/M2.2 end-to-end).
- **Layer:** L2 (real-binary PTY; a shared `dot-agent-deck daemon serve` driven over its hook + attach sockets, then a fresh real-binary TUI launched against the same daemon's sockets; `#[cfg(feature = "e2e")]`).
- **Agent:** none (the agent is a `sh -c 'sleep 600'` stub; the live status is taught via synthetic Claude Code hooks — no LLM tokens).
- **Asserts:** a daemon-owned agent (spawn-time `agent_type = None`, pane `pane-recon`, display name `recon-live-77`) is driven to a live `Working` session with an active `Read` tool by writing `session_start` + `tool_start` hooks (carrying the registry agent id so the `ListAgents` snapshot join matches) — with NO TUI attached. A FRESH TUI then launched against the same daemon, writing no further hook, rebuilds the dashboard card showing the live `Working` status and the agent's display name immediately on reconnect, and does not render the `No agent` placeholder for that live agent.
- **Does not assert:** a literal first-TUI detach cycle (the daemon owns the live state regardless of whether a TUI was ever attached); the in-process seeding seam (session/live/004); the active-tool tally/label beyond the status badge; the daemon-side join/serde (session/live/001–003).
- **Platform coverage:** mac+linux.

##### session/live/007 — `DaemonClient::list_agents` scrubs and clamps a hostile `AgentRecord.live` at the wire boundary so a malformed daemon can't corrupt the rebuilt card (PRD #162 review-fix, parallels embed/attach/005).
- **Layer:** in-crate integration (a hand-rolled mock attach daemon over a Unix socket advertises one hostile `AgentRecord`; the real `DaemonClient::list_agents` boundary sanitizer runs; fast tier; no PTY/vt100).
- **Agent:** none (the mock daemon hand-crafts the hostile `AttachResponse`).
- **Asserts:** a daemon advertises an `AgentRecord.live` whose `last_user_prompt`, every `first_prompts` entry, and `active_tool.name` / `.detail` carry ANSI escapes, NUL bytes, and other ASCII control chars AND are over-long (~100 KiB each), and whose `first_prompts` is oversized (6 entries — double the `MAX_FIRST_PROMPTS` cap of 3). `list_agents` returns the record with its live snapshot PRESERVED (the agent is real) but SCRUBBED — no byte `< 0x20` or `== 0x7f` survives in `last_user_prompt`, any `first_prompts` entry, or `active_tool.name` / `.detail` — and CLAMPED — every one of `last_user_prompt`, `active_tool.name`, `active_tool.detail`, and each `first_prompts` entry is length-bounded to <= 65536 bytes (not passed through verbatim), and `first_prompts` is cut to at most `MAX_FIRST_PROMPTS` (3) entries.
- **Does not assert:** the daemon-side join/serde (session/live/001–003); the seeding of the card's fields (session/live/004); the `agent_type` precedence fallback (session/live/008); the `tab_membership` scrub itself (embed/attach/005); the exact sanitized output beyond "no raw control bytes survive and the list is clamped".
- **Platform coverage:** mac+linux.

##### session/live/008 — An event-derived `AgentType::None` snapshot falls back to the spawn-time agent type on reconnect instead of seeding the card as "No agent" (PRD #162 review-fix).
- **Layer:** pure-state (in-process `AppState`; `SessionState::live_snapshot` + `AppState::seed_hydrated_session`; no daemon/TUI harness). Runs in the fast tier.
- **Agent:** none.
- **Asserts:** a live `SessionState` whose event-derived `agent_type` is `AgentType::None` (the agent emitted events but never identified itself) snapshots via `live_snapshot` to `agent_type == None` (Option::None, NOT `Some(AgentType::None)`), so when `seed_hydrated_session` seeds a reconnected card whose spawn-time `agent_type` is `Some(ClaudeCode)`, the snapshot does not shadow the spawn-time fallback and the card carries the REAL `ClaudeCode` type — not "No agent".
- **Does not assert:** the wire-boundary scrub/clamp (session/live/007); the full snapshot field seeding (session/live/004); the daemon-side newest-wins join (session/live/003); the post-reconnect remap (session/live/005).
- **Platform coverage:** mac+linux+windows.

##### session/live/009 — An unknown `SessionStatus` string on `AgentRecord.live.status` degrades gracefully instead of failing the whole record parse (PRD #162 Greptile review-fix, forward-compat).
- **Layer:** pure-data (serde decode of a hand-crafted wire JSON; no daemon/TUI harness; fast tier).
- **Agent:** none.
- **Asserts:** an `AgentRecord` wire JSON whose `live.status` is a string this build does not know (`"Hibernating"`) deserializes via `serde_json::from_str::<AgentRecord>` to `Ok` (NOT `Err`) and the record survives with its `id` / `pane_id_env` intact — a newer daemon's future status variant must not fail an older TUI's entire `AgentRecord` decode just because `live` is a present field. Mechanism-agnostic: does NOT pin whether the fix maps the unknown status to a catch-all variant (`live` stays `Some`) or drops `live` to `None`.
- **Does not assert:** which degrade mechanism is chosen (`#[serde(other)]` vs lenient `live -> None`); the older-shape back-compat (`live` absent -> `None`, session/live/001); the wire-boundary scrub/clamp (session/live/007).
- **Platform coverage:** mac+linux+windows.

### Session save (snapshot freshness, PRD #89 Phase 1)

These entries cover PRD #89 Phase 1: the saved-session snapshot must be kept continuously fresh — written on meaningful TUI state changes and on detach — not only at clean teardown/quit.

#### session/save

##### session/save/001 — A meaningful TUI state change (creating a new dashboard pane) writes a fresh saved-session snapshot to disk without quitting.
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path).
- **Agent:** none (the pane runs `sleep 600`; no LLM).
- **Asserts:** starting with no prior snapshot on disk, creating a new dashboard pane via the new-pane flow (Ctrl+N → dir-picker → form → submit) — and NOT quitting — causes a `session.toml` to be written that contains the newly created pane's command.
- **Does not assert:** the coalescing/debounce window (covered by `session/save/003`); restore-on-startup behavior (PRD #89 Phase 2).
- **Platform coverage:** mac+linux.

##### session/save/002 — Triggering a detach path (Ctrl+W close-pane) flushes a fresh snapshot reflecting the workspace, without quitting.
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path).
- **Agent:** none (panes run `sleep 600`; no LLM).
- **Asserts:** with two dashboard panes present and any prior snapshot removed, closing a pane with Ctrl+W writes a fresh `session.toml` that still reflects the (non-empty) workspace — proving the detach path flushes the snapshot mid-session, not only at clean quit.
- **Does not assert:** which specific pane survives the close; the coalescing/debounce window (`session/save/003`).
- **Platform coverage:** mac+linux.

##### session/save/003 — A burst of meaningful state changes coalesces to at most one or two snapshot writes, not one per change.
- **Layer:** pure-data (in-crate `#[cfg(test)]` unit test on `config::SnapshotCoalescer`; no TUI harness, synchronous clock).
- **Agent:** none.
- **Asserts:** driving the coalescer (750 ms-style interval) with 50 rapid `mark_dirty` notifications observed at one instant — each followed by the loop's `is_due`/`record_write` check — produces only the leading-edge write; a single trailing check after the interval flushes the rest, for ≤2 total writes (and ≥1), and nothing is due once flushed.
- **Does not assert:** the production interval value, real wall-clock timing, or that the on-disk file content is correct (covered by `session/save/001`–`002`).
- **Platform coverage:** mac+linux+windows.

##### session/save/004 — Opening an orchestration tab captures its orchestration metadata into the saved-session snapshot (PRD #89 Phase 2b M2b.3 capture).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path).
- **Agent:** none (the `orch-deck` fixture's `demo-orch` roles run `cat`; no LLM).
- **Asserts:** opening the fixture orchestration via the new-pane form (a Phase 1 M1.1 meaningful state change that flushes the coalesced snapshot) — and NOT quitting — writes a `session.toml` carrying a `[panes.orchestration]` block that records the resolved `config_name` (`demo-orch`), the roles (`orchestrator`, `worker`) in display order, and the `start_role_index` (`0`, the `start = true` orchestrator), so the daemon-empty restore path (`session/restore/008`) can rebuild the tab.
- **Does not assert:** the restore branch that consumes the metadata (`session/restore/008`–`009`); the serde round-trip of the schema in isolation (`config/saved-session/001`); the coalescing window (`session/save/003`).
- **Platform coverage:** mac+linux.

### Saved-session schema (orchestration metadata, PRD #89 Phase 2b)

This entry covers PRD #89 Phase 2b M2b.2: the saved-pane schema gains an `Option<OrchestrationSnapshot>` (role order, `start_role_index`, `orchestrator_prompt`, resolved config name + project path, `version`, and which roles were started) so the daemon-empty restore path can rebuild an orchestration tab. The field is `Option` + `#[serde(default)]` so old `session.toml` files still parse.

#### config/saved-session

##### config/saved-session/001 — An `OrchestrationSnapshot` on a saved pane round-trips through TOML, and a legacy snapshot without the field still parses (PRD #89 Phase 2b M2b.2).
- **Layer:** pure-data (in-crate `#[cfg(test)]` unit test on `config::SavedSession` / `SavedPane` / `OrchestrationSnapshot`; no TUI harness, no I/O).
- **Agent:** none.
- **Asserts:** (a) a `SavedSession` whose pane carries an `OrchestrationSnapshot` (version, role order in display order, `start_role_index`, `orchestrator_prompt`, `config_name`, `project_path`, `started_role_indices`) serializes to TOML and deserializes back with every field intact; (b) a legacy `session.toml` string with no `orchestration` key parses with `orchestration == None` — the `#[serde(default)]` forward-compat guarantee for snapshots written before the field existed.
- **Does not assert:** the snapshot-fallback restore branch that consumes the metadata (M2b.3 / `session/restore/008`–`009`); capture (populating the field when writing the snapshot); any TUI rendering.
- **Platform coverage:** mac+linux+windows.

### CLI surface (PRD #89 Phase 3)

#### cli/continue-removed

##### cli/continue-removed/001 — `--continue` is removed from the CLI surface and rejected on use (PRD #89 Phase 3).
- **Layer:** L2 (thin real-binary subprocess spawn; no PTY drive).
- **Agent:** none.
- **Asserts:** `dot-agent-deck --help` no longer advertises `--continue`, and `dot-agent-deck --continue` exits non-zero with a message that references the flag (guiding the user toward the now-default auto-restore). Since auto-restore is unconditional, the flag has no remaining purpose.
- **Does not assert:** the exact wording of the rejection message (clap's default unknown-argument text or a custom friendly message both satisfy it).
- **Platform coverage:** mac+linux.

### Fresh-start escape hatch (PRD #89 Phase 4)

These entries cover PRD #89 Phase 4: with auto-restore now the default, a user who wants to start clean has one obvious action — `dot-agent-deck snapshot clear` (M4.2) — because the snapshot is a single GLOBAL file. `dot-agent-deck remote remove <name>` (M4.1) is registry-only and intentionally does NOT touch the snapshot (decided Option 1); there is no per-deck saved state to clear.

#### session/snapshot

##### session/snapshot/001 — `dot-agent-deck snapshot clear` deletes the local saved-session snapshot (PRD #89 Phase 4 M4.2).
- **Layer:** L2 (thin real-binary subprocess spawn; no PTY drive; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path).
- **Agent:** none.
- **Asserts:** with a non-empty `session.toml` staged at the redirected path, running `dot-agent-deck snapshot clear` exits 0 and the snapshot file is gone afterward — the local fresh-start escape hatch. The command shape is a `snapshot` subcommand group with a `clear` action (decided; not `reset`/`--reset`).
- **Does not assert:** the subsequent no-flag startup landing on an empty dashboard (that follows from the deleted snapshot + `session/restore/006`); the exact stdout wording of the success message.
- **Platform coverage:** mac+linux.

##### session/snapshot/002 — `dot-agent-deck remote remove <name>` is registry-only and leaves the global snapshot intact (PRD #89 Phase 4 M4.1, Option 1).
- **Layer:** L2 (thin real-binary subprocess spawn; no PTY drive; `DOT_AGENT_DECK_SESSION` + `DOT_AGENT_DECK_REMOTES` redirected to test-owned paths).
- **Agent:** none.
- **Asserts:** with a remote deck `myhost` registered in the staged `remotes.toml` and a non-empty `session.toml` staged, running `dot-agent-deck remote remove myhost` exits 0 AND leaves the global snapshot intact — the file is still present afterward with byte-for-byte unchanged contents. The snapshot is a single GLOBAL file, so remove is registry-only (decided Option 1); there is no per-deck saved state to clear and `snapshot clear` (001) is the one fresh-start action.
- **Does not assert:** that the registry entry was removed (that is `remote remove`'s pre-existing behavior, exercised elsewhere); any per-deck keying of saved state (none exists — the snapshot is a single global file).
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

#### chain-smoke/pi

##### chain-smoke/pi/001 — A REAL `pi` orchestrator, driving a real model, loads the bundled extension, calls the native `delegate` tool, the daemon routes to a REAL `claude` worker that creates a uniquely-named sentinel + signals `work-done`, and the Pi pane's status is tracked via `agent-event` with NO hook (PRD #201 M4.1, the flagship).
- **Layer:** L2 (in-process daemon whose hook loop routes `delegate`/`work-done`/`agent-event` and re-broadcasts `AgentEvent`s; real agent PTYs via `AgentPtyRegistry::spawn_agent` — the `e2e` tier, hits a real model). Mirrors `e2e_delegate_work_done_chain.rs` with the ORCHESTRATOR role swapped to `pi`: the worker (spawned + ready first) is a black-box `claude` with its hooks/CLI unchanged; the orchestrator is a real `pi` whose HOME carries the bundled extension (materialized via `orchestrator_ext::materialize`). `OPENROUTER_API_KEY` + `HOME` are explicitly propagated into the pi child's `opts.env` (the key is never printed).
- **Agent:** REAL `pi` 0.80.6 orchestrator (`--provider openrouter --model openai/gpt-5-nano --approve`, the cheapest GPT-5.x tier that reliably tool-calls) + REAL Claude Code (Haiku, `claude-haiku-4-5-20251001`, `--allowedTools Bash Read Write`) worker. Flaky-tolerant pre-PR tier (real LLM) — run once, not looped (rule 4/5). Runtime-skipped (Decision 26) when `pi`/`claude`/credentials/`OPENROUTER_API_KEY` are absent.
- **Asserts:** the directive-prompted pi calls the native `delegate` tool once (role `coder`), the daemon routes it into the pre-spawned worker pane, and the real worker creates the sentinel `pi_orch_sentinel_7c3f.txt` (contents `PI_ORCH_SENTINEL_OK`) via the delegated task (proves the full pi→daemon→worker route ran); the daemon writes `.dot-agent-deck/work-done-coder.md` (work-done returned to the orchestrator); and a `Pi`-typed `AgentEvent` for the orchestrator pane rode the daemon's broadcast — status tracked through the extension's `agent-event` path with NO hook installed. Generous per-step timeouts (240s sentinel / 120s work-done) sized to confidence, not token cost (Design Decision #7).
- **Does not assert:** exact agent phrasing / the exact task text pi forwards (the sentinel filename + content are the literal tokens that must survive); the extension's per-event state mapping (covered deterministically by the TS unit tests + synthetic `status/agent-event/003`); the daemon's routing/role-guard internals (covered by `orchestration/delegate/*`).
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** one cheap gpt-5-nano turn (orchestrator delegates) + one short Haiku turn (worker creates a file + work-done) — well under Decision 23's <$0.05/run bound.

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

##### mouse/buttonbar/002 — On a narrow/windowed terminal the full bar WRAPS to multiple rows keeping full labels (PRD #144 — no shortcut-only chips).
- **Layer:** L1.
- **Agent:** none (renders the full global + dashboard context bar at 80 cols into a multi-row area).
- **Asserts:** at a narrow/windowed 80 cols the full `[Label Shortcut]` set (~133 cells) does not fit one row, so PRD #144 has the bar WRAP to multiple rows keeping the full label of every button — `[New Pane Ctrl+N]`, `[Close Ctrl+W]`, `[Toggle Layout Ctrl+T]`, `[Help ?]`, `[Quit Ctrl+C]`, and `[Scheduled Tasks s]` all render somewhere across the rows — the shortcut-only `[Ctrl+N]` chip is absent, and the bar occupies ≥2 rows. Inverts the pre-#144 shortcut-only degradation.
- **Does not assert:** exact column widths; which button lands on which row; the exact row count beyond "more than one".
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

##### mouse/buttonbar/006 — At the default 120-col PTY width the FULL dashboard button set WRAPS to a second row keeping full labels (PRD #144 — no shortcut-only chips, Scheduled Tasks not special-cased).
- **Layer:** L1.
- **Agent:** none (renders the full global + dashboard context bar, including the always-shown Scheduled Tasks button, into a multi-row area).
- **Asserts:** at 120 cols (`DEFAULT_COLS`) the full set (~133 cells) overflows one row, so PRD #144 has the bar WRAP to a second row keeping EVERY button's full label — the full `[New Pane Ctrl+N]` label is present and the shortcut-only `[Ctrl+N]` chip is absent — and the bar occupies ≥2 rows. Degradation is uniform: `[Scheduled Tasks s]` is full-labelled like the rest, NOT special-cased to keep its label while others chip. Inverts the pre-#144 collapse-to-chips behavior at the reference width.
- **Does not assert:** the exact column widths; click behavior; which button lands on which row; the exact ceded row count (pinned by `render/layout/004`); the full-label rendering at roomy widths (covered by `mouse/buttonbar/001` / `005`).
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

##### mouse/inline/001 — Inline filter/rename rows gain Apply/Save/Cancel buttons; PaneInput gains `[Command Mode Ctrl+D]`.
- **Layer:** L1 (button render) + L2 (click outcomes).
- **Agent:** none (synthetic card + a real `--continue` pane for detach).
- **Asserts:** the filter row renders `[Apply]`/`[Cancel]` and the rename row `[Save]`/`[Cancel]` alongside the input; clicking them commits/abandons like Enter/Esc; clicking inside the field keeps it focused (typing stays keyboard); `[Command Mode Ctrl+D]` returns from PaneInput to the dashboard.
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

##### theme/guard/001 — No absolute background on any cheaply-seamable surface; selection is cued by the Magenta+BOLD border, not an absolute fill.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none.
- **Asserts:** rendering the five overlay seams plus a session card in both the unselected and selected states, (a) no cell carries a `Color::Rgb(..)` background — backgrounds must be `Color::Reset`; and (b) the selected card is distinguished from the unselected one by a terminal-relative cue (the `▸ ` title prefix and a `Color::Magenta`+BOLD border — the dedicated PRD #155 `selected` accent role, which never reuses a status color or the `focused` cyan) rather than an absolute `selected_bg` fill.
- **Does not assert:** named-ANSI accents/status colors; the `render_frame` canvas/tab-bar fills (not cheaply reachable through a render seam — guarded by `theme/guard/002`).
- **Platform coverage:** mac+linux+windows.

##### theme/guard/002 — `src/ui.rs` carries no forbidden absolute-background patterns (source lint).
- **Layer:** L1 (source lint — reads `src/ui.rs` from disk; no rendering).
- **Agent:** none.
- **Asserts:** `src/ui.rs` contains none of `bg(Color::Rgb`, `bg(palette.terminal_bg)`, `bg(palette.selected_bg)`, `bg(palette.tab_bar_bg)` — guarding the `render_frame` canvas/tab-bar fills that paint the whole window and aren't cheaply reachable through a render seam.
- **Does not assert:** runtime rendering behavior (covered by `theme/guard/001` and `theme/contrast/001`); absolute colors in other source files.
- **Platform coverage:** mac+linux+windows.

##### theme/guard/003 — The deck-card, embedded-pane and stats-bar render paths resolve colors through the centralized palette, not inline status literals (source lint).
- **Layer:** L1 (source lint — reads `src/ui.rs` and `src/terminal_widget.rs` from disk; no rendering).
- **Agent:** none.
- **Asserts:** both render paths reference the centralized `palette`; the deck-card status mapping (`status_style`) and border resolver (`render_session_card`) in `src/ui.rs` carry no inline status/accent `Color::Green/Blue/Yellow/Red/Cyan`/`Color::Magenta` literals; the embedded-pane path (`src/terminal_widget.rs`) carries no inline status `Color::Green/Blue/Yellow/Red` literal; and the stats bar (`render_stats_bar` in `src/ui.rs`) carries no inline status `Color::Green/Blue/Yellow/Red` literal — the palette is the single source of truth (PRD #155 M4 tightening).
- **Does not assert:** the palette module's exact API/shape (the rendered-color tests `theme/palette/001-004` cover behavior); absolute backgrounds (covered by `theme/guard/002`); the stats bar's legitimate non-status `Color::Cyan` (active-count) and `Color::LightMagenta` (mode-label) accents, which are not status roles; inline literals in render paths other than the deck-card/pane/stats-bar status colors.
- **Platform coverage:** mac+linux+windows.

#### theme/palette

##### theme/palette/001 — Deck-card border encodes status via the centralized palette roles.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (six live session fixtures, one per status).
- **Asserts:** rendering a deck card (not selected, not focused) for each agent status resolves its border to the matching centralized status role — working=`Color::Green`, thinking=`Color::Blue`, compacting=`Color::Blue` (shares the thinking role), waiting=`Color::Yellow`, error=`Color::Red`, idle=`Color::DarkGray`; and that no status border reuses an accent role (`Color::Magenta`=selected, `Color::Cyan`=focused), so a status never collides with selection/focus.
- **Does not assert:** the per-card status badge text/glyph; selection/focus accents (covered by `theme/palette/003-004`); the palette module's internal API (reads the rendered border color).
- **Platform coverage:** mac+linux+windows.

##### theme/palette/002 — Embedded-pane border uses the SAME status color the deck card uses (deck/pane consistency).
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (six live session fixtures + a `TerminalWidget` per status).
- **Asserts:** for each agent status (including compacting, which shares the thinking/Blue role), the embedded pane's border color (neither selected nor focused) equals the deck card's border color for that status, and both equal the palette status role — so a given state looks identical as a deck card and as an embedded pane (PRD #155 success criterion #2).
- **Does not assert:** pane content/title rendering; the focused/selected pane accents (covered by `theme/palette/004` / `theme/guard/001`).
- **Platform coverage:** mac+linux+windows.

##### theme/palette/003 — Selected deck-card border is the dedicated `selected` accent (Magenta+BOLD+marker), never a status/focus color.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (one selected live session fixture).
- **Asserts:** rendering a selected deck card resolves its border to `Color::Magenta` + `Modifier::BOLD` with a `▸ ` title marker, and that this color is neither the working-status green nor the focused-pane cyan — the `selected` role never collides with the status palette or the `focused` accent.
- **Does not assert:** the status badge (still shows status independent of selection); the absolute-background guard (covered by `theme/guard/001`).
- **Platform coverage:** mac+linux+windows.

##### theme/palette/004 — Focused-pane border is the dedicated `focused` accent (Cyan), distinct from every status and from `selected`.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (one focused `TerminalWidget`).
- **Asserts:** rendering a focused embedded pane resolves its border to `Color::Cyan`, and that this color is distinct from every status role (green/blue/yellow/red/dark-gray) and from the `selected` accent (magenta) — focus stays Cyan while selection moves to Magenta, so status/selection/focus are provably distinct (PRD #155 success criterion #3). Also asserts the PRECEDENCE invariant: a pane that is focused AND carries a present `Working` status still renders the focused accent (Cyan), never the Working/Green status color — focus OVERRIDES a present status in the unified border precedence (Option A).
- **Does not assert:** unfocused-pane status coloring (covered by `theme/palette/002`); pane content rendering.
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

##### scheduler/cli/004 — `dot-agent-deck schedule add` accepts the issue-dispatch flags (`--repo`/`--max-per-run`/`--label`/`--query`, `--command` optional) and writes a `[scheduled_tasks.issue_dispatch]` sub-table that round-trips + reloads (PRD #120).
- **Layer:** L2.
- **Agent:** none (runs the `schedule` CLI subprocess against a live `daemon serve`).
- **Asserts:** running `schedule add --repo acme/widgets --max-per-run 2 --label … --query …` (plus name/cron/working-dir/prompt) WITHOUT `--command` succeeds; the global `schedules.toml` gains a `[scheduled_tasks.issue_dispatch]` sub-table whose repo/max_per_run/label/query round-trip back into an `IssueDispatchConfig` through the loader; the running daemon registers the task via the add-triggered reload; and a malformed `--repo` (not `owner/name`) exits non-zero with a clear error. RED until the flags exist: today `schedule add` has no `--repo`/`--max-per-run`/`--label`/`--query`, so clap rejects the unknown `--repo` and the add exits non-zero.
- **Does not assert:** the dispatch flow on fire (covered by `scheduler/dispatch/*`); the exact malformed-repo wording (loose substring on "repo" + owner/name/slug).
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

#### scheduler/dispatch

##### scheduler/dispatch/001 — Firing an `issue_dispatch` task clones the repo, creates a per-issue worktree on `agent/issue-<n>`, and spawns an agent into it with the substituted prompt (PRD #120 M2.1–M2.3).
- **Layer:** L2 (headless `dot-agent-deck daemon serve` driven via the `RunNow` control message — no PTY/grid, same shape as `scheduler/spawn/*`). All GitHub access is isolated offline behind a stub `gh` on PATH (`issue list`/`pr list` → canned JSON; `repo clone` → `git clone` of a local one-commit fixture remote that carries a committed `.dot-agent-deck.toml`).
- **Agent:** none (run-now; the fixture orchestration role runs `cat`, which echoes the delivered prompt).
- **Asserts:** the repo is cloned to `<working_dir>/<name>`, the worktree appears at `<clone>/.worktrees/issue-7` with branch `agent/issue-7` (via `git`), and an `orchestrator`-role agent rooted at that worktree (`orchestration_cwd`) receives the substituted per-issue prompt (`ISSUEDISPATCH-7`, echoed by `cat`).
- **Does not assert:** the single-agent-card branch (covered by `scheduler/dispatch/004`); fetch+pull refresh of an existing clone; the exact `gh` argv (covered by the pure-data `issue_dispatch` unit tests).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/002 — A second fire with no intervening close skips an issue whose worktree already exists: no re-clone error, no duplicate spawn, and the skip is surfaced (PRD #120 M2.2 idempotency, primary signal).
- **Layer:** L2 (as `scheduler/dispatch/001`).
- **Agent:** none (run-now; observes the registry orchestrator count + on-disk worktree/clone + daemon stderr).
- **Asserts:** the first fire creates the issue-7 worktree and one orchestrator agent; a second fire leaves the worktree and clone in place, does NOT grow the orchestrator count beyond one (no duplicate spawn), and surfaces a skip for the already-claimed issue.
- **Does not assert:** the open-PR secondary signal (covered by `scheduler/dispatch/003`); the exact skip-message wording (loose substring on the issue key / "skip").
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/003 — An issue whose `gh pr list` reports an open PR on `agent/issue-<n>` is skipped while a sibling issue with no PR dispatches (PRD #120 M2.2 idempotency, secondary signal).
- **Layer:** L2 (as `scheduler/dispatch/001`; the stub `gh pr list --head agent/issue-7` returns a non-empty array, while issue 8 returns `[]`).
- **Agent:** none (run-now; observes per-issue worktrees + orchestrator count).
- **Asserts:** issue 8 (no PR) dispatches — worktree present, orchestrator agent running — proving the flow ran; issue 7 (open PR) is skipped — no `issue-7` worktree, and the run's orchestrator count is one.
- **Does not assert:** parsing `Closes #n` from PR bodies (the check keys on the deterministic head branch only); the worktree-exists primary signal (covered by `scheduler/dispatch/002`).
- **Note:** a control issue (8, no PR) is included so "the flow ran AND issue 7 was skipped" is observable from end-state alone.
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/004 — A clone with an `[[orchestrations]]` block opens an orchestration tab (prompt to the `orchestrator` role); a clone without one opens a single-agent card (prompt delivered) — reached through the dispatch path (PRD #120 M2.3).
- **Layer:** L2 (as `scheduler/dispatch/001`; two `issue_dispatch` tasks, one fixture remote with a committed `.dot-agent-deck.toml`, one without; `default_command = cat` via `DOT_AGENT_DECK_CONFIG` so the single-agent card runs `cat`).
- **Agent:** none (run-now; observes `ListAgents` tab_membership + spawn cwd + PTY prompt echo).
- **Asserts:** the orchestration clone spawns an `orchestrator`-role agent in its worktree and the substituted prompt (`ORCHDISP-11`) reaches it; the plain clone spawns a non-orchestration single-agent card whose cwd is its worktree and the substituted prompt (`PLAINDISP-22`) reaches it.
- **Does not assert:** the clone/worktree/branch derivation (covered by `scheduler/dispatch/001`); the orchestration-vs-card branch outside the dispatch path (covered by `scheduler/spawn/002`).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/005 — When `gh` returns more open issues than `max_per_run`, only the first N (in returned order) get worktrees + spawns; the rest are left untouched (PRD #120 M3.1 cap).
- **Layer:** L2 (as `scheduler/dispatch/001`; the stub returns five issues while `max_per_run = 2`, so the flow's own cap — not the stub — bounds the run).
- **Agent:** none (run-now; observes per-issue worktrees + orchestrator count).
- **Asserts:** issues 1 and 2 are dispatched (worktrees present), issues 3–5 are left untouched (no worktrees), and exactly two orchestrator agents exist.
- **Does not assert:** issue ordering/scoring beyond "returned order" (out of scope per the PRD); the label/query filters (pure-data `issue_dispatch` argv tests cover those).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/006 — Closing a dispatched tab removes its worktree from disk and `git worktree list` while preserving the clone (PRD #120 M2.4 tab-close → cleanup plumbing).
- **Layer:** L2 (as `scheduler/dispatch/001`; close is driven via the `StopAgent` control message on the dispatched orchestrator).
- **Agent:** none (run-now to dispatch; `StopAgent` to close; observes on-disk worktree/clone + `git worktree list`).
- **Asserts:** after dispatch the issue worktree exists; after closing the tab the worktree is gone from disk and from `git worktree list`, while the clone directory remains.
- **Does not assert:** the in-deck close gesture (`Ctrl+w`) — the daemon-side close→cleanup contract is exercised over the protocol; auto-restoration of dispatched tabs (out of scope per the PRD).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/007 — One issue's dispatch failing (a simulated `gh` error for that issue) does not abort the others, and the failure is surfaced as a notification, not swallowed (PRD #120 M3.2 per-issue resilience).
- **Layer:** L2 (as `scheduler/dispatch/001`; the stub `gh pr list --head agent/issue-11` exits non-zero while issue 10 is healthy).
- **Agent:** none (run-now; observes survivor worktrees + orchestrator count + daemon stderr).
- **Asserts:** issue 10 still dispatches (worktree + orchestrator agent) despite issue 11 failing; issue 11 produces no worktree; and a failure referencing issue 11 is surfaced through the notifier (daemon stderr).
- **Does not assert:** cross-repo fan-out resilience (one repo per task — removed from scope); the exact failure-message wording (loose substring on the issue 11 key).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/008 — An issue dispatched, then closed without a PR (worktree removed, branch left behind), is re-dispatched on a later fire: the worktree is re-created and an agent spawns again, with no failure surfaced (PRD #120 B1 — `worktree add` must tolerate the leftover `agent/issue-<n>` branch).
- **Layer:** L2 (as `scheduler/dispatch/001`; first run-now to dispatch, `StopAgent` to close, second run-now while the stub still reports the issue open with no PR).
- **Agent:** none (run-now ×2 + `StopAgent`; observes the re-created worktree, a re-spawned orchestrator, and daemon stderr).
- **Asserts:** after close the worktree is gone but branch `agent/issue-7` survives; the second fire re-creates the issue-7 worktree and spawns the orchestrator again; no per-issue failure (`failed:` / "already exists") is surfaced.
- **Does not assert:** the exact branch-reattach git mechanics (probe vs. retry-without-`-b`) — only the observable re-dispatch; behavior when an open PR exists (covered by `scheduler/dispatch/003`).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/009 — Closing ONE role of a multi-role orchestration dispatch leaves the shared issue worktree on disk; only closing the LAST role removes it, clone preserved (PRD #120 S1 — refcount the worktree, remove on last close).
- **Layer:** L2 (as `scheduler/dispatch/001`; the fixture remote commits a two-role `[[orchestrations]]` config — `orchestrator` + `reviewer`, both `cat` — so a dispatch opens two role panes sharing one `orchestration_cwd`).
- **Agent:** none (run-now to dispatch; `StopAgent` per role; observes on-disk worktree + `git worktree list` + clone dir).
- **Asserts:** both role panes spawn into the same issue worktree; closing the reviewer leaves the worktree present (disk + `git worktree list`); closing the orchestrator (last role) removes the worktree while the clone directory remains.
- **Does not assert:** the refcount/registry internals (counted at spawn, decremented per close) — only the observable last-close-removes contract; the single-role close path (covered by `scheduler/dispatch/006`).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/011 — A fired `issue_dispatch` task surfaces its per-issue card LIVE on an already-attached TUI — the user-visible showcase (and demo-reel clip) the headless `scheduler/dispatch/001-009` family can't observe (PRD #120 M2.3 live surfacing).
- **Layer:** L2 PTY (the real `dot-agent-deck` binary in an isolated PTY via the `TuiDeck` harness, asserted on the rendered vt100 grid — same harness as `scheduler/live/*`, NOT the headless `daemon serve` of `scheduler/dispatch/001-009`). Composes the OFFLINE GitHub seam (stub `gh` on PATH: `issue list`/`pr list` → canned JSON, `repo clone` → `git clone` of a local one-commit fixture remote with NO `.dot-agent-deck.toml`) with the live-fire seam (`DOT_AGENT_DECK_SCHEDULES` loaded by the lazily-spawned daemon; fire via the `RunNow` control message over the deck's attach socket). The dispatch behavior is ungated, so the env carries no `DOT_AGENT_DECK_EXPERIMENTAL`; `default_command = cat` (via `DOT_AGENT_DECK_CONFIG`) makes the dispatched single-agent card a long-lived `cat`.
- **Agent:** none (run-now; the dispatched single-agent card runs `cat`, no real LLM, no real GitHub).
- **Asserts:** after the fire the daemon registers the dispatched agent under the schedule's friendly name `github-issues` (precondition), then a per-issue card surfaces LIVE on the rendered dashboard — its `Dir:` line shows the issue worktree basename `issue-7` (the per-issue identity) and its title shows the schedule name `github-issues`.
- **Does not assert:** the clone/worktree/branch derivation or skip/dedup/cap/cleanup logic (covered by the headless `scheduler/dispatch/001-009`); the orchestration-tab dispatch path (NOT live-surfaced by `spawn` — rebuilt by the TUI's hydration path on reconnect, the #140 session-partitioning concern); prompt-echo delivery into the card.
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/012 — A worktree-present second fire short-circuits to a SKIP BEFORE the open-PR check, so a transient `gh pr list` error on that issue never surfaces as a failure (PRD #120 / Greptile P1 regression guard — primary signal short-circuits the secondary, commit 212bc73).
- **Layer:** L2 (as `scheduler/dispatch/001`; first run-now dispatches issue 7, then the stub is armed so `gh pr list --head agent/issue-7` exits non-zero, then a second run-now fires with the worktree already present).
- **Agent:** none (run-now ×2; observes the orchestrator count + on-disk worktree/clone + daemon stderr).
- **Asserts:** the second fire does NOT grow the orchestrator count (no duplicate spawn/re-creation), surfaces an `IssueDispatchSkipped` ("already-claimed issue #7") for the present worktree, does NOT surface an `IssueDispatchFailed` ("issue #7 … failed") despite the armed `gh pr list` error, and leaves the worktree and clone in place.
- **Does not assert:** the worktree-absent path that DOES consult the open-PR signal and propagates a `gh` error as a failure (covered by `scheduler/dispatch/007`); the plain worktree-present skip without a PR-check hazard (covered by `scheduler/dispatch/002`); the exact skip/failure wording (loose substring on the issue-7 key).
- **Note:** the fix is in current code, so this is GREEN as a regression guard, not RED-first; it pins that the primary (worktree-exists) signal short-circuits the secondary (open-PR) check, which `scheduler/dispatch/002` cannot catch because it never forces the PR check to error.
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/013 — A fired `issue_dispatch` task against an ORCHESTRATION repo drives the GENUINE `gh` → clone → per-issue worktree → real-agent path against LIVE GitHub, and the dispatched orchestration must surface LIVE as an orchestration TAB (with its orchestrator + worker role panes) on the already-attached TUI — the real-scenario multi-agent showcase (CLAUDE.md rule 4) a `cat`/stub stand-in can never prove (PRD #120). RED until the daemon live-surfaces a dispatched orchestration tab.
- **Layer:** L2 PTY (the real `dot-agent-deck` binary in an isolated PTY via the `TuiDeck` harness, asserted on the rendered vt100 grid — same harness as `scheduler/dispatch/011`). REAL seams, no stand-ins: REAL `gh` on the normal PATH (no `gh` stub) really enumerates/PR-checks/clones against live GitHub, with `GITHUB_TOKEN` threaded through the scrubbed deck env so the daemon's `gh` inherits auth; the clone's `[[orchestrations]]` resolves to two FULLY INTERACTIVE `claude` role panes pinned to Haiku (`claude-haiku-4-5-20251001`, `--allowedTools Bash`, no `-p`); the freshly-built `dot-agent-deck` binary's dir is prepended to the deck→daemon→agents PATH (`with_env("PATH", …)` wins over the harness scrub) so the orchestrator's `dot-agent-deck delegate --to worker` resolves. The dispatch behavior is ungated, so the env carries no `DOT_AGENT_DECK_EXPERIMENTAL`; the fire is driven by `RunNow` over the attach socket.
- **Fixture:** the permanent public repo `vfarcic/dot-agent-deck-tests` — a committed `DISPATCH_E2E_SENTINEL.md`, a `.dot-agent-deck.toml` with `[[orchestrations]] name = "issue-work"` (roles `orchestrator` (start) + `worker`, both Haiku `claude`; the orchestrator's `prompt_template` delegates the task to the worker), and a PERMANENT open issue #1 labelled `agent-dispatch-test`. The schedule filters on that label with `max_per_run = 1`, so ONLY issue #1 is enumerated (deterministic). Both role panes share the per-issue worktree cwd (pre-trusted in the per-test HOME so claude's first-run gates clear with no keystroke). Clone + worktree live under a `tempfile::tempdir()` removed on drop.
- **Agent:** REAL Claude Code (Haiku) ×2 role panes, cheap interactive turns (<$0.05/run). Flaky-tolerant pre-PR tier (real LLM + real network) — run once, not looped (rule 4). Runtime-skipped (Decision 26) when the `claude` CLI/credentials or `GITHUB_TOKEN` are absent.
- **Asserts:** after the fire the daemon registers the dispatched orchestration's role agents under the schedule name `github-issues` (precondition — proves the live clone + worktree + spawn happened); the dispatched ORCHESTRATION then surfaces LIVE as an orchestration TAB labelled `issue-work` (the fixture's `[[orchestrations]] name`) in the attached TUI's tab strip, with no reconnect/relaunch — RED today, because `spawn::spawn`'s orchestration branch does not call `surface_spawned_pane` and orchestration tabs are rebuilt only at hydration, so the role panes appear only as flat dashboard cards and no `issue-work` tab paints live. Best-effort (once GREEN, logged not gated): switching to the orchestration tab, the worker (delegated to by the orchestrator) lists the cloned repo's files including the committed sentinel `DISPATCH_E2E_SENTINEL.md`; and the fixture repo has no pushed `agent/issue-1` branch afterward (NO REMOTE WRITES).
- **Does not assert:** the delegation chain / sentinel as a hard gate (logged best-effort — too LLM/timing-dependent); exact agent phrasing; the clone/worktree/branch derivation or skip/dedup/cap/cleanup logic (covered by the headless `scheduler/dispatch/001-009` and the deterministic-stub `scheduler/dispatch/011-012`); the single-agent live-surfacing path (covered by `scheduler/dispatch/011`).
- **Platform coverage:** mac+linux.

#### scheduler/pi

##### scheduler/pi/001 — A SCHEDULED, UNATTENDED real `pi` job (no TUI client attached) boots and its bundled extension reports the Pi pane's status via `agent-event`, re-broadcast on the daemon's event stream (PRD #201 M4.2).
- **Layer:** L2 (real `daemon serve` via the `DaemonProc` harness — no PTY, no attached TUI). The schedule's `command` is a REAL `pi` (`--provider openrouter --model openai/gpt-5-nano --approve -p ready`, a cheap non-interactive turn); the bundled extension is materialized into the daemon's HOME (via `orchestrator_ext::materialize`) so the scheduler-spawned pi (which inherits that HOME) auto-discovers it. `OPENROUTER_API_KEY` (never printed) + the freshly-built binary dir on PATH are propagated into the daemon via `spawn_daemon_serve_with_env` and inherited by the spawned pi. The fire is driven by `RunNow`; status is observed via an unattended `SubscribeEvents` consumer.
- **Agent:** REAL `pi` 0.80.6 (cheap gpt-5-nano `-p` turn). Flaky-tolerant pre-PR tier — run once, not looped. Runtime-skipped (Decision 26) when `pi`/`OPENROUTER_API_KEY` are absent.
- **Asserts:** after `RunNow`, the scheduled pi boots and its real extension shells `dot-agent-deck agent-event`, which the daemon ingests and re-broadcasts as a `Pi`-typed `AgentEvent` in one of the extension's mapped states (`WaitingForInput`/`Thinking`/`Idle`) carrying the scheduler-injected pane id — proving a scheduled, unattended (no-client) real pi is status-tracked through the same `AgentEvent` contract every client consumes. The match EXCLUDES `SessionStart`: the scheduler's `surface_spawned_pane` broadcasts a synthetic `SessionStart` with the `from_command`-guessed `Pi` type the instant the pane spawns (before pi's runtime boots), so requiring a non-`SessionStart` state is what makes the pass attributable to the REAL extension rather than the daemon's spawn-time guess.
- **Does not assert:** the delegate/work-done chain (covered by `chain-smoke/pi/001`); the exact lifecycle→state mapping across running/waiting/finished (covered synthetically by `status/agent-event/003` and the TS unit tests); a dashboard-attached Pi pane (the synthetic dashboard render is `dashboard/pane/007`; the real-agent unattended path is the M4.2 value here).
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** one cheap gpt-5-nano `-p` turn (and the status assertion resolves on boot, before the turn completes) — well under Decision 23's <$0.05/run bound.

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

##### scheduler/manager/002 — Editing a schedule reuses the Ctrl+n dir-picker + mode-locked Edit Schedule form; submitting spawns the seeded authoring agent running the CONFIGURED command (`default_command`), pre-filled with the row's current values (PRD #127 M3.3; PRD #170 M2.1 + unified Add/Edit flow).
- **Layer:** L2 (same no-L1-seam reason for the manager dialog; the mode-locked form's render is covered at L1 by `scheduler/form/001`). Two shims are on PATH: a distinctive `default_command` (e.g. `stub-authoring`) shimmed to a recorder that posts SessionStart and records its delivered seed, and `claude` shimmed to a separate neutralizing recorder (so the host's real `claude` is never invoked and so a fall-back-to-`claude` regression is observable).
- **Agent:** the shimmed authoring agent (records the gated-delivered seed, mirroring how `tabs/mode/005` observes seed delivery).
- **Asserts:** with `default_command` set to the distinctive stub, pressing `e` on a row opens the directory picker (` Select Directory `); confirming the dir with Space opens the mode-locked ` Edit Schedule ` form (Command pre-filled from `default_command`); submitting via `[Submit]` spawns the seeded authoring agent running THAT configured command — its recorder receives the authoring seed carrying the row's current prompt value (pre-fill), AND the `claude` recorder receives nothing (the confirmed command came from `default_command`). RED until the unified flow exists: today `e` opens the deleted pick-agent modal, so the dir picker's ` Select Directory ` chrome never renders and the wait times out.
- **Does not assert:** the full authoring seed-prompt text; that the agent ultimately calls `schedule update` (covered by the CLI + seed-delivery mechanism); the add (blank) path (covered by `scheduler/form/002` / `scheduler/manager/010`); the spawn-in-picked-dir / working_dir pre-seed (covered by `scheduler/form/002` / `scheduler/form/003`); the mode-locked form's render (covered by `scheduler/form/001`).
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
- **Asserts:** after arming delete (`d`) on a long-named row, the confirmation's trailing `(y/n)` prompt — the only `(y/n)` in the app — still renders, proving the message is contained within the modal. Under PRD #144 the confirmation sits on two fixed natural lines (the name line; the `… (y/n)` trailer) and the content-sized modal grows in WIDTH to contain the long name line (clamped to ≤90% of the terminal), so the trailer is never clipped off the right border — superseding the PRD #127 wrap-to-grow-height band-aid.
- **Does not assert:** the modal's precise content-sized width / clamp fraction; the confirmation wording beyond the `(y/n)` tail and `Delete schedule` prefix.
- **Platform coverage:** mac+linux.

##### scheduler/manager/006 — Clicking a schedule row moves the selection to that row (PRD #127 finding — mouse parity).
- **Layer:** L2 (same no-L1-seam reason). Drives the real dialog via `S`, then a left-click SGR mouse report on a row, asserting on the rendered vt100 grid.
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`, two enabled tasks).
- **Asserts:** with two rows (`alpha` auto-selected, `bravo` not), clicking the `bravo` row moves the `▶` selection marker to it (`▶ bravo` renders and `▶ alpha` is gone), proving a row click hit-tests and re-selects.
- **Does not assert:** that the click also fires an action (it only selects); keyboard j/k navigation (the pre-existing selection path); scroll-into-view when the clicked row is off-window.
- **Platform coverage:** mac+linux.

##### scheduler/manager/007 — The manager dialog auto-sizes to its content and renders all fields un-clipped at both a roomy and a windowed width (PRD #144).
- **Layer:** L2 (no public L1 dialog render seam — same constraint as `scheduler/manager/001`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid, at two PTY sizes via `with_pty_size`). Opened with the `S` keybinding.
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`, one enabled task whose name is longer than the legacy fixed-width name cell).
- **Asserts:** opening the manager at a roomy (200-col) terminal AND at a windowed (80-col) terminal renders the task's FULL name un-clipped on the grid at both widths — proving the dialog auto-sizes to its content (PRD #144 shared modal sizing helper, clamped within the windowed terminal) instead of truncating the field to the fixed 72-col modal. RED today: the modal is hard-capped at 72 cols and the name is truncated to 21 chars (`truncate_cell`), so the full name never appears.
- **Does not assert:** the exact modal width / clamp fraction at each terminal size; the `[min, max]` bounds of the shared helper (covered by the coder's pure-data unit test); the delete-confirmation containment (covered by `scheduler/manager/005`).
- **Platform coverage:** mac+linux.

##### scheduler/manager/010 — A blank/unset `default_command` falls back to `claude` (`DEFAULT_AUTHORING_COMMAND`) for the authoring agent, NOT a bare `$SHELL` (PRD #170 R1 fallback, via the unified Add flow).
- **Layer:** L2 (drives the real manager + dir-picker + mode-locked form via PTY; observed via a `claude` recorder shim on disk).
- **Agent:** the shimmed `claude` authoring agent (records the gated-delivered seed).
- **Asserts:** with `default_command = ""` (the unconfigured-user case), pressing `a` (Add) opens the directory picker (` Select Directory `); confirming the dir with Space opens the mode-locked ` New Schedule ` form whose Command pre-fills via the resolved authoring command (a blank default → `claude`); submitting via `[Submit]` spawns `claude` — its recorder receives the base authoring seed (`throwaway authoring session`) — proving the blank command resolves to the default authoring command instead of spawning a bare login shell that cannot act on the seed. RED until the unified flow exists: today `a` opens the deleted pick-agent modal, so the dir picker never appears and the ` Select Directory ` wait times out.
- **Does not assert:** the whitespace-only variant of the fallback (the same code path); the mode-locked form's render (covered by `scheduler/form/001`).
- **Platform coverage:** mac+linux.

#### scheduler/form

##### scheduler/form/001 — The new-pane form mode-locked to schedule renders ONLY Dir + Command (no Mode cycler, no Name field) and titles itself ` New Schedule ` (Add) / ` Edit Schedule ` (Edit) (PRD #170 unified Add/Edit flow).
- **Layer:** L1 (ratatui `TestBackend` via a new public `render_new_pane_form_schedule_to_buffer(edit, w, h)` seam, mirroring `render_new_pane_form_to_buffer`). RED is a COMPILE error until the coder adds the seam + the `NewPaneFormState::new_schedule_locked` constructor and locked render branches it drives.
- **Agent:** none.
- **Asserts:** the schedule-locked form renders the Dir field, the (free-text) Command field, and the `[Submit]`/`[Cancel]` buttons, with the Mode cycler HIDDEN (no `No mode` chip) and the Name field HIDDEN (no `Name:`); its title is ` New Schedule ` in the Add variant (`edit = false`) and ` Edit Schedule ` in the Edit variant (`edit = true`). RED until the locked render branches exist: today the form always shows the Mode cycler + Name field and titles itself ` New Agent `.
- **Does not assert:** the Command pre-fill value (configured-command resolution is covered at L2 by `scheduler/manager/002`/`010`); the spawn on submit (covered by `scheduler/form/002`/`003`); insta byte-snapshot identity (plain substring assertions, matching `mouse/form/001`).
- **Platform coverage:** mac+linux+windows.

##### scheduler/form/002 — Manager Add reuses the Ctrl+n dir-picker + mode-locked ` New Schedule ` form; submitting spawns the seeded authoring agent IN the picked directory (PRD #170 unified Add/Edit flow).
- **Layer:** L2 (drives the real manager → dir picker → mode-locked form via PTY; observed via distinct-name recorder shims on disk that record their spawn `pwd` then the delivered seed). `default_command = "stub-add-authoring"` (a recorder shim) with a `claude` neutralizer on PATH.
- **Agent:** the shimmed `stub-add-authoring` authoring agent (records spawn cwd + the gated-delivered seed).
- **Asserts:** pressing `a` (Add) opens the directory picker (` Select Directory `); confirming the current dir with Space opens the mode-locked ` New Schedule ` form (Command pre-filled from `default_command`); submitting via `[Submit]` spawns the seeded authoring agent — its recorder receives the base authoring seed (`throwaway authoring session`) AND its recorded `pwd` carries the picked dir's basename (the agent spawned IN the confirmed directory), while the `claude` neutralizer stays empty. RED until the unified flow exists: today `a` opens the deleted pick-agent modal, so the dir picker never appears and the ` Select Directory ` wait times out.
- **Does not assert:** the Edit pre-fill / working_dir-from-row behavior (covered by `scheduler/form/003`); the blank-default→`claude` fallback (covered by `scheduler/manager/010`); the mode-locked form's render (covered by `scheduler/form/001`).
- **Platform coverage:** mac+linux.

##### scheduler/form/003 — Manager Edit starts the dir picker at the row's `working_dir`, pre-fills the authoring seed with the existing schedule's values, and spawns the agent IN that working_dir (PRD #170 unified Add/Edit flow).
- **Layer:** L2 (drives the real manager → dir picker → mode-locked form via PTY; observed via distinct-name recorder shims on disk that record their spawn `pwd` then the delivered seed). `default_command = "stub-edit-authoring"` (a recorder shim) with a `claude` neutralizer on PATH; the fixture row's `working_dir` is a distinctively-named existing dir (`.../EDITWORKDIR`) and its prompt is `EDITPROMPTMARKER`.
- **Agent:** the shimmed `stub-edit-authoring` authoring agent (records spawn cwd + the gated-delivered seed).
- **Asserts:** pressing `e` (Edit) opens the directory picker which STARTS at the row's `working_dir`; confirming it with Space (no navigation) opens the mode-locked ` Edit Schedule ` form; submitting via `[Submit]` spawns the seeded authoring agent — its recorder receives the row's distinctive prompt `EDITPROMPTMARKER` (the seed is PRE-FILLED with the existing schedule's values) AND its recorded `pwd` carries `EDITWORKDIR` (the picker started at, and pre-seeded as the spawn cwd, the row's working_dir), while the `claude` neutralizer stays empty. RED until the unified flow exists: today `e` opens the deleted pick-agent modal, so the dir picker never appears and the ` Select Directory ` wait times out.
- **Does not assert:** the Add (blank-context) path (covered by `scheduler/form/002`); the configured-command vs `claude` resolution beyond the neutralizer check (covered by `scheduler/manager/002`); the mode-locked form's render (covered by `scheduler/form/001`).
- **Platform coverage:** mac+linux.

##### scheduler/form/004 — Cancelling a MANAGER-originated schedule flow at the DIRECTORY PICKER (Esc / `q`) returns to the Scheduled-Tasks manager dialog, not the bare dashboard (PRD #170 round 4, reviewer F5).
- **Layer:** L2 (drives the real manager → dir picker via PTY; asserted on the rendered vt100 grid plus the daemon registry). A benign `default_command = "cat"` so any erroneous spawn never invokes the host's real `claude`.
- **Agent:** none (the flow is cancelled before any authoring agent spawns).
- **Asserts:** opening the manager (`S`), pressing `a` (Add) or `e` (Edit) opens the directory picker (` Select Directory `); pressing Esc (Add + Edit) or `q` (Add) from the picker returns to the MANAGER dialog — its `NEXT FIRE` header re-renders — with the picker chrome (` Select Directory `) gone and NO `schedule` authoring agent spawned. RED until cancel is intent-aware: today the picker's Esc/`q` handlers unconditionally set `UiMode::Normal` (dashboard), so `NEXT FIRE` never reappears and the wait times out. Restores the intent the removed `scheduler/manager/011` (Esc) / `013` (`q`) pinned, re-targeted at the unified flow.
- **Does not assert:** the form cancel point (covered by `scheduler/form/005`); a `Ctrl+n`-origin cancel still dropping to the dashboard (unchanged, out of scope); the spawn/seed on submit (covered by `scheduler/form/002`/`003`).
- **Platform coverage:** mac+linux.

##### scheduler/form/005 — Cancelling a MANAGER-originated schedule flow at the mode-locked FORM (Esc / click `[Cancel]`) returns to the Scheduled-Tasks manager dialog, not the bare dashboard (PRD #170 round 4, reviewer F5).
- **Layer:** L2 (drives the real manager → dir picker → mode-locked form via PTY; asserted on the rendered vt100 grid plus the daemon registry). A benign `default_command = "cat"` so any erroneous spawn never invokes the host's real `claude`.
- **Agent:** none (the flow is cancelled before any authoring agent spawns).
- **Asserts:** opening the manager (`S`), pressing `a` (Add) or `e` (Edit) → confirming a dir with Space opens the mode-locked schedule form (` New Schedule ` / ` Edit Schedule `, with `[Submit]`); pressing Esc (Add + Edit) or clicking `[Cancel]` (Add) from the form returns to the MANAGER dialog — its `NEXT FIRE` header re-renders — with the form chrome (`[Submit]`) gone and NO `schedule` authoring agent spawned. RED until cancel is intent-aware: today the form's Esc/`[Cancel]` handlers unconditionally set `UiMode::Normal` (dashboard), so `NEXT FIRE` never reappears and the wait times out. Restores the intent the removed `scheduler/manager/015` (click `[Cancel]`) pinned, re-targeted at the unified flow.
- **Does not assert:** the picker cancel point (covered by `scheduler/form/004`); a `Ctrl+n`-origin cancel still dropping to the dashboard (unchanged, out of scope); the spawn/seed on submit (covered by `scheduler/form/002`/`003`).
- **Platform coverage:** mac+linux.

##### scheduler/form/006 — On Edit, re-picking a DIFFERENT working_dir makes that picked dir WIN in the authoring seed — no conflicting old-vs-new working_dir (PRD #170 round 4, reviewer F3).
- **Layer:** L2 (drives the real manager → dir picker → mode-locked form via PTY; observed via a distinct-name recorder shim on disk that records its spawn `pwd` then the delivered seed). `default_command = "stub-repick-authoring"` (a recorder shim) with a `claude` neutralizer on PATH; the fixture row's `working_dir` is a distinctively-named existing dir (`.../ROWDIRALPHA`) with a sibling re-pick target (`.../PICKDIRBRAVO`) and the row's prompt is `EDITPROMPTF3`.
- **Agent:** the shimmed `stub-repick-authoring` authoring agent (records spawn cwd + the gated-delivered seed).
- **Asserts:** pressing `e` (Edit) opens the dir picker started at the row's `working_dir` (`ROWDIRALPHA`); going UP one level (`h`) and descending into the DIFFERENT sibling `PICKDIRBRAVO` (double-click, confirmed via its `INNERMARK` child) then confirming with Space, and submitting via `[Submit]`, spawns the seeded authoring agent whose recorded seed — once delivered through its `EDITPROMPTF3` prompt line (which follows the `working_dir:` line) — carries `PICKDIRBRAVO` but ZERO occurrences of the row's stale `ROWDIRALPHA`. RED today: the edit seed appends the row's `working_dir: .../ROWDIRALPHA` as a conflicting current value alongside the picked `working_dir DEFAULT: .../PICKDIRBRAVO`.
- **Does not assert:** the unchanged-pick / pre-fill path (covered by `scheduler/form/003`); the in-`src` `build_schedule_authoring_mode` seed unit tests (the coder's); the Add path (covered by `scheduler/form/002`).
- **Platform coverage:** mac+linux.

##### scheduler/form/007 — Selecting the experimental `schedule: issues` Mode option seeds the authoring agent with ISSUE-DISPATCH instructions (calls `schedule add --repo …`, gathers `max_per_run`), distinct from the plain `schedule` seed (PRD #120).
- **Layer:** L2 (drives the real new-pane dialog via PTY — the experimental issue-dispatch option lives on the Ctrl+n Mode cycler, not the mode-locked manager form, so this drives Ctrl+n directly; observed via a `stub-issue-authoring` recorder shim on disk that records the gated-delivered seed). `default_command = "stub-issue-authoring"`; the deck is launched with `DOT_AGENT_DECK_EXPERIMENTAL=1`.
- **Agent:** the shimmed `stub-issue-authoring` authoring agent (records the gated-delivered seed).
- **Asserts:** opening the new-pane form (Ctrl+n → Space confirms the dir) and cycling the Mode field to the `schedule: issues` option (waited on via the selection-dependent ` … — schedule: issues mode ` title), then submitting via `[Submit]`, spawns the seeded authoring agent whose recorded seed contains the issue-dispatch guidance `schedule add --repo` AND `max_per_run` — neither present in the plain `schedule` seed (which calls `schedule add --name`). RED today: no `schedule: issues` option exists, so cycling never lands on it and the `schedule: issues mode` title wait times out.
- **Does not assert:** the flag-gated visibility of the option in the cycler (covered by `prompt/new-pane/010`); the CLI write the agent ultimately performs (covered by `scheduler/cli/004`); the full seed-prompt text (loose substring on the issue-dispatch-specific tokens); the plain `schedule` seed (covered by `scheduler/form/002`).
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

##### features/gating/004 — A Pi pane's card shows its first-class identity ONLY with the experimental flag ON; OFF hides the Pi identity while keeping the pane visible (PRD #201 M5.1).
- **Layer:** L1 (ratatui `TestBackend` + buffer-text assertion). Renders the `dashboard/pane/007` Pi fixture via `render_card_to_buffer` and flips the process-global flag with `features::set_for_test(Features::test_with(..))` between renders (serialized with `features/reload/001` via a file-local `FLAG_LOCK`).
- **Agent:** none (a fixture `SessionState` with `agent_type = AgentType::Pi`, no display name).
- **Asserts:** with the flag OFF the rendered card contains no `Pi ·` identity (the pre-feature unrecognized-`pi` baseline) yet still renders the pane (its session id `orch-01` is present, so the flag never makes a running pane invisible); with the flag ON the same card shows `Pi · orch-01`. Pins the `features::show_pi_agent()` gate at the render seam (CLAUDE.md #9).
- **Does not assert:** the CLI surfaces (`agent-event`, `orchestrator setup`), the extension, the daemon protocol, or `AgentType::from_command` inference — none are flag-gated (business logic / protocol stay flag-free); the exact OFF fallback styling.
- **Platform coverage:** mac+linux+windows.

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
