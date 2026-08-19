# PRD #611: Pane scrolling is per-agent, and the deck should say so

**Status**: In progress
**Issue**: [#611](https://github.com/vfarcic/dot-agent-deck/issues/611)
**Builds on**: [#341](https://github.com/vfarcic/dot-agent-deck/issues/341) (the focused-pane scroll routing and the command-mode banner this PRD's notice is modelled on)
**Interacts with**: [#362](https://github.com/vfarcic/dot-agent-deck/issues/362) (wheel events routed by focus rather than by pointer), [#385](https://github.com/vfarcic/dot-agent-deck/issues/385) (which also extends the `mode/scroll` family)
**Priority**: Medium

## Problem Statement

Scrolling a pane with a two-finger trackpad swipe works on a `claude` pane and does nothing at all on a `codex` pane — "as if it's locked". Reported on macOS/Ghostty; the mechanism is agent-side and not OS-specific. `fn`+Up/Down (PageUp/PageDown) behaves the same way: works on claude, nothing on codex.

**No line of deck code is misbehaving.** Both branches of `scroll_focused_agent_pane` (`src/ui.rs:17719`) do exactly what they were designed to do — `PaneInput` plus child mouse reporting forwards the wheel into the child's mouse protocol, and everything else moves the deck's own vt100 scrollback. The two agents use *opposite* scrolling models, and the deck supports one of them well and the other not at all.

| | claude | codex |
|---|---|---|
| Mouse tracking enabled | all four: `1000h`, `1002h`, `1003h`, `1006h` | **none** — only `1004h` (focus reporting, not mouse) |
| Alternate screen (`1049h`) | — | **no** |
| Who owns the transcript | **the app** | **the terminal** |
| Responds to PageUp / `ESC[5~` | yes | **no** |

**claude is app-managed.** It requests mouse tracking precisely so it can scroll its own transcript. The deck forwards the wheel while in `PaneInput`, claude scrolls itself, and the user sees exactly what they expect. **codex is terminal-managed.** It requests no mouse events, so no terminal ever sends it any; it expects the *terminal* to hold scrollback — while contributing none. For such an agent there is nothing in the deck's buffer to move, by wheel or by key, in any mode, and both branches of the routing decision therefore no-op silently.

Three separate things are wrong with that state of affairs, and they are what the three milestones address: the docs describe pane scrolling as though it worked uniformly, the deck gives no feedback when a scroll cannot land so working-as-designed reads as broken, and the very state the feedback would key off — `mouse_mode_enabled` — is derived by a function with zero tests and three verified defects.

## Solution Overview

Nothing here tries to give codex scrollback the deck cannot manufacture (see **Rejected routes** below, and Out of Scope). The work is to make the deck honest about a per-agent reality, and to make the state it reasons about correct.

1. **Say it in the docs** (M1). `docs/keyboard-shortcuts.md` states the per-agent reality: app-managed agents own and scroll their own transcript, terminal-managed agents contribute no scrollback, and for the latter the deck has nothing to scroll.
2. **Say it on screen** (M2). When a scroll is *attempted* and cannot land, a brief dismissable notice appears in the pane, modelled on the existing `COMMAND MODE — Ctrl+D to type` banner (`src/ui.rs:14791`). Silence is what turns correct behaviour into a bug report.
3. **Derive the state correctly** (M3). `scan_mouse_mode` (`src/embedded_pane.rs:2722`) misses the combined SET form, lets an enable in a chunk beat a disable in the same chunk regardless of byte order, and cannot see a sequence split across a PTY read boundary. It is the input to the branch M2 reacts to, so its defects are the fastest way to make M2 lie.

## What was measured

A real 139 KB interactive codex session (v0.147.0) was captured with `script -qfc codex` and replayed through the repo's own `vt100` 0.16 at several parser heights, asking for 3 lines the way `FOCUSED_PANE_SCROLL_LINES` (`src/ui.rs:17684`) does. `vt100` clamps `set_scrollback` to the real buffer, so the readback is authoritative:

| parser height | codex buffer depth |
|---|---|
| **51 rows (what codex rendered for)** | **0 lines** |
| 44 rows | 182 lines |
| 24 rows | 187 lines |
| 12 rows | 225 lines |
| *control: 2000 plain streamed lines @ 51 rows* | *1950 lines* |

Codex sizes its scroll regions to the exact terminal height (164 DECSTBM calls: `ESC[1;50r`, `ESC[40;51r`, …) and repaints its whole transcript in place — 4971 cursor-position sequences against 95 newlines. **It never lets a line scroll off the top**, so there is nothing for any terminal to retain. The 182–225 lines at smaller heights are an artifact of the parser being shorter than codex believed the terminal to be; they do not occur in reality. The control row proves the parser and the measurement are sound — the same rig retains 1950 of 2000 plainly streamed lines at the same height that yields zero for codex.

**Codex's transcript is unscrollable in Ghostty too.** Scrolling up during a long codex session in a plain terminal reaches the content that was on screen *before* codex started — never an earlier part of the conversation. Codex does not use the alternate screen, so the pre-codex screen stays in the host terminal's scrollback above its UI. That is what appears to be "scrolling working outside the deck". A deck pane spawned for the agent has no pre-existing content, so nothing moves at all. This matters beyond the diagnosis: it is the objection every user will raise, so M1's prose has to pre-empt it.

### Rejected routes

Two ways of manufacturing scrollback for a terminal-managed agent were considered and rejected.

- **Retaining overwritten frames.** Codex repaints its whole transcript region rather than emitting new lines, so successive frames are near-duplicates. Paging back through them would page through redundant screenshots, not scrollback — the user would see the same conversation tail again and again with cosmetic differences, which is worse than the honest nothing they get today.
- **Handing the agent a taller virtual terminal than the pane.** Codex lays out against the height it is told, and sizes its DECSTBM regions to it. Give it more rows than the pane has and its composer renders off-screen, breaking the live view to buy history for a view nobody is looking at.

**A real fix can only happen upstream in codex** — filed separately, and out of scope here.

## Hypotheses falsified along the way

Recorded so nobody re-treads them.

1. **Codex enables mouse tracking and ignores the forwarded wheel.** No — it enables none of the four mouse modes, so the forwarding branch of `scroll_focused_agent_pane` is never taken for it in the first place. The only private mode it sets is `1004h`, focus reporting, which is not mouse reporting.
2. **Codex's pane is scrollback-starved at startup while claude's is not.** No — the startup measurement showed the *opposite* (codex 9 lines, claude 0). Both startup captures were unrepresentative of a working session, which is why the measurement above uses a real 139 KB interactive capture instead.
3. **Mouse-mode is seeded per agent type.** No — only the test seams take a bool (`EmbeddedPaneController::for_scroll_seam_with_focused_pane` and friends, `src/embedded_pane.rs:493`/`:558`/`:598`); the real spawn path always seeds `false` and lets `scan_mouse_mode` derive it from the child's own bytes.
4. **`ESC[5~` reaches codex and scrolls it.** No — codex ignores PageUp entirely, which is why the keyboard path fails for exactly the same reason the wheel path does and why the M2 message must not offer one as a workaround for the other.

**`~/.local/state/dot-agent-deck/deck.log` has no scroll or mouse instrumentation at all**, which is why none of this was diagnosable from artifacts and why all of it had to be measured by replaying a captured session through the parser.

## Scope

### In Scope

- **Docs that state the per-agent reality.** `docs/keyboard-shortcuts.md` — its mouse/scrolling preamble and its `### Scrolling back through a pane` section, both of which read as though scrolling worked uniformly — plus any other published page making the same claim. `docs/workspace-modes.md`'s "Reading a Pane in Command Mode" turned out to be the only other one.
- **A reactive on-screen notice** when a scroll is attempted on a pane that has nothing to scroll, on dashboard panes and the orchestrator pane, modelled on the command-mode banner's tiering and self-clearing behaviour.
- **Correct derivation of `mouse_mode_enabled`**: the combined SET form, disable-wins-within-a-chunk by byte position, and carry-over across PTY read boundaries — with the unit tests the function has never had.
- **Harness coverage per CLAUDE.md rule 4** for M2, extending the existing `mode/scroll/00X` family in `tests/mode_indication.rs` rather than adding a near-duplicate, with the `tests/CATALOG.md` entries revised alongside.
- **A synthetic regression fixture** for M3 — a byte stream reproducing the DECSTBM-plus-repaint pattern — rather than the real captured session, which contains actual conversation content.

### Out of Scope

- **Making codex's transcript scrollable.** That requires codex to use the alternate screen, to enable mouse tracking, or to bind a scroll key, and belongs upstream. Nothing the deck can do from the outside substitutes for it; see **Rejected routes** above for the two attempts that were considered and why each fails.
- **Any registry entry naming codex.** Excluded by a Decision below rather than merely unplanned — the condition is detected, the agent is not.
- **Scroll or mouse instrumentation in `deck.log`.** Its absence is recorded as a Risk because it is why this took a replay rig to diagnose, but adding it is a separate piece of work with its own noise/volume trade-offs.
- **Pointer-based routing of wheel events (#362) and the wider copy/selection work (#385).** They touch the same call site and the same test family; they are not folded in here.
- **Worker panes in an orchestration.** See the scope Decision below — the reactive design covers them for free if anyone ever scrolls one, and they are not a target.

## Decisions

- **The notice is reactive, not proactive.** It fires when a scroll is *attempted* and cannot land — never on pane focus or on spawn. A proactive variant nags every codex pane at startup, including the many times nobody meant to scroll, and a notice that appears when you were not asking anything is noise that teaches users to ignore it. This matches the banner philosophy already documented for command mode: *"a key that isn't bound to anything keeps it up, because that is the moment you most likely thought you were talking to the agent"* (`docs/keyboard-shortcuts.md:34`).
- **The notice must not swallow the keystroke.** It is visual only, cleared by a timer or by the next key *without consuming it*. The alternative — a dismissal that eats the keypress — would introduce a dropped-character bug in `PaneInput` as the price of explaining a no-op, which is a straight downgrade.
- **Detect the condition, not the agent name.** The trigger is "this pane's scrollback is empty AND the agent has produced substantial output", which is what distinguishes "nothing yet" (a claude pane one second after spawn) from "this agent never produces any". Codex is not special; its *rendering model* is, and a future repainting agent must be covered automatically. A registry entry naming codex would go stale the moment either that agent or another one changed, and it would silently under-report rather than fail loudly.
- **The message says what is true and offers no false alternative.** In particular it does **not** suggest PageUp — hypothesis 4 above establishes that PageUp fails for exactly the same reason the wheel does, so offering it would send the user to a second dead end and cost the message its credibility.
- **Scope is dashboard panes and the orchestrator pane.** Worker panes in an orchestration are mostly autonomous and are not a scroll target in normal use. With the reactive design this needs no exclusion logic: a pane nobody scrolls never shows the notice, so the scope statement describes where it will be *seen* rather than a branch that has to be written.
- **Experimental flag → no.** CLAUDE.md rule 9 question asked and answered by the user before work started: M2 ships **visible by default**. It is a small explanatory message that makes existing behaviour legible, not a new mode or a new surface with its own semantics, and shipping it flagged-off would hide the explanation from exactly the confused user it exists for — the one who has already concluded the pane is "locked" and who will never have set `experimental = true`. Consequences to hold to: **no** `features::show_*` wrapper is added in `src/features.rs`, no `experimental_enabled()` call appears on this path, and **no `graduate-*` follow-up issue** is filed at ship time.
- **CLAUDE.md rule 12 → the TUI↔daemon contract is unchanged; `PROTOCOL_VERSION` does not move and no `.breaking.md` fragment is needed.** All three milestones are strictly client-side. M1 is prose. M2 is render-seam and input-seam state living in the TUI's own `EmbeddedPaneController` and `UiMode` handling. M3 is a scanner over PTY bytes: `scan_mouse_mode` is called from `process_agent_output_chunk` (`src/embedded_pane.rs:2854`), which is deliberately *"shared between the local-PTY reader thread and the stream-backed I/O task so both backends produce identical render state from identical bytes"* — meaning the derivation happens on the client side of the wire in both the local and the remote case, from bytes the daemon already streams unchanged as `KIND_STREAM_OUT`. Neither `mouse` nor any scroll offset appears anywhere in `src/daemon_protocol.rs`; the daemon has never known whether a child requested mouse reporting and does not learn it here. So `PROTOCOL_VERSION` stays at `7` (`src/daemon_protocol.rs:206`), there is no same-wire/different-meaning break to version, and this is a **patch** bump. The rule 12 cross-version test remains worth running pre-PR as confirmation, not as a decision.

## Success Criteria

- `docs/keyboard-shortcuts.md` states the per-agent reality, and a user reading it can predict which of their agents will scroll and which will not, before trying.
- No published doc claims that pane scrolling works uniformly across agents.
- Attempting a scroll on a pane with nothing to scroll produces a visible, self-clearing explanation instead of silence — on the dashboard and on the orchestrator pane, by wheel and by key.
- The notice never consumes the keystroke that dismisses it: a character typed into a `PaneInput` pane while the notice is up reaches the agent.
- The notice does not appear on a freshly spawned pane that simply has not produced output yet, and does not appear on a pane whose scroll landed.
- No agent name appears in the trigger condition.
- `scan_mouse_mode` detects `ESC[?1000;1002;1006h`, honours a disable that follows an enable in the same chunk, and sees a sequence split across two PTY reads — each pinned by a unit test, where today the function has none.
- A claude pane, which emits all four mouse modes, still forwards the wheel to the agent in `PaneInput` after M3, and still scrolls the deck's own scrollback in command mode.
- `cargo test-fast` green per task; `cargo test-e2e` green pre-PR.

## Milestones

- [x] **M1 — Docs tell the truth about per-agent scrolling.** The mouse/scrolling preamble in `docs/keyboard-shortcuts.md` and its `### Scrolling back through a pane` section now state the app-managed versus terminal-managed distinction, that a terminal-managed agent leaves the deck nothing to scroll by wheel or by key in any mode, and why scrolling up in a plain terminal appears to work — the last of those under a new `#### How far back you can scroll depends on the agent` subsection that the other two link to. `docs/workspace-modes.md:160` carried the same uniform claim in "Reading a Pane in Command Mode" and is corrected alongside it. Keybinding tables, action names, remappability and the `Ctrl+PageUp`/`Ctrl+PageDown` note are all retained — this adds honesty, it does not delete reference material.
- [ ] **M2 — The deck says so on screen instead of no-opping silently.** A brief dismissable notice on an attempted scroll that cannot land, modelled on the `COMMAND MODE — Ctrl+D to type` banner (`src/ui.rs:14789`/`:14791`), under every Decision above: reactive, non-consuming, condition-detected, no false alternative, dashboard plus orchestrator pane, visible by default. Extends the `mode/scroll/00X` family in `tests/mode_indication.rs` with its `tests/CATALOG.md` entries.
- [ ] **M3 — Derive mouse state correctly (`scan_mouse_mode`).** Combined SET form detected; disable and enable resolved by byte position within the chunk rather than by pattern-list order; a carry-over that survives a PTY read boundary. Unit tests for all three plus the synthetic DECSTBM-repaint fixture, against a function that has zero tests today.

## Key Files

- `src/ui.rs:17719` — `scroll_focused_agent_pane`, the one decision a wheel event over the focused agent pane makes; its `else` branch is the one a terminal-managed agent always takes and the one M2 hangs off. `FOCUSED_PANE_SCROLL_LINES` (`:17684`) is the 3-line request the measurement above reproduced.
- `src/ui.rs:17749` — `handle_focused_pane_scroll_key`, the keyboard door to the same operation. Its doc comment already records that *"a claimed key with no focused embedded pane is a silent no-op, matching every other bound-but-inapplicable command in the deck"* — M2 is the argument that a claimed key with a focused pane and an empty buffer deserves better than that.
- `src/embedded_pane.rs:2722` — `scan_mouse_mode`, M3's subject, with its `contains_bytes` helper immediately below at `:2752`; `contains_bytes` is the single-chunk window scan that makes defect 3 structural rather than incidental.
- `src/embedded_pane.rs:2854` — the `scan_mouse_mode` call inside `process_agent_output_chunk`, the shared local-PTY / stream-backed path that the rule 12 Decision above rests on.
- `src/embedded_pane.rs:762` — `mouse_mode_enabled`, the accessor joining M3's derivation to M2's branch.
- `src/ui.rs:14789`/`:14791` — `BANNER_SUBTITLE` and `BANNER_LINE`, the `COMMAND MODE — Ctrl+D to type` banner M2 is modelled on, together with the tiering in `draw_centred_banner_line` (`:14797`) that degrades it on a narrow pane.
- `docs/keyboard-shortcuts.md:12-14` and `:111-129` — the two places M1 corrected, the second now carrying the `#### How far back you can scroll depends on the agent` subsection at `:117`; `:34` is the banner-philosophy sentence the reactive Decision cites, and the tone M2's own notice has to match.
- `docs/workspace-modes.md:160` — the same uniform claim in the "Reading a Pane in Command Mode" section, corrected in M1.
- `tests/mode_indication.rs` — host of the `mode/scroll` family, including `mode_scroll_001_mouse_wheel_routes_by_mode_and_child_mouse_state` (`:1084`) and `mode_scroll_002_keyboard_scroll_is_semantic_and_remappable` (`:1137`).
- `tests/CATALOG.md:3598-3629` — the `mode/scroll` entries; `mode/scroll/001`'s **Does not assert** line explicitly disclaims *"real terminal mouse-report decoding"*, which is the sentence that records why `scan_mouse_mode` is covered nowhere.
- `src/daemon_protocol.rs:206` — `PROTOCOL_VERSION`, the value the rule 12 Decision says does not move.

## Risks and Mitigations

- **M3 and M2 are coupled, and getting M3 wrong reproduces the exact symptom this PRD exists to explain.** `scan_mouse_mode` derives `mouse_mode_enabled`, which is the precise input to the branch M2 reacts to. An agent that enabled tracking via the combined `ESC[?1000;1002;1006h` form is mis-derived as *not* mouse-enabled, the deck wrongly takes the scrollback branch, and it produces **this same "locked" symptom for an agent that would otherwise have worked** — and, worse after M2 ships, an on-screen notice confidently explaining a rendering model the agent does not have. claude emits all four modes and is therefore the agent most exposed. *Mitigation*: land M3's unit tests before or with M2's trigger, and pin the claude-shaped case (all four modes, combined and separate forms) explicitly rather than only the codex-shaped one.
- **`deck.log` has no scroll or mouse instrumentation at all.** None of this was diagnosable from artifacts; it needed a captured session replayed through the parser at four heights plus a control. The same absence will make the *next* report of this shape equally expensive, and it makes an M3 mis-derivation in the field invisible. *Mitigation*: recorded here as a known gap rather than fixed in this PRD (see Out of Scope); if M3 turns out to need field evidence, a single debug line at the point `mouse_mode` flips is the cheapest possible addition and is where to start.
- **A notice that fires when it should not is worse than silence.** The "substantial output" half of the trigger is what separates a codex pane from a claude pane one second after spawn, and it is a heuristic. Too eager and every fresh pane accuses its agent of being unscrollable; too lazy and the codex pane the user is actually swiping at stays silent. *Mitigation*: pin both edges in the `mode/scroll` family — the fresh-pane negative case as well as the empty-buffer positive one — so a later tuning of the threshold cannot quietly lose either.
- **M1's forward reference to M2 can go stale.** M1 ships first and tells users the deck says so on screen; M2's wording is not written yet. *Mitigation*: M1 keeps that reference light and promises no specific banner text, and M2 revisits `docs/keyboard-shortcuts.md` if its wording needs it.
- **Users may read M1 as the deck getting worse.** Nothing about behaviour changes in M1; only the documentation stops overpromising. Someone who had not yet noticed that their codex pane does not scroll will learn it from the docs. *Mitigation*: the docs say plainly why, and point at the upstream requirement, rather than leaving it as an unexplained limitation.

## Open Questions

- **What exactly is "substantial output"?** A byte count since spawn, a count of parser feeds, or elapsed time with any output at all. A byte threshold is the most direct expression of "this agent has been running and has still produced no retained lines", but the right number is not established and a wrong one is invisible until someone reports the notice on a fresh pane.
- **How does the notice clear — timer, next key, or both?** The command-mode banner does both, and reusing its TTL keeps one vocabulary. What is genuinely undecided is whether an *unbound* key should keep this notice up the way it keeps the command banner up: the argument that justifies it there (you probably thought you were talking to the agent) does not obviously transfer.
- **Should a second scroll attempt re-arm the notice, or stay quiet after the first?** Re-arming on every swipe risks a notice that flickers under a user who is repeatedly swiping at an unresponsive pane — which is exactly the user it is for.
- **Does M3's carry-over live in the scanner or in the caller?** A carry-over buffer needs state that survives across `process_agent_output_chunk` calls for a given pane. Threading it through the shared function keeps both backends identical (which is why that function exists) but widens its signature; parking it on the pane is more local but easier to forget for one of the two backends.
- **Is `1004h`-only worth naming as its own signal?** Focus reporting without any mouse mode is, on the evidence here, a strong hint of a terminal-managed agent. It is deliberately *not* the M2 trigger — the Decision above detects the condition, not a mode fingerprint — but it may be a useful corroborating signal if the output heuristic proves hard to tune.
