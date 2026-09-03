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

Tier vocabulary (issue #502, CLAUDE.md rule 5): L1 tests run in the fast tier, `cargo test-fast`. L2 tests are split by whether they reach a **real agent** — **lane 1** is the 47 credential-free `tests/e2e_*.rs` files, `cargo test-e2e`, which CI runs on every PR; **lane-2** entries below are in one of the 24 files gated `#![cfg(all(feature = "e2e", feature = "e2e-live"))]`, run by `cargo test-e2e-live`, which **runs on a developer's machine and nowhere in CI** — no e2e test reaches a real agent on a runner, and no test credential is registered on this repository (the separately credentialed Codex issue-labeler is out of scope; see [`docs/develop/issue-labeling.md`](../docs/develop/issue-labeling.md)). Lane 2 is where the real-agent tests live and it is flaky-tolerant. Nothing here is a "pre-PR" tier any more — that obligation was removed with #502 — but this catalog is how you find the tests that cover what you changed, which rule 5 still requires you to run and to name. See [`docs/develop/e2e-lanes.md`](../docs/develop/e2e-lanes.md), and note that a lane-2 test whose preflight cannot be satisfied SKIPS and is counted as a pass.

Demo-reel eligibility marker: a trailing ` [reel]` on an entry's `##### <id> — <headline>` line opts that test into the PRD #180 demo reel (`.claude/skills/demo-reel-adapter`). Eligibility is **opt-in** — the default (no marker) is *not* eligible even for a PTY-attached test that records a cast. Mark a test only if it validates the feature **as a user actually runs and sees it** — a real agent genuinely spinning up (spawn → agent → work) — never a synthetic/stand-in test (`cat`, scripted echo, recorder stubs, terminal-probe, or synthesized hook events). The adapter includes a marked test in the reel only when it *also* has a cast and its source changed on the branch.

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
- **Asserts:** Ctrl+W opens the close confirmation; navigating from default Cancel to Close and confirming removes the card, and the focused card index stays within bounds.
- **Does not assert:** which card receives focus next (`dashboard/selection/*` covers selection-after-close).
- **Platform coverage:** mac+linux.

##### dashboard/pane/003 — The dashboard pane (tab 0) is never closable.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** `Ctrl+w` from the dashboard tab with no card selected is a no-op: neither the pane-scoped nor tab-scoped confirmation opens, no panic occurs, the dashboard remains rendered, and the tab count is unchanged.
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
- **Does not assert:** keyboard-driven selection movement (`dashboard/selection/*`); elapsed-time rollover behavior (the fixture uses one current instant for all three cards).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/006 — Card row shows `Dir:` (working directory basename), `Last:` (elapsed since last activity), `Tools:` (tool count), `Prmt:` (latest user prompts).
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** an over-long working-directory basename renders with all four fields retained; `Dir:` owns the full inner content width and truncates with an ellipsis immediately before the right border, while `Last:` / `Tools:` live in the bottom border. A second 14-column render proves a newline in `abc\ndef` costs no terminal cell, so all six visible prompt cells render as `abcdef` without an ellipsis.
- **Does not assert:** the card-stats degradation thresholds (covered by `dashboard/card-stats/002` and `/004`); elapsed-time rollovers beyond the fixture's stable one-hour display.
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/007 — A Pi pane's card renders the Pi agent-type identity (PRD #201 M2.2).
- **Layer:** L1 (ratatui `TestBackend` + `insta`-style buffer text assertion).
- **Agent:** none (a fixture `SessionState` with `agent_type = AgentType::Pi` and no display name).
- **Asserts:** a live Pi session with no friendly name renders its card title in the `<agent-type> · <session-id>` form showing the Pi identity (`Pi · orch-01`) — with NO experimental flag touched, since the Pi surface ships visible by default (PRD #201 reverses Design Decision #8, un-gating Pi); the fixture's cwd basename and session id carry no capital `Pi`, so the match pins the agent-type Display specifically. The card must NOT show `ClaudeCode` / `OpenCode` / `No agent` — a plain `pi` pane is first-class, not "No agent".
- **Does not assert:** the status badge color (`status/badge/001`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/008 — Agent cards retain their registry-colored type badges even when they have friendly display names (PRD #20 M7, review finding 9).
- **Layer:** L1 (ratatui `TestBackend` + color-aware `insta` snapshot).
- **Agent:** none (synthetic Claude Code, OpenCode, Pi, and Codex `SessionState` fixtures, including friendly display names).
- **Asserts:** the unnamed Codex card and named cards for all four shipped agents contain their registry identity; every badge cell uses that registry entry's `badge_color`; complete color-aware buffers are snapshotted.
- **Does not assert:** wrapper event delivery or real Codex execution (covered by `codex/wrap/001` and `codex/live/001`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/009 — A history-only session is visibly distinct from a live writable session (PRD #20 M4).
- **Layer:** L1 (ratatui `TestBackend` + inline `insta` snapshot).
- **Agent:** synthetic Codex `AgentEvent` fixtures, one live and one history-only.
- **Asserts:** the history-only card visibly contains a history marker and its numeric input shortcut carries `Modifier::DIM`; the live contrast card has neither treatment.
- **Does not assert:** delivery feedback or daemon send results (covered by `prompt/pane-input/004`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/010 — A pane keeps exactly one card when a hook reports on it without an `agent_id` (issue #398).
- **Layer:** L1 (in-process `AppState::apply_event` + ratatui `TestBackend` buffer text assertion).
- **Agent:** none (a tagged spawn placeholder plus one synthetic untagged `WaitingForInput` `AgentEvent`).
- **Asserts:** after an `agent_id: None` event lands on a pane that already carries a tagged session, exactly one session claims that `pane_id`, that session carries the reported `WaitingForInput` status, and the rendered card grid contains exactly one status badge. Before #398 the untagged event minted a second session, so the deck drew two cards for one pane and `build_pane_status` picked between their statuses by `HashMap` iteration order.
- **Does not assert:** that the tagged session keeps its accumulated history (the `pre_f9_hook_with_no_agent_id_*` unit tests in `src/state.rs` pin that half); the `WaitingForInput` command-entry carve-out that reads the collision-hardened join (`orchestration/lock/007`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/011 — A multi-byte-Unicode `session_id` truncates on a char boundary instead of panicking the whole deck (issue #574).
- **Layer:** L1 (ratatui `TestBackend`).
- **Agent:** none (a hook event carrying a non-ASCII `session_id`, replayed through `AppState::apply_event`).
- **Asserts:** `apply_event` keys the session map on a producer-supplied `session_id` of nine `α` characters verbatim (no validation between the hook socket and the render), and rendering that card in a three-card deck draws ALL THREE cards — the poisoned card's id shortened to the longest char-boundary prefix within the 11-byte title budget plus an `…`, its two healthy neighbours untouched. Repeats the render across 2-, 3- and 4-byte characters at every ASCII offset that puts byte 11 mid-character. Before the fix `&session.session_id[..11]` panicked inside the render loop, so every frame died and the whole deck went down with it.
- **Does not assert:** any rejection or sanitisation of the id upstream of the render (the id is stored verbatim by design); the OSC-8 hyperlink status line, which is the same defect on a different string (`mouse/hyperlink/001`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/012 — A hostile `display_name` on the hook socket is scrubbed and clamped before it can reach a card (issue #670).
- **Layer:** L1 (in-process `AppState::apply_event` + ratatui `TestBackend` via `render_card_grid_to_buffer`).
- **Agent:** none (four synthetic hook events, two of them carrying a hostile `display_name` in `event.metadata`).
- **Asserts:** a name carrying ESC, NUL, CR/LF, DEL and a `U+202E` right-to-left override is stored scrubbed and trimmed, and a 400-character one is stored clamped to `agent_pty::DISPLAY_NAME_MAX_LEN` bytes on a character boundary with a trailing `…`; rendering all four cards through the deck's own `ui.display_names` → `SessionState.display_name` resolution draws four status badges, leaves both neighbouring titles intact, and puts no control character or bidi override in ANY buffer cell. Before the fix the ingest applied only `.filter(|n| !n.is_empty())`, and `U+202E` was measured reaching cell (30, 9) — where a flush writes it to the real terminal and it reorders the text around it.
- **Does not assert:** `ui.display_names`, the card's *preferred* name source, which is hydrated from the daemon's `is_valid_display_name`-gated `AgentRecord.display_name` over the attach socket and is a separate path with its own audit — deliberately out of scope here, and tracked on the fork this defect was reported from as `prageethw/dot-agent-deck#562`; the title's char-vs-display-column fit budget (`truncate_styled_segments`, issue #357).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/013 — A declared agent identity fills only the initial `No agent` card badge (issue #308).
- **Layer:** L1 (ratatui `TestBackend` buffer-text assertions).
- **Agent:** synthetic neutral and ClaudeCode `SessionState` fixtures with a declared Codex identity.
- **Asserts:** a neutral session plus `Some(Codex)` renders `Codex · reviewer` and no `No agent`; after that session reports `ClaudeCode`, the observed identity renders `ClaudeCode · reviewer` with no stale `Codex` label. A neutral session with no declaration retains the pre-#308 `No agent` baseline.
- **Does not assert:** declaration propagation through config, spawn, or daemon dispatch; those end-to-end paths are covered by `codex/spawn/009` and `codex/spawn/011`.
- **Platform coverage:** mac+linux+windows.

#### dashboard/stats

##### dashboard/stats/001 — A narrow stats bar keeps the `tools` total and spends no width on a per-agent-type breakdown.
- **Layer:** L1 (in-process `AppState::aggregate_stats` + ratatui `TestBackend` stats render).
- **Agent:** none (22 synthetic sessions: 14 Claude Code + 8 Codex).
- **Asserts:** rendered at 60 columns — the width the bar gets from the left dashboard column when panes are open — the bar still shows `22 active` and the `tools` total, and contains no `ClaudeCode` / `Codex` per-type segments. The breakdown (PRD #20, review finding 10) cost ~30 columns at this width and silently clipped the `tools` total off the right edge; the type information stays on the cards, which each carry a registry-colored badge.
- **Does not assert:** priority-ordered truncation for bars too narrow even for the status counts, or exact badge colors.
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

##### dashboard/density/005 — A Spacious idle card shows the flashing-dot indicator over ordinary card content, the same as Normal and Compact (issue #519).
- **Layer:** L1 (ratatui `TestBackend`, buffer inspection + `insta`).
- **Agent:** none.
- **Asserts:** an `Idle` session rendered at Spacious density keeps its ordinary card content — prompt line, dir line, agent-type badge — and carries the `Idle` status badge whose leading dot is inked at the flash-on tick and blank at the flash-off tick, matching the indicator Normal renders for the same session. Pins the fallback that idle cards use in **every** density now that issue #519 removed the Spacious-only ASCII-art overlay, which used to `Clear` this content and paint generated frames over it.
- **Does not assert:** the removed art path itself (deleted, with no seam left to drive); the flash period, covered by the `flash_dot` unit test.
- **Platform coverage:** mac+linux+windows.

#### dashboard/grid

##### dashboard/grid/001 — A deck too short for its cards in one column widens to a second rather than painting a subset (issue #588).
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none (seven synthetic idle role-card fixtures, named for the roles in the report).
- **Asserts:** the reported geometry — seven roles on a 90x27 deck, whose 25 usable rows cannot hold seven single-column Compact cards (7x5 = 35) — paints all seven role names, draws all seven cards (corner glyphs counted in the buffer, so the count is of what reached the screen), and does so in two columns. Control: the same seven on a 90x60 deck, which one column always fitted, still paints all seven **in one column** — widening is spent only on completeness, never gratuitously.
- **Does not assert:** the `(↑a ↓b)` overflow indicator (covered by `dashboard/grid/003`); the `UiState::columns` navigation agreement (`dashboard/grid/002`); the density ladder's own thresholds (`dashboard/density/*` and the `choose_grid_layout_density_*` unit tests); the stats-bar row the grid reserves but does not draw.
- **Platform coverage:** mac+linux+windows.

##### dashboard/grid/002 — The column count left/right card navigation reads always equals the column count drawn (issue #588).
- **Layer:** L1 (ratatui `TestBackend`, buffer inspection).
- **Agent:** none.
- **Asserts:** across six deck geometries — one column, one column tall enough for every card, a deck escalated to two columns, a deck already at two from width, three columns from width alone, and a deck nothing fits — `UiState::columns` after the render equals the number of card top-border glyphs counted in the rendered buffer. The count is derived from the drawn cells, so it is an independent witness rather than a re-run of the layout code. Guards the desync a render-path-only fix produces: the grid looks right and arrow-key movement steps somewhere the user is not looking.
- **Does not assert:** what a left/right keypress does with that count (the dashboard's own `h`/`l` switch tabs — this pins the value, not its consumers); the selection-follows-scroll behaviour (`dashboard/selection/*`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/grid/003 — When no layout fits, the deck title counts the cards it is not showing (issue #588).
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** seven roles on a 90x12 deck, too small at every column count 90 columns allows, genuinely overflows (two of seven cards drawn) and carries `(↓5)` in a title that still names all `7 session(s)`; the column count is **not** escalated, since narrowing every card buys nothing once completeness is out of reach. Selecting the last card scrolls the window down and flips the marker to `(↑5)`. Closes the asymmetry the issue names — the scheduled-tasks header signalled its hidden rows while the card grid rendered its title plain.
- **Does not assert:** which key scrolls the grid (selection movement does, via `dashboard/selection/*`); the indicator's format, pinned by the `scroll_indicator_reports_only_what_is_hidden` unit test shared with the scheduled-tasks modal.
- **Platform coverage:** mac+linux+windows.

#### dashboard/card-stats

##### dashboard/card-stats/001 — A wide card renders its full Last/Tools stats at the bottom-right border.
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none (synthetic Thinking-session fixture).
- **Asserts:** a comfortably wide live card right-aligns `Last: 1h  Tools: 14` in its bottom border, and neither counter appears on an inner content row; the complete character buffer is snapshotted. A wide placeholder `No agent` card also retains its full Last/Tools counters in the bottom border.
- **Does not assert:** narrow-width degradation (covered by `/002` and `/004`); border title colors.
- **Platform coverage:** mac+linux+windows.

##### dashboard/card-stats/002 — A 20-column card degrades its stats label without damaging border corners.
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none (synthetic Thinking-session fixture).
- **Asserts:** with 18 usable bottom-border cells, the card selects `1h · 14 tools`, preserves both bottom corner glyphs, and renders no dedicated stats content row; the complete character buffer is snapshotted.
- **Does not assert:** widths below the shortest form or the complete transition sweep (covered by `/004`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/card-stats/003 — Crossing the former 60-column breakpoint is structurally inert.
- **Layer:** L1 (ratatui `TestBackend`, comparative buffer inspection).
- **Agent:** none (the same synthetic session rendered on both sides of the old breakpoint).
- **Asserts:** real Normal-density card renders with 59 and 61 inner columns expose the same `Dir:` / `Prmt:` / `Last:` / `Tools:` labels and keep those labels on the same rows.
- **Does not assert:** production density selection, because the available L1 render seams require the caller to supply a density; exact horizontal truncation or full-buffer equality, since changing width legitimately changes available text cells.
- **Platform coverage:** mac+linux+windows.

##### dashboard/card-stats/004 — The stats-label degradation ladder transitions at exact display widths.
- **Layer:** L1 pure-data unit test over the hidden-public label selector.
- **Agent:** none.
- **Asserts:** the reference input selects no label below 9 cells, `2m · 14` from 9, `2m · 14 tools` from 15, and `Last: 2m  Tools: 14` from 21 onward, with both sides of the exact transitions pinned. Property sweeps over `1h 5m`/1234, a six-digit tool count, empty elapsed text, and Unicode/combining text prove every result fits its display-column budget and is the first, widest fitting form.
- **Does not assert:** ratatui title placement or styling (covered by `/001` and `/002`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/card-stats/005 — A real interactive Haiku card keeps its height while opening its pane narrows the card and degrades the bottom-border counters. [reel]
- **Layer:** L2 PTY-attached (the real `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness, with recording enabled for a `full-stream.cast`).
- **Agent:** REAL interactive Claude Code on `claude-haiku-4-5-20251001`, with onboarding/project trust seeded and `--allowedTools Bash`; no `-p`. A second real client on the observer's daemon performs the ordinary Ctrl+N flow and types the prefix-only prompt after Claude's native editor becomes ready; the recorded client observes the live card and later attaches that same daemon pane on demand.
- **Asserts:** the sentinel response and native Thinking/Working/Idle plus Bash hook prove the genuine spawn → agent → work path; at one fixed 68×16 recording size, the unattached card shows a nonzero, right-aligned full `Last: … Tools: …` label only in its bottom border, then attaching the real pane narrows the dashboard and selects the shorter `… · … tools` rung while preserving matching-weight intact bottom corners (`└`/`┘` or `┗`/`┛`), the tool count, the `Dir:`/`Prmt:`/`Bash` row offsets, and card height.
- **Does not assert:** exact Claude prose beyond the discovered sentinel filename; exact elapsed-time text; multiple cards or density changes caused by terminal height.
- **Platform coverage:** mac+linux.

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
- **Asserts:** with `selected_index = None` (inactive, nothing armed), dispatching `Action::CloseSelected` opens no confirmation, issues no `close_pane` call, and removes no session — it does NOT arm or close card 0. Encodes the PRD invariant (inactive = nothing armed) alongside `dashboard/pane/003`.
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
- **Asserts:** with a Mode tab active and `selected_index == None`, dispatching `Action::CloseSelected` opens confirmation and `ConfirmCloseSelected` closes that tab (tab count drops back to the lone Dashboard); the same holds for an active Orchestration tab. Bounds the `dashboard/selection/012` no-op gate: the inactive-selection guard suppresses an unarmed dashboard CARD, but an active Mode/Orchestration TAB remains a valid confirmation target. Regression for the PR #151 e2e failure `e2e_render_contract::layout_002`.
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

##### dashboard/selection/020 — Enter on a live card whose pane is not wired locally attaches it on demand instead of deleting the card.
- **Layer:** L1 (`dispatch_action(Action::Focus, …)` against a mock controller whose `focus_pane` fails until `try_hydrate_pane` attaches the pane).
- **Agent:** none.
- **Asserts:** Enter attempts the on-demand attach exactly once, the session survives, and the deck enters `PaneInput`. Pre-fix the failed `focus_pane` was read as "stale card" and the LIVE session was removed — only the digit-jump path (`dashboard/selection/003`) carried the PRD #127 guard.
- **Does not assert:** the real `list_agents`/attach round-trip behind `EmbeddedPaneController::hydrate_pane` (L2 territory); which tab the card belongs to.
- **Platform coverage:** mac+linux+windows.

##### dashboard/selection/021 — Enter still removes a card whose pane the daemon genuinely does not have.
- **Layer:** L1 (same harness, mock reports the pane is not attachable).
- **Agent:** none.
- **Asserts:** the attach is still attempted, the session is removed, and the deck does not enter `PaneInput` — the fix must not turn a genuinely dead card into an undeletable one.
- **Does not assert:** the status-message wording.
- **Platform coverage:** mac+linux+windows.

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

##### dashboard/filter/003 — `type:<agent>` filters mixed sessions by registry identity and composes with ordinary text (PRD #20 M9).
- **Layer:** L1 (in-process `filter_sessions` pure-data matrix).
- **Agent:** none (synthetic Claude Code, OpenCode, Pi, and Codex session states).
- **Asserts:** `type:claude`, `type:claudecode`, `type:opencode`, `type:pi`, and `type:codex` each select only that agent; type matching is case-insensitive; a remaining text term is ANDed with the type; conflicting `type:codex type:claude` constraints use true AND semantics and yield no matches; an unknown type yields no matches; plain id/cwd/status/display-name matching is unchanged.
- **Does not assert:** the rendered dashboard result (covered by `dashboard/filter/004`).
- **Platform coverage:** mac+linux+windows.

##### dashboard/filter/004 — Typing `type:codex` in the `/` search visibly narrows the dashboard to Codex cards (PRD #20 M9).
- **Layer:** L1 (in-process keyboard handlers + ratatui `TestBackend` dashboard render).
- **Agent:** none (synthetic Claude Code, OpenCode, Pi, and Codex session states).
- **Asserts:** `/` enters filter mode; typing `type:codex` through the filter input leaves the Codex card visible and hides every non-Codex card in the rendered buffer.
- **Does not assert:** accepting or clearing the filter (covered by `dashboard/filter/002` and `dashboard/selection/004`).
- **Platform coverage:** mac+linux+windows.

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
- **Asserts:** `insta` file snapshot of the overlay buffer; the Ctrl+D row describes a bidirectional command-mode / pane-input toggle rather than the one-way destination `Command mode (dashboard)`.
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

##### status/agent-event/004 — A typed synthetic Codex wrapper lifecycle updates one dashboard card through active, error, recovery, and idle states (PRD #20 M7).
- **Layer:** L1 (in-process `SyntheticAgent<AgentType::Codex>` events applied through `AppState::apply_event`).
- **Agent:** synthetic Codex wrapper identity.
- **Asserts:** the same Codex session remains one card and its observable status follows Thinking → Error → Thinking → Idle while retaining `AgentType::Codex`.
- **Does not assert:** stdout classification or socket transport (covered by `codex/wrap/001`).
- **Platform coverage:** mac+linux+windows.

##### status/agent-event/005 — A respawned agent whose first event is NOT a `SessionStart` still retires the previous card, so one pane keeps one card.
- **Layer:** L1 (two `SyntheticAgent` generations on one pane, applied through `AppState::apply_event`).
- **Agent:** synthetic Pi identity (the only shipped agent with no `SessionStart`).
- **Asserts:** after a `clear = true` respawn mints a new `agent_id`, the outgoing generation's card is retired by the incoming generation's first `agent-event` (`Thinking`), leaving exactly one session on the pane, carrying the new `agent_id`.
- **Does not assert:** repeated respawns after the initial spawn-time placeholder → first-respawn transition (`status/supersede/005` covers the stable producer id reused by later generations); the orchestration deck's rendering of the duplicate (the unreachable-highlight consequence is pinned by the `sync_and_derive_selection` unit tests in `src/tab.rs`); the Pi extension's own state mapping (TS unit tests).
- **Platform coverage:** mac+linux+windows.

##### status/agent-event/006 — A delayed event from the OUTGOING agent does not retire the incoming agent's live card.
- **Layer:** L1 (out-of-order `AgentEvent` timestamps applied through `AppState::apply_event`).
- **Agent:** synthetic Pi identity.
- **Asserts:** once the incoming generation has established its card, an older-timestamped event from the previous `agent_id` leaves that live card intact — the monotonicity guard that makes retiring on a non-`SessionStart` event safe.
- **Does not assert:** that the stale event is dropped entirely (it may still surface its own card; what must hold is that the LIVE card survives).
- **Platform coverage:** mac+linux+windows.

#### status/supersede

##### status/supersede/001 — A real scheduler agent supersedes its friendly `No agent` placeholder without creating a duplicate card or losing the task name.
- **Layer:** L1 (in-process scheduler placeholder and real `SessionStart` events applied through `AppState::apply_event`).
- **Agent:** none (synthetic ClaudeCode identity for the real hook).
- **Asserts:** a `Some(agent_id)` real session replaces the same-pane `None` placeholder even when its producer timestamp is older, leaving exactly one live card that inherits the placeholder's friendly display name.
- **Does not assert:** the rendered card grid or daemon hook transport (`scheduler/live/004` covers the PTY-attached surface).
- **Platform coverage:** mac+linux+windows.

##### status/supersede/002 — Replacing a session identity on an armed pane leaves the close-confirm target vanished rather than retargeting the replacement.
- **Layer:** L1 (in-process state replacement through `AppState::apply_event`).
- **Agent:** none (synthetic ClaudeCode generations).
- **Asserts:** after a different-agent `SessionStart` takes over the same pane, the armed session id is absent, the replacement id remains, and only one card owns the pane, which makes stable-id close resolution return vanished.
- **Does not assert:** modal rendering or actual close dispatch (`prompt/close-confirm/005` covers the PTY-attached behavior).
- **Platform coverage:** mac+linux+windows.

##### status/supersede/003 — A delayed outgoing `SessionEnd` cannot erase the live replacement card from its pane.
- **Layer:** L1 (in-process terminal event applied through `AppState::apply_event`).
- **Agent:** none (synthetic ClaudeCode generations).
- **Asserts:** after live agent B establishes a card, a newer-stamped `SessionEnd` from outgoing agent A on the same pane leaves B's card present instead of leaving the live pane with zero cards.
- **Does not assert:** daemon hook transport, placeholder restoration for the ending session, or rendered card layout.
- **Platform coverage:** mac+linux+windows.

##### status/supersede/004 — Reordered same-session activity cannot weaken the outgoing-straggler guard.
- **Layer:** L1 (in-process reordered events applied through `AppState::apply_event`).
- **Agent:** none (synthetic Pi generations).
- **Asserts:** live agent B established at T=30 survives an outgoing agent A straggler at T=20 even after B's delayed same-session event at T=10 is delivered between them.
- **Does not assert:** daemon socket task scheduling or that the outgoing straggler is dropped entirely; only the live card's survival is required.
- **Platform coverage:** mac+linux+windows.

##### status/supersede/005 — A repeated Pi respawn refreshes the card identity carried under the pane-derived stable producer session id.
- **Layer:** L1 (successive in-process Pi generations applied through `AppState::apply_event`).
- **Agent:** none (synthetic Pi generations using the production `{pane_id}-session` construction).
- **Asserts:** after Pi agent 2 establishes the stable card and Pi agent 3 reports through the same producer session id, exactly one card remains and carries agent 3's identity.
- **Does not assert:** close-target retargeting across stable-key generations. Close confirmation arms on the session id alone (`CloseTarget::Session`) and resolves it by direct key lookup; because Pi reuses `{pane_id}-session` across respawns, that target remains resolvable after a generation change and confirmation can act on whichever generation currently occupies the pane. This behavior predates #284 and is neither introduced nor worsened by it: before the fix the key resolved to a stale corpse entry, after it resolves to the live replacement, and in both cases it maps to the pane's current card. The #284 identity refresh is a prerequisite for fixing this properly by arming on the generation (session id plus agent id), because the refreshed `agent_id` can now expose a generation change that the pre-fix stale `pi-agent-2` identity would have concealed. That fix belongs at the arm/resolve seam (`CloseTarget` / `resolve_close_plan`), while `prompt/close-confirm/005` remains the close-flow proof. This test also does not assert the initial spawn-time placeholder → first-respawn transition (`status/agent-event/005`), socket transport, or rendered card history.
- **Platform coverage:** mac+linux+windows.

##### status/supersede/007 — A Pi card that already exists inherits the friendly name when its newer status retires the scheduler placeholder.
- **Layer:** L1 (out-of-order scheduler placeholder and Pi events applied through `AppState::apply_event`).
- **Agent:** none (synthetic scheduler placeholder and Pi identity).
- **Asserts:** an older first Pi frame initially coexists with the friendly scheduler placeholder, then a newer Pi status retires it and leaves one Pi card carrying `morning-digest`.
- **Does not assert:** scheduler dispatch, daemon socket delivery, or rendered card layout.
- **Platform coverage:** mac+linux+windows.

##### status/supersede/008 — A pane's friendly name survives a session END, so a `clear = true` respawn's replacement card is still named for its role.
- **Layer:** L1 (a spawn-time placeholder, a first generation, its `SessionEnd` and a replacement `SessionStart` applied through `AppState::apply_event`).
- **Agent:** none (synthetic generations under two distinct registry agent ids).
- **Asserts:** after the outgoing agent's `SessionEnd` and the replacement's own `SessionStart`, exactly one card remains on the pane and it still carries `morning-digest`.
- **Does not assert:** the delegate/respawn machinery itself (`orchestration/delegate/022`, `/023`), the TUI-side `ui.pane_display_names` mirror that independently rescues the name on every non-dispatched orchestration path, or rendered card layout.
- **Platform coverage:** mac+linux+windows.

#### status/shell-activity

##### status/shell-activity/001 — The process-table primitive finds a real, detached grandchild process as a descendant and reports its no-controlling-tty / session-leader / argv / session-id facts correctly (PRD #386 M1).
- **Layer:** L1.
- **Agent:** none (a real `sleep` process, spawned and `setsid()`'d by the test itself — no agent involved).
- **Asserts:** `process_table()` enumerates the machine's processes and `descendants()` finds a real grandchild of the test process (spawned on pipes, detached via `setsid()`) as a descendant of the test's own pid; the found entry reports no controlling terminal, session-leader true, its full argv (a uniquely marked command line), and a session id differing from the test process's own (checked independently via `libc::getsid(0)`) — the real-process proof that `getsid` reads what the M2 discriminator's load-bearing condition assumes it reads. On Windows, `process_table()` returns `None` (no process-enumeration backend exists there — same contract as `foreground_pgid`).
- **Does not assert:** that this primitive is wired into any pane's status, that the discriminator (`descendant_shell_activity`, `status/shell-activity/003`) classifies anything as "busy", or anything about a real agent pane — this is a mechanism test only, included so a later failure localises. PRD #370's failure was exactly a correct mechanism test attached to nothing; this test proves nothing about the shell-activity feature working end to end on its own.
- **Platform coverage:** mac+linux (real-process assertion) + windows (the `None` contract).

##### status/shell-activity/002 — The descendant walk terminates instead of looping forever when a synthetic process table contains a `ppid` cycle (PRD #386 M1).
- **Layer:** L1.
- **Agent:** none (a hand-built synthetic table — no real processes, no `ps` involved).
- **Asserts:** `descendants()` called against a table where a `ppid` cycle loops back to the root pid returns within a bounded timeout (not an infinite loop) and reports each reachable non-root descendant exactly once, correctly excluding the root pid even though the cycle links back to it.
- **Does not assert:** anything about how a real `ps` sample could produce such a cycle, or the discriminator/classification logic — purely a termination/dedup guarantee on the walk.
- **Platform coverage:** mac+linux+windows (pure data, no OS process calls).

##### status/shell-activity/003 — The structural session-id discriminator classifies the measured Bash-tool descendant as busy and every measured confounder as idle, unchanged when every process has no controlling terminal (PRD #386 M2).
- **Layer:** L1.
- **Agent:** none (hand-built fixture tables reproducing the `getsid`/`ps` captures from `.dot-agent-deck/386-argv-notes.md` and the PRD — no real processes, no `ps` involved).
- **Asserts:** `descendant_shell_activity(table, root_pid, shapes)`, called with the argv cross-check disabled (`shapes: &[]`), returns `Some(true)` for a table containing the measured Bash-tool descendant (its own POSIX session, differing from the agent's) alongside the agent's five measured long-lived children (`context7`, `task-master`, `engram`, `pysemgrep`, `caffeinate`, all in the agent's own session), and `Some(false)` for the same table with the Bash-tool descendant removed — pinning the claim that the session-id test alone, without any argv help, already excludes every measured confounder. A third and fourth case rebuild both tables with every row, the agent included, reporting no controlling terminal (the CI/container shape measured in `386-argv-notes.md` §5) and assert classification is unchanged — the direct regression test for a bare no-controlling-terminal fallback collapsing where the agent itself has no terminal either.
- **Does not assert:** the argv cross-check itself (`shapes` is empty throughout — that path is exercised by a real agent in `status/shell-activity/005`, the M6a rot canary), that this primitive is wired into any pane's status, or anything about a real agent pane. One measured field is a documented derivation rather than a direct reading: the fixture's `task-master`/`pysemgrep` session ids are inferred from their measured `ps` `pgid` (which coincides with `sid` throughout this tree), not from an explicit `getsid` line in the notes, which list only three of the five confounders by name.
- **Platform coverage:** mac+linux+windows (pure data, no OS process calls).

##### status/shell-activity/004 — `RunningAgent::shell_foreground_busy` (via the registry's `shell_foreground_busy_snapshot` seam) flips idle → busy → idle for a real, detached, pipes-only descendant of a real PTY pane's shell (PRD #386 M3).
- **Layer:** L1 (real PTY pane spawned through `AgentPtyRegistry`; real `setsid()`'d `ps`-visible child — no daemon, no hooks).
- **Agent:** none (a real `/bin/sh` pane spawned by the test, whose script launches a real `python3`-then-`/bin/sleep` child that `setsid()`s itself — no AI agent involved).
- **Asserts:** the pane is spawned with `agent_type: Some(AgentType::ClaudeCode)` — load-bearing, since `shell_tool_shape_key` selects `CLAUDE_BASH_TOOL_SHAPE` only for that agent kind and `shell_foreground_busy_snapshot` filters the shapes it is handed down to `&[]` for any other kind before the scan ever sees them; without this the test's `&[CLAUDE_BASH_TOOL_SHAPE]` argument would be discarded before reaching the classifier. With that in place, `shell_foreground_busy_snapshot(&[CLAUDE_BASH_TOOL_SHAPE])` reads idle for the pane before the detached child appears, busy while it lives, and idle again once it is killed — the rising *and* falling edge, so an implementation that only sets busy and never clears would still fail here. Independently confirms, via `process_table()` + `descendants()` on the real sample, that the found descendant has no controlling terminal, is its own session leader, and carries a POSIX session id different from the pane's own shell — the exact topology (on pipes, off the PTY entirely, in its own session) PRD #370's `tcgetpgrp`-based test could never produce, because #370 typed its command directly into the pane's PTY, keeping the child in the pane's own foreground process group. The detached child's argv is crafted to carry the measured Bash-tool shape (`shell-snapshots/snapshot-` and `&& eval `), and — because the pane now carries the Claude agent kind — the argv cross-check is genuinely exercised against a real process rather than only the fixture strings in `status/shell-activity/003`.
- **Does not assert:** anything about a real AI agent's Bash tool, the daemon's poll task (`run_shell_activity_monitor`), or the `pane_hook_session_id` gate — this is the pane-primitive layer only. `status/shell-activity/005`–`007` (M6a/b/c) carry the burden of proving the signal fires for a real agent.
- **Platform coverage:** mac+linux (real-process assertion; not run on Windows, where `process_table()` is unconditionally `None`).

##### status/shell-activity/005 — A real interactive Haiku Claude agent's Bash-tool call trips the descendant scan: the daemon's shell-activity monitor synthesizes a `ShellBusy` broadcast event for the pane (PRD #386 M6a, the rot canary).
- **Layer:** L2, PTY-attached, real agent (drives the actual `dot-agent-deck` binary, which lazily spawns its own daemon; no synthetic hook, no fabricated `SessionStart`, no hand-set `pane_id`).
- **Agent:** a real interactive `claude --model claude-haiku-4-5-20251001 --allowedTools Bash` pane, spawned through the normal Ctrl+N new-pane flow with per-folder trust pre-seeded, exactly as a user would drive it.
- **Asserts:** after a directive prompt (naming a uniquely-named sentinel fixture file so the wording survives LLM phrasing variance) drives the agent to make exactly one Bash tool call running `ping -c 20 127.0.0.1 > /dev/null` — real, non-blocked foreground work lasting ~19-20s, since Claude Code blocks long `sleep` at the tool layer and emits no `ToolStart` for it — the test first confirms the native `ToolStart`/`Bash` hook event fired (precondition), then asserts, over a live `SubscribeEvents` connection (never against the rendered grid), that the daemon's `run_shell_activity_monitor` poll synthesizes a `ShellBusy` `AgentEvent` carrying this pane's `pane_id`, within 15s of `ToolStart` — comfortably inside the ~19-20s command window. The badge is never the pass/fail signal: the pane already reads `Working` from `ToolStart` alone regardless of whether this mechanism fires at all, which is exactly how PRD #370's mechanism shipped green while dead. A miss here means either Claude Code stopped `setsid`-detaching its Bash-tool child (a total, silent false negative) or the descendant scan is not reaching this pane in production — never a fixture-only artifact, since nothing here is a fixture.
- **Does not assert:** the falling edge (`ShellIdle` once the command completes — that is `004`'s job against a stand-in, not repeated here against a real agent), the >120s-cap user-visible badge scenario (`status/shell-activity/006`, M6b), or no-false-positive-at-idle (`status/shell-activity/007`, M6c). The soft sentinel-content check on the model's final reply is logged, not gating — matching the model's free-text reply is more phrasing-sensitive than the two typed events the test actually gates on.
- **Platform coverage:** mac+linux (real Claude Code interactive session; not run on Windows, and gated `#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]` — lane 2, so it runs on a developer's machine and in no CI job (CLAUDE.md rule 5)).

##### status/shell-activity/006 — A real interactive Haiku Claude agent's Bash call that crosses Claude Code's 120s default timeout keeps the pane's rendered badge on `Working`, with the command genuinely still running — the reported bug, reproduced as the user actually sees it (PRD #386 M6b). [reel]
- **Layer:** L2, PTY-attached, real agent (drives the actual `dot-agent-deck` binary, which lazily spawns its own daemon; no synthetic hook, no fabricated `SessionStart`, no hand-set `pane_id`).
- **Agent:** a real interactive `claude --model claude-haiku-4-5-20251001 --allowedTools Bash` pane, spawned through the normal Ctrl+N new-pane flow with per-folder trust pre-seeded, exactly as a user would drive it.
- **Asserts:** a directive prompt drives the agent to make exactly one Bash tool call running `ping -c 200 127.0.0.1 > <sentinel> 2>&1` — real, non-blocked foreground work lasting ~200s under **default** Bash settings (no `timeout` parameter, no `run_in_background`), reproducing the reported case exactly. After the native `ToolStart`/`Bash` hook event confirms the call actually started, the test waits for the real, native `Idle` event — mapped from Claude Code's own `Stop` hook, never fabricated — for this agent, bounded at 157s (the PRD's own measured 127s `ToolStart`-to-`Idle` gap plus a 30s margin). Only once that `Idle` genuinely lands does the test switch to the dashboard (`Ctrl+D`) and assert the rendered card badge reads `Working` (not `Idle`) — sampled at the instant a broken monitor would have painted it `Idle`. It also independently proves the command is genuinely still running at that moment: `process_table()` + `descendants()`, walked from the test binary's own pid (never a global `ps` scan, so a concurrently running e2e test's own processes can't be mistaken for this one's), finds a live process carrying the sentinel text in its argv (the Bash-tool shell's `eval '<user command>'` segment) with a live `ping` process beneath it. A miss on either half — badge or process — means the fix did not land or a different bug (a stale badge next to a finished command) is passing as one. **A PASS therefore means the bug path was genuinely exercised** — this is the guarantee added for PRD #386's tester follow-up: previously the test sampled on a fixed wall-clock offset regardless of whether Claude Code's `Stop` hook had actually fired, so roughly two runs in three (when Claude ended the capped call with `ToolEnd` and no `Stop`) passed without the card ever having anything to recover from. If the real `Idle` does not land within the bound, the test now **fails loudly** with a `PRECONDITION NOT MET` message distinguishing "this run never exercised the bug path" from "the badge was actually wrong" — an inconclusive run is never reported as a pass.
- **Does not assert:** anything about the discriminator's internals (`descendant_shell_activity`, `003`) or the pane primitive in isolation (`004`) — this test only observes the full pipeline's user-visible output. Does not assert what happens after the sample point (the eventual falling edge once the 200s ping finishes) or what the agent does with the "moved to background" tool result. A `PRECONDITION NOT MET` panic is not a badge assertion at all — it asserts nothing about `Working`/`Idle` and must be read as inconclusive (rerun), not as evidence the fix broke.
- **Platform coverage:** mac+linux (real Claude Code interactive session; not run on Windows, and gated `#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]` — lane 2, so it runs on a developer's machine and in no CI job (CLAUDE.md rule 5)).

##### status/shell-activity/007 — A real interactive Haiku Claude agent left at its idle prompt, with its real MCP servers alive as children, keeps the pane's rendered badge on `Idle` — no false positive against a live process table (PRD #386 M6c).
- **Layer:** L2, PTY-attached, real agent (drives the actual `dot-agent-deck` binary, which lazily spawns its own daemon; no synthetic hook, no fabricated `SessionStart`, no hand-set `pane_id`).
- **Agent:** a real interactive `claude --model claude-haiku-4-5-20251001 --allowedTools Bash` pane, spawned through the normal Ctrl+N new-pane flow with per-folder trust pre-seeded — never sent a prompt, left at its own idle prompt exactly as a user who opened a pane and stepped away would leave it.
- **Asserts:** after confirming, by polling the real process table (`process_table()` + `descendants()`, walked from the test binary's own pid so a concurrent test's own processes can't be mistaken for this one's), that the agent genuinely has live children (its MCP servers and whatever else Claude Code keeps alive) — a precondition, since "an agent with no children proves nothing here" — the test waits a margin past the daemon's 500ms shell-activity poll and then asserts the dashboard's rendered card badge reads `Idle`, not `Working`. It re-samples the process table at the same moment to confirm the children are STILL alive (not just before the badge check) and logs their argv as the evidence for what was actually running. It then also runs `descendant_shell_activity()` directly against that live table and asserts it independently agrees (`Some(false)`) — the M2 fixture claim (`003`), proven here against a live process table rather than a captured one.
- **Does not assert:** which specific MCP servers are present — that is whatever the operator's real `~/.claude.json` configures (carried into the seeded test HOME by `seed_claude_trust_in_home`), logged for evidence rather than asserted by name, since a hardcoded expected set would tie the test to one machine's configuration. Does not assert anything about a busy pane (`006`, `005`) or about agent kinds other than Claude.
- **Platform coverage:** mac+linux (real Claude Code interactive session; not run on Windows, and gated `#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]` — lane 2, so it runs on a developer's machine and in no CI job (CLAUDE.md rule 5)).

### Agent protocol

#### agent/readiness

##### agent/readiness/001 — Every agent's registry entry pins the pre-prompt readiness fact the delegate and scheduler gates read (issue #243).
- **Layer:** L1 (pure registry data — no daemon, no PTY, no socket, no LLM, no `e2e` feature gate).
- **Agent:** all six registry identities at once (Claude Code, OpenCode, Pi, Codex, Devin, and the neutral "No agent" placeholder), read as data rather than launched.
- **Asserts:** each `AgentSpec::pre_prompt_readiness` equals the value issue #243 established for that agent — Claude Code and Devin `NativeSessionStart`, Codex `WrapperInterfaceReady`, OpenCode `NoSignal` (measured in #146), Pi and the placeholder `Unknown` — with the measurement each rests on recorded beside it; that the registry lookup is total and returns each agent its own entry; that `agent_registry::ALL` holds exactly the five detectable agents and excludes the placeholder. The expectation table is an exhaustive `match` on `AgentType`, so a new agent variant does not compile until somebody states what a fresh instance of it announces before its first prompt.
- **Does not assert:** what the gates DO with the value (`orchestration/delegate/029` for the wrapper-observed path, `/030` for the no-signal skip, `/011` and `scheduler/spawn/005` for the conservative `Unknown` wait); that Pi's `Unknown` is the tightest true classification — `NoSignal` is literally true and deliberately not claimed, see the enum's own doc comment; any timing.
- **Platform coverage:** mac+linux+windows.

##### agent/readiness/002 — The readiness gate's discriminator is the registry's own readiness fact, not `hook_install.is_some()` (issue #243).
- **Layer:** L1 (pure registry data — no daemon, no PTY, no socket, no LLM, no `e2e` feature gate).
- **Agent:** all six registry identities at once, read as data.
- **Asserts:** `PrePromptReadiness::has_signal()`'s full truth table, including that `Unknown` answers `true` so an unmeasured agent keeps the conservative wait and only a POSITIVE `NoSignal` buys the short path; then that the retired predicate and the real one disagree in BOTH directions on shipped agents — OpenCode carries a hook installer yet declares no pre-prompt signal at all (the case that burned 30 s per delegate), while Pi and the placeholder carry no hook installer yet still keep the gate waiting; that Codex is the third shape, native hooks present but its pre-prompt fact supplied by the deck's wrapper rather than by its own `SessionStart`, which the prompt itself causes; and finally the whole-registry form of the same claim, that the set of agents where the two predicates part company is exactly `OpenCode`, `Pi`, `No agent` — an empty list means readiness has been re-derived from `hook_install` and the defect is back.
- **Does not assert:** the gate's runtime branch, which is `pub(crate)` and covered behaviourally by `orchestration/delegate/029` / `/030` / `/011` and `scheduler/spawn/005`; that hooks are irrelevant generally — they remain how Claude, Codex, Devin and OpenCode report status, just not what answers "is there anything to wait for".
- **Platform coverage:** mac+linux+windows.

##### agent/readiness/003 — The `session_start_origin` marker's three values map to the four `AgentEvent` predicates exactly, so the one fact that RELEASES the readiness gate cannot be widened by accident (issue #243 review).
- **Layer:** L1 (pure event data — no daemon, no PTY, no socket, no LLM, no `e2e` feature gate).
- **Agent:** none (four hand-built `SessionStart` events, one per origin value plus one with no origin key at all).
- **Asserts:** the full truth table of `is_wrapper_fork_session_start` / `is_wrapper_interface_ready_session_start` / `is_wrapper_interface_settled_session_start` over `wrapper_fork`, `wrapper_interface_ready`, `wrapper_interface_settled` and an ABSENT key — with the load-bearing row being that the settled marker must NOT satisfy the ready predicate, since that single bit is what decides which of the wrapper's two facts may release a Wrapper-strategy agent's gate before the upgrade window expires, and which post-readiness buffer is then owed (`src/state.rs` guard 1). It used to decide which fact SKIPPED the buffer; measurement retracted that in round 3, and the predicates are unchanged by the retraction. The two composite predicates are asserted as the unions of the narrow ones rather than tabulated, so the RELATIONSHIP survives a fourth value being added: `is_wrapper_interface_session_start` is exactly the either-fact question the readiness GATE asks, and `is_wrapper_session_start` is that plus the fork marker. Finally that an unrecognised value (`wrapper_interface_ready_v2`) satisfies none of them, so a future or hostile origin cannot inherit the strong one's privilege by prefix.
- **Does not assert:** what the daemon does with each value (`orchestration/delegate/026` for the settled fact being held for an upgrade, `/027` for the ready fact's interface buffer, `/028` for an unattributed one); which fact a given child produces, which is the wrapper's `InterfaceWatch` and is behavioural (`codex/wrap/006`); that the marker is authenticated — it is not, deliberately, which is why guard 2 exists.
- **Platform coverage:** mac+linux+windows.

#### protocol/live-target

##### protocol/live-target/001 — `AgentEvent.live_target` preserves every target-kind and writability value while remaining optional for legacy events (PRD #20 M3).
- **Layer:** L1 (pure serde wire contract).
- **Agent:** none (JSON fixtures).
- **Asserts:** every Cartesian combination of `process|pty|tmux|sdk|none` and `live|history-only|none` survives an `AgentEvent` deserialize/serialize round trip; a legacy event without the field still deserializes and reserializes with the optional field omitted.
- **Does not assert:** state propagation or rendering (covered by `dashboard/pane/009`).
- **Platform coverage:** mac+linux+windows.

##### protocol/live-target/002 — A declared non-live capability survives eviction of its declaring event from bounded recent history (PRD #20, blocker 2).
- **Layer:** L1 (in-process `AppState::apply_event` state transition).
- **Agent:** synthetic Codex events.
- **Asserts:** after a history-only `SessionStart` and 51 later events omitting `live_target`, the session remains `Writable::HistoryOnly` rather than falling back to Live when the first event leaves the 50-entry journal.
- **Does not assert:** reconnect serialization (covered by `session/live/010`) or card rendering (`dashboard/pane/009`).
- **Platform coverage:** mac+linux+windows.

#### protocol/send-result

##### protocol/send-result/001 — Every input-delivery result retains its distinct public wire value (PRD #20 M3).
- **Layer:** L1 (pure serde wire contract).
- **Agent:** none (JSON fixtures).
- **Asserts:** `applied`, `queued`, `stale`, `wrong-session`, `history-only`, and `no-live-target` each survive an `AttachResponse` deserialize/serialize round trip.
- **Does not assert:** actual pane delivery or rendered feedback (covered by `prompt/pane-input/004`).
- **Platform coverage:** mac+linux+windows.

#### daemon/status

##### daemon/status/001 — `dot-agent-deck daemon status` names a managed agent and visibly reflects a driven live status, not a placeholder identical to an agent with no session.
- **Layer:** fast synthetic real-binary-subprocess integration (the REAL `dot-agent-deck daemon status` CLI as a subprocess + an in-process daemon attach socket, `common::spawn_inprocess_daemon`, + real `ListAgents`; no PTY attach, no LLM, no `e2e` feature gate).
- **Agent:** none (synthetic — two `cat`-stub worker panes registered as managed; one is driven to `Thinking` over the daemon's hook socket exactly as `agent-event --type running` would, the other never receives an event, as a same-daemon control).
- **Asserts:** the subprocess exits successfully; its stdout names both the driven and the control agent by pane id; and, after normalizing BOTH the pane id and the registry agent id out of each agent's own output lines (the latter differs per spawn regardless of live status), the driven agent's text differs from the control agent's — proving the command actually surfaces the live status rather than an identical placeholder or one that differs only by identity fields. Deliberately does not pin column layout, exact status wording, or row ordering.
- **Does not assert:** `--json` output shape (`daemon/status/002`); the no-daemon path (`daemon/status/003`); prompt/task redaction (`daemon/status/004`).
- **Platform coverage:** mac+linux.

##### daemon/status/002 — `dot-agent-deck daemon status --json` pins its schema-versioned public document shape and every supported live-status string.
- **Layer:** fast synthetic real-binary-subprocess integration (the REAL `dot-agent-deck daemon status --json` CLI as a subprocess + an in-process daemon attach socket + real `ListAgents`; no PTY attach, no LLM, no `e2e` feature gate).
- **Agent:** none (synthetic — a labeled `cat`-stub mode pane spawned through the TUI's real `StartAgent` attach path, driven first by the REAL `dot-agent-deck agent-event --type running` CLI with the daemon-injected pane and agent ids, then by a raw `ToolStart` on the same production hook wire so the representative row has an active tool).
- **Asserts:** the subprocess exits successfully; stdout parses as a JSON object; `schema_version` equals the exact current public version; that schema has exactly the top-level fields `schema_version` and `agents`; the fully populated managed-agent row has exactly `agent_id`, `pane_id`, `label`, `cwd`, `role`, `status`, and `active_tool`, with the expected values and nested `{ "name": ... }` tool shape; and all six supported statuses serialize with the exact public strings `Thinking`, `Working`, `Compacting`, `WaitingForInput`, `Idle`, and `Error`.
- **Does not assert:** the human-readable table (`daemon/status/001`); omission behavior when optional fields are absent; tool-detail privacy (`daemon/status/004`).
- **Platform coverage:** mac+linux.

##### daemon/status/003 — `dot-agent-deck daemon status` against an unreachable daemon fails distinguishably from a crash and from clap's own unrecognized-subcommand error, and never brings a daemon into existence at the socket it queried.
- **Layer:** fast synthetic real-binary-subprocess integration (the REAL `dot-agent-deck daemon status` CLI as a subprocess against a scratch attach-socket path with nothing listening; no in-process daemon, no PTY, no LLM, no `e2e` feature gate).
- **Agent:** none.
- **Asserts:** the subprocess does not report success; its stderr carries no Rust panic; its exit code is not clap's own generic usage/parse-error code (`2`) and its stderr carries no clap `Usage:` banner — ruling out "this build's CLI does not understand the `status` subcommand" as the reason for the failure, so it stays distinguishable from a genuinely-handled "no daemon reachable" outcome; and the queried socket path still does not exist on disk afterward, proving the read-only diagnostic never starts the daemon it is diagnosing. Deliberately does not pin the exact exit code value or message wording.
- **Does not assert:** the live-agent path (`daemon/status/001`/`002`); prompt redaction (`daemon/status/004`).
- **Platform coverage:** mac+linux.

##### daemon/status/004 — Neither human nor JSON daemon status output reveals prompt text or active-tool detail.
- **Layer:** fast synthetic real-binary-subprocess integration (the REAL `dot-agent-deck daemon status` CLI as a subprocess + an in-process daemon attach socket + real `ListAgents`; no PTY attach, no LLM, no `e2e` feature gate).
- **Agent:** none (synthetic — a `cat`-stub worker pane driven into a live `Read` tool over the daemon's hook socket with distinctive sentinels seeded into `user_prompt` and `tool_detail`).
- **Asserts:** both the human and `--json` subprocesses exit successfully; the event's prompt and active-tool-detail sentinels reach daemon live state as a test precondition; and neither sentinel appears anywhere in either command's combined stdout/stderr, keeping both output modes on the same privacy contract.
- **Does not assert:** task-file/delegate text (out of scope for `ListAgents`-derived data); the exact non-sensitive tool-name representation.
- **Platform coverage:** mac+linux.

##### daemon/status/005 — A real `agent-event` CLI report joins to the TUI-spawned agent's live status.
- **Layer:** fast synthetic real-binary-subprocess integration (the REAL `dot-agent-deck agent-event --type running` and `daemon status [--json]` CLIs as subprocesses + an in-process daemon's hook and attach sockets + real `StartAgent`/`ListAgents`; no PTY attach, no LLM, no `e2e` feature gate).
- **Agent:** none (synthetic — two `cat`-stub panes spawned through the TUI's real attach request; the driven pane runs the CLI with the exact `DOT_AGENT_DECK_PANE_ID` and daemon-injected `DOT_AGENT_DECK_AGENT_ID`, while the second is an untouched control).
- **Asserts:** the lifecycle subprocess exits successfully and the daemon broadcasts its raw `Thinking` event carrying the exact pane and agent ids; human status distinguishes the driven row from the identity-normalized control row; and the driven JSON entry carries a `status` key rather than omitting it as a placeholder.
- **Does not assert:** exact human status wording or column layout; the exact JSON status string and schema field names (`daemon/status/002`); a literal TUI detach/reconnect (`session/live/012`).
- **Platform coverage:** mac+linux.

#### worktree/reclaim

##### worktree/reclaim/001 — `dot-agent-deck worktree list` succeeds in a git repo and names the worktree it examined.
- **Layer:** fast synthetic real-binary-subprocess integration (the REAL CLI as a subprocess against real `git` repos in a tempdir, with a synthetic `gh` on `PATH`; no PTY, no daemon, no LLM, no `e2e` feature gate).
- **Agent:** none.
- **Asserts:** the command exits successfully and its output names the examined worktree. Deliberately does not pin the verdict wording or column layout, which are the implementation's to choose.
- **Does not assert:** the removal path (`worktree/reclaim/002`); JSON shape (`006`).
- **Platform coverage:** mac+linux.

##### worktree/reclaim/002 — A deck-owned, MERGED, clean worktree is reclaimed even though its commits are NOT in `main`'s ancestry (the squash-merge case), and its branch survives.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none (a real worktree on a branch carrying its own commit, with the stub `gh` reporting `MERGED` — the exact shape a squash-merged branch has locally).
- **Asserts:** first, a **fixture precondition** that `git branch --merged main` does NOT list the branch, so the test provably exercises the ancestry-vs-PR-state divergence rather than passing for the wrong reason; then that the worktree directory is gone after `reclaim --yes`, and that `git branch --list` still shows the branch — committed work stays recoverable.
- **Does not assert:** remote branch state; the ownership-marker file format (only that marking a tree makes it reclaimable).
- **Platform coverage:** mac+linux.

##### worktree/reclaim/003 — A dirty worktree is never removed, even with a MERGED PR and `--yes`, and the report says why it was kept.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none (an untracked file placed in an otherwise-reclaimable worktree).
- **Asserts:** first, that the exit code is not clap's own generic `2` and stderr carries no clap `Usage:` banner — ruling out "this build's CLI does not understand `worktree reclaim`" as the reason the worktree survives, so the domain assertion below is not vacuously true; then that the worktree still exists after `reclaim --yes`, and the output names dirtiness/uncommitted/untracked as the reason. The untracked file was never part of the PR, so it is genuinely absent from `main` — the case the "the code is already merged" argument does not cover.
- **Does not assert:** the exact wording of the reason; behaviour for tracked-but-modified files (the same gate, one representative case tested).
- **Platform coverage:** mac+linux.

##### worktree/reclaim/004 — A worktree whose branch IS an ancestor of `main` but has NO pull request is never removed.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none (a branch created at `main`'s tip with no canned PR fixture, so the stub `gh` returns `[]`).
- **Asserts:** a **fixture precondition** that `git branch --merged main` DOES list the branch — so the ancestry check's false-positive is genuinely present — then, as in `003`, that the exit code and stderr rule out clap's own unrecognized-subcommand error (without this, "the worktree still exists" would hold vacuously today, since clap never touches the filesystem either) — and finally that the worktree still exists after `reclaim --yes`. This is the destructive direction of the naive check: the same shape as a live scratch worktree that a "git says merged, delete it" rule would destroy.
- **Does not assert:** the reason wording; closed-unmerged or open-PR states (same gate, distinct fixtures).
- **Platform coverage:** mac+linux.

##### worktree/reclaim/005 — A foreign (unmarked) merged clean worktree is asked about, not removed, and the ask names the exact path and the command that would proceed.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none (a reclaimable worktree deliberately left without an ownership marker).
- **Asserts:** as in `003`/`004`, first that the exit code/stderr rule out clap's own unrecognized-subcommand error; then that the worktree still exists after a bare `reclaim`; the output contains the worktree's exact path (not a count or a category); and it contains `--yes`, the specific command that would proceed. Pins the "when it asks, it asks specifically" requirement.
- **Does not assert:** interactive confirmation (this is the non-interactive path); the ordering of ask-versus-detail in the output, which is not mechanically checkable here.
- **Platform coverage:** mac+linux.

##### worktree/reclaim/006 — `dot-agent-deck worktree list --json` emits a document carrying `schema_version` and the examined worktree.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none.
- **Asserts:** stdout parses as JSON, carries a `schema_version` key, and includes the examined worktree. Deliberately does not pin field names beyond `schema_version`.
- **Does not assert:** the full document shape; per-verdict field naming.
- **Platform coverage:** mac+linux.

##### worktree/reclaim/007 — PR state is resolved against a worktree's OWN `origin` remote, not the caller's cwd — regression coverage for the `resolve_pr_state(repo_dir, ...)` → `resolve_pr_state(&wt.path, ...)` fix.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none (the main checkout's `origin` is removed entirely, then a worktree is given its own `origin` via `extensions.worktreeConfig` naming a repo whose branch has a MERGED PR fixture; `remote.<name>.url` is a list-accumulating git config variable — verified directly against git 2.55.0 — so a per-worktree override only actually takes effect when the common config defines no `origin` at all, which is why the main checkout's is removed rather than merely overridden).
- **Asserts:** `worktree list`'s row for the worktree carries PR column `merged`, verdict `remove`, and reason `-` (none) — reachable only by resolving PR state from the worktree's own remote, since the main checkout has no `origin` and resolving against its path (the pre-fix behaviour) can never derive a `--repo`, always failing closed to `keep`/`unresolvable` regardless of the worktree's actual PR.
- **Does not assert:** the `reclaim` (removal) path for this fixture, or JSON output — same gate, already covered elsewhere (`002`, `006`); the "unrelated repo's coincidental MERGED PR" framing from the fix's own doc comment, which this suite could not reproduce (see the test's doc comment and `set_worktree_origin`) because it requires the common config to ALSO carry a resolvable `origin`, which the list-accumulation behavior above rules out.
- **Platform coverage:** mac+linux.

##### worktree/reclaim/008 — A reclaimable worktree whose DIRECTORY NAME contains a non-UTF-8 byte is still removed by `reclaim --yes`.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none (a worktree directory built from raw bytes via `OsStr::from_bytes`/`Command::arg`, never through a `&str`/`to_string_lossy` conversion that would corrupt the byte before git ever saw it).
- **Asserts:** first, a **fixture precondition** that the scratch dir genuinely contains an entry whose raw bytes exactly match the intended non-UTF-8 name — ruling out "the filesystem silently normalised or rejected it" as the reason later assertions pass; then, as in `003`/`004`/`005`, that the exit code/stderr rule out clap's own unrecognized-subcommand error; then that the human report actually carries a non-empty `Removed:` section (not `Removed: none`) — ruling out "the directory was simply never created" as the reason it's absent; and finally that the worktree directory is gone. Pins Greptile P1 (upstream PR #427, `src/worktree_reclaim.rs:482`): `examine_worktrees` lossy-converts the parsed `PathBuf` into a `String`, and `run_reclaim` feeds that mangled string to `git worktree remove`, so a worktree whose path contains non-UTF-8 bytes is never reclaimed even though it is otherwise fully eligible.
- **Does not assert:** behaviour on non-Linux filesystems (APFS/HFS+ reject non-UTF-8 filenames outright, so this scenario cannot exist there); which specific byte is preserved, only that the exact bytes round-trip.
- **Platform coverage:** linux.

##### worktree/reclaim/009 — Two pending worktrees whose names differ only in one non-UTF-8 byte render as two DIFFERENT bullets, so the operator can tell which directory `--yes` would delete.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`).
- **Agent:** none (two worktree directories built from raw bytes via `OsStr::from_bytes`/`Command::arg`, named `candidate-\xff` and `candidate-\xfe`, both MERGED and clean and deliberately left unmarked so both land on the `ask` surface).
- **Asserts:** a **fixture precondition** that the scratch dir holds an entry whose raw bytes exactly match each intended name, and that the two names genuinely differ — ruling out "the filesystem normalised one of them" as the reason the bullets do or do not collide; then, as in `003`/`004`/`005`/`008`, that the exit code/stderr rule out clap's own unrecognized-subcommand error; a **control** that a bare `reclaim` still leaves both directories on disk, so the report is a decision pending rather than a post-mortem; that exactly two bullet lines appear; and finally that those two lines differ. The comparison is made on the subprocess's **raw stdout bytes**, never through `String::from_utf8_lossy` — comparing lossy strings would apply the very conversion under test, so a correct byte-distinct rendering could be reported as colliding purely because the harness collapsed it. Pins issue #578: `format_reclaim_human` rendered paths through `Path::to_string_lossy`, which maps every invalid sequence to `U+FFFD`, so two byte-distinct directories printed as one identical line while the removal acted on the distinct byte-exact values.
- **Does not assert:** the escape's exact syntax — "the two lines differ" accepts every shape the issue allows (reversible escaping, disambiguation, or refusing to offer the reclaim), rather than pinning the implementation's choice; that a literal `\xFF` in a name cannot alias the raw byte `0xFF` (the unit test `display_path_does_not_alias_a_raw_byte_with_its_literal_escape_text` in `src/worktree_reclaim.rs` covers that half of injectivity, which needs no worktree); the `worktree list` PATH column (unit-tested alongside it); the `--json` document, whose `path` field is still lossy by deliberate schema decision.
- **Platform coverage:** linux (as `008`: APFS/HFS+ reject non-UTF-8 filenames outright, so this scenario cannot exist there).

##### worktree/reclaim/010 — A worktree created through the deck's REAL creation path reads as deck-owned and reaches the unattended `remove` verdict, while an otherwise-identical hand-made sibling stays foreign.
- **Layer:** fast synthetic real-binary-subprocess integration (as `worktree/reclaim/001`), with one addition: the subject worktree is created by calling the production `issue_dispatch_run::create_worktree` in-process — the only `git worktree add` in `src/`, and the function every dispatch and every issue-dispatch fire goes through — rather than by the fixture's own `git worktree add`.
- **Agent:** none (two worktrees on the same repo, both clean, both with a canned `MERGED` PR; one created by the production path, one by a plain `git worktree add`).
- **Asserts:** a **fixture precondition** that the production call returned `WorktreeCreation::Created` — the already-claimed arm is somebody else's directory and is deliberately never marked, so a test that accepted it would prove nothing; then that `worktree list --json`'s entry for the deck-created worktree carries `owned: true` and `verdict: "remove"`; and, as a **control**, that the hand-made sibling carries `owned: false` and `verdict: "ask"` — without it, `owned` could be reading `true` for every worktree and the test would still pass. Nothing calls the fixture's `mark_owned` helper: the marker has to arrive from the creation path itself. Pins issue #425, where `OWNER_MARKER_FILENAME` was defined and read but written nowhere in `src/`, so every deck-created worktree read as foreign and the `remove` tier was unreachable in normal use.
- **Does not assert:** the marker file's *content* or format (the ownership gate is an existence check by design; the unit tests in `src/issue_dispatch_run.rs` cover the write's placement, tree-cleanliness and idempotency); the `reclaim` removal path for this fixture (`002`); retro-marking of pre-existing worktrees, which is deliberately not done at all.
- **Platform coverage:** mac+linux.

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

#### prompt/close-confirm

##### prompt/close-confirm/001 — Command-mode Ctrl+W opens a Cancel-default close confirmation.
- **Layer:** L1 (in-process key mapper + close-confirm state + ratatui `TestBackend`).
- **Agent:** none.
- **Asserts:** Ctrl+W resolves `CloseSelected`, an available target opens the confirmation, both pane- and tab-scoped states render their exact blast-radius sentence/description without copy leakage, and `Cancel` remains selected by default.
- **Does not assert:** daemon teardown after confirmation (covered by `lifecycle/stop/*` and `dashboard/pane/002`).
- **Platform coverage:** mac+linux+windows.

##### prompt/close-confirm/002 — Cancel preserves the target while explicit confirmation authorizes one close.
- **Layer:** L2 (real-binary PTY plus real daemon registry).
- **Agent:** none (continued `cat` pane).
- **Asserts:** production Ctrl+W on a plain dashboard pane opens the pane-scoped `Close selected pane?` Cancel-default modal; Enter on Cancel preserves the rendered card and daemon agent record; a fresh Ctrl+W followed by Down+Enter removes both.
- **Does not assert:** StopAgent error classification (covered by `lifecycle/stop/005`–`008`).
- **Platform coverage:** mac+linux.

##### prompt/close-confirm/003 — The `[Close]` button and Ctrl+W share the same confirmation action path.
- **Layer:** L2 (real-binary PTY; production button render, SGR mouse decoding, and keyboard dispatch).
- **Agent:** none (continued `cat` pane).
- **Asserts:** clicking the live `[Close Ctrl+W]` button opens the same rendered pane-scoped Cancel-default modal as Ctrl+W, and neither path tears down the daemon agent before explicit confirmation.
- **Does not assert:** tab-strip `×` dispatch (covered by `mouse/tabstrip/002`–`003`).
- **Platform coverage:** mac+linux.

##### prompt/close-confirm/004 — Input queued behind the arming mouse event cannot confirm an unseen modal.
- **Layer:** L2 (real-binary PTY; one raw burst through production SGR mouse + keyboard event decoding).
- **Agent:** none (continued `cat` pane).
- **Asserts:** a single burst containing the real Close-button click followed by Down+Enter opens the modal with Cancel still selected and leaves the daemon agent alive; only a fresh post-render Down+Enter closes it.
- **Does not assert:** terminal-driver event chunking beyond the one-write burst used by the regression.
- **Platform coverage:** mac+linux.

##### prompt/close-confirm/005 — A vanished armed session closes nothing and never retargets its replacement.
- **Layer:** L2 (real-binary PTY plus a synthetic replacement SessionStart delivered through the real daemon hook socket).
- **Agent:** none (continued `cat` pane; the hook gives the same pane a distinct replacement agent/session identity in rendered state).
- **Asserts:** after Ctrl+W arms the original session identity, a different-agent SessionStart replaces it on the same pane; confirming surfaces `Nothing closed`, retains the card, and leaves the daemon agent alive rather than closing the replacement.
- **Does not assert:** tab identity binding (covered independently by `mouse/tabstrip/003`).
- **Platform coverage:** mac+linux.

##### prompt/close-confirm/006 — A dashboard Session target that belongs to a Mode tab uses whole-tab copy and teardown.
- **Layer:** L2 (real-binary PTY against a protocol-faithful scripted daemon).
- **Agent:** none (a hydrated Mode agent pane rendered as a dashboard card plus one persistent side pane).
- **Asserts:** arming Ctrl+W from the selected dashboard card renders `Close this tab and all its panes?`, never the pane sentence; confirming sends stops for both daemon panes and removes the tab only after the registry is empty.
- **Does not assert:** internal `CloseTarget`/`ClosePlan` variants; the rendered promise and observable blast radius are the contract.
- **Platform coverage:** mac+linux.

##### prompt/close-confirm/007 — A close that would LEAVE a dispatched worktree holding uncommitted work says so, with its path, before the user answers (issue #717).
- **Layer:** L1 (in-process `TestBackend` through `render_close_confirm_to_buffer`).
- **Agent:** none.
- **Asserts:** a confirmed-dirty tree renders the flat claim (`Uncommitted work here is KEPT, not deleted:`) plus the absolute path, positioned ABOVE the Cancel/Close options because it changes what answering means; an inconclusive probe renders the conditional wording (`…here, if any, is KEPT:`) with the path and never the flat claim; and a close that leaves nothing behind renders exactly the pre-#717 dialog.
- **Why it exists:** the outcome this warns about is decided in a detached daemon task AFTER the card and pane are destroyed, so the dialog is the last surface that still exists while the information is both obtainable and actionable. The wording split is the honest half: the probe is time-boxed on an interactive key path, and a deadline it blew must degrade the sentence rather than drop the path.
- **Does not assert:** where the preview comes from, or that it is accurate — the daemon-side resolution is unit-tested (`issue_dispatch_run::kept_worktree_preview_*`) and driven end to end by `dispatch/close/002`.
- **Platform coverage:** mac+linux+windows.

##### prompt/close-confirm/008 — A kept-worktree path too long for the terminal is clipped from the FRONT, keeping the tail that identifies it.
- **Layer:** L1 (in-process `TestBackend`, 60-column terminal against a 130-character path).
- **Agent:** none.
- **Asserts:** the distinguishing tail survives, the clip is marked with a leading `…`, the full path is never claimed verbatim, and no rendered line exceeds the terminal width.
- **Why it exists:** the popup widens to fit the path rather than truncating by default, so truncation is only reachable when the terminal itself is too narrow — and there the default (tail-dropping) rule would produce `/home/user/code/dot-agent-…`, which names none of the sibling worktrees it has to tell apart.
- **Does not assert:** the widening itself, or the wording variants (`prompt/close-confirm/007`).
- **Platform coverage:** mac+linux+windows.

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

##### prompt/pane-input/004 — A history-only send returns an honest result and surfaces feedback instead of silently dropping input (PRD #20 M3/M4).
- **Layer:** L2 (real spawned TUI + daemon in the PTY/vt100 harness; synthetic pane and hook event, no LLM).
- **Agent:** synthetic wrapped Codex session backed by `cat`, declared `writable = history-only` through `AgentEvent.live_target`.
- **Asserts:** an unidentified paned `WriteAndSubmit` returns `send_result = no-live-target`, the same target with explicit agent/session identity returns `history-only`, attempting to enter the card renders `History-only session cannot accept live input`, and the rejected send does not remove the Codex card.
- **Does not assert:** real Codex execution or wrapper stdout classification (covered by `codex/live/001` and `codex/wrap/001`).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/005 — An open attach stream rejects key and paste input after its focused session becomes non-live (PRD #20, blocker 6).
- **Layer:** L1 protocol integration (in-process daemon attach server + real PTY-backed shell; fast tier).
- **Agent:** synthetic Codex session bound to the shell pane.
- **Asserts:** a baseline live key reaches the child; after the same session declares history-only, subsequent key and bracketed-paste `KIND_STREAM_IN` frames produce no child output.
- **Does not assert:** UI mode exit or card feedback; this pins the authoritative daemon stream-input gate.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/006 — Seed-prompt delivery is retained safely and abandoned after its deadline (PRD #20 findings #3/#4/#13).
- **Layer:** L1 (in-process seed-prompt readiness consumer with a controllable `PaneController`).
- **Agent:** none.
- **Asserts:** injected transport error and non-applied outcomes retain the seed with feedback and backoff; two fresh TUI states generate distinct IDs; delivery captures its logical session; an expired permanent failure is abandoned without another RPC.
- **Does not assert:** daemon production of stale/wrong-session or orchestration-role status; those require identity-bearing daemon requests and the orchestration render loop.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/007 — An orchestrator prompt is retained and the role stays non-working after a non-applied result (PRD #20, blocker 5 / finding 17).
- **Layer:** L2 PTY-attached real orchestration flow with a synthetic role that changes from history-only to live.
- **Agent:** synthetic Codex role emitting raw `AgentEvent` liveness transitions; no LLM.
- **Asserts:** the real spawn-time orchestrator-prompt action surfaces HistoryOnly feedback, does not mark the role Working, retains the prompt, and retries it successfully once the same role declares a live PTY target.
- **Does not assert:** the other result variants (covered at the seed consumer by `prompt/pane-input/006`).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/008 — Stream-input rejection visibly exits PaneInput for both keys and paste (PRD #20 R20-007).
- **Layer:** L2 PTY-attached real dashboard with a synthetic pane and hook liveness transition.
- **Agent:** synthetic Codex session backed by `cat`; no LLM.
- **Asserts:** after a focused live pane becomes history-only, a rejected key and rejected bracketed paste each render feedback and leave PaneInput mode.
- **Does not assert:** the daemon's byte-level stream gate (covered by `prompt/pane-input/005`).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/009 — A queued prompt cannot cross an agent or logical-session generation (PRD #20 finding #4).
- **Layer:** L1 protocol integration with an in-process daemon and real PTY-backed shells.
- **Agent:** synthetic Codex identities bound sequentially to the same pane.
- **Asserts:** paned requests with no expected agent return `no-live-target`, and requests with no expected session against either an attached or unattached pane's current hook session return `stale`; all write no marker. Requests queued for an original agent, a same-agent pre-`/clear` session, or a session missing on the target also fail closed, while a matching agent/session and an identified agent with no hook session still deliver.
- **Does not assert:** UI feedback for the returned result (covered by `prompt/pane-input/006`).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/010 — Delivery IDs are atomic and bound to a request fingerprint (PRD #20 finding #3).
- **Layer:** L1 protocol integration with an in-process daemon and real PTY-backed shell.
- **Agent:** synthetic Codex identity backed by `/bin/sh`.
- **Asserts:** sequential and writer-barrier concurrent duplicates produce one append; reusing an ID with a different payload or target cannot replay a false successful result.
- **Does not assert:** retry scheduling or visible feedback.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/011 — Unknown send-result variants decode as safe non-delivery (PRD #20 R20-011).
- **Layer:** L1 fast wire-decoding unit test.
- **Agent:** none.
- **Asserts:** a future `send_result` value does not reject the whole response and is not classified as delivered.
- **Does not assert:** live daemon version skew.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/012 — `ok=false` overrides a contradictory applied send result (PRD #20 R20-011).
- **Layer:** L1 fast client test with a synthetic Unix-socket daemon.
- **Agent:** none.
- **Asserts:** the client does not report delivery for `{ok:false, send_result:"applied"}`.
- **Does not assert:** server-side response construction.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/013 — Liveness is revalidated after acquiring the exact target writer (PRD #20 R20-006).
- **Layer:** L1 protocol integration with an in-process daemon, held writer mutex, and real PTY-backed shell.
- **Agent:** synthetic Codex identity backed by `/bin/sh`.
- **Asserts:** a request authorized while live but blocked on the writer writes no bytes after the session becomes history-only.
- **Does not assert:** the attach-handle removal race in R20-008, which has no deterministic harness barrier.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/014 — Post-snapshot stream rejection returns a typed reason (PRD #20 finding #10).
- **Layer:** L1 protocol integration with an in-process daemon and live state handle.
- **Agent:** synthetic Codex identity backed by `/bin/sh`.
- **Asserts:** after the client observes `Live` but daemon state changes before `KIND_STREAM_IN`, both key and paste frames receive a non-empty typed rejection frame.
- **Does not assert:** the TUI's visible feedback/mode exit after consuming that frame; no injectable UI/server barrier currently spans those processes.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/015 — Guarded send fails safe against a daemon without the capability (PRD #20 finding #6).
- **Layer:** L1 client protocol test with a synthetic previous-shape Unix-socket daemon.
- **Agent:** none.
- **Asserts:** an identity-bearing send returns an error and submits zero requests when the daemon handshake lacks guarded-send capability.
- **Does not assert:** the release-step manual test against the actual previous-release daemon.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/016 — Orchestrator prompt identity is captured at tab creation (PRD #20 finding #5).
- **Layer:** L1 orchestration action with a controllable pane-controller rebind.
- **Agent:** none.
- **Asserts:** replacing the start pane's agent after tab creation cannot change the queued prompt's captured target identity.
- **Does not assert:** daemon-side stale rejection, covered by `prompt/pane-input/009`.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/017 — Malformed guarded-send identity fails closed (PRD #20 Greptile finding #1).
- **Layer:** L1 protocol integration with an in-process daemon and real PTY-backed shells.
- **Agent:** synthetic pane targets backed by `/bin/sh`.
- **Asserts:** a wrong JSON type for `expected_agent_id`, `expected_session_id`, or `delivery_id` is rejected and submits no marker bytes.
- **Does not assert:** malformed base `WriteAndSubmit` fields, covered by the protocol's general malformed-request tests.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/018 — A pane-less history-only target rejects stream input (PRD #20 Greptile finding #2).
- **Layer:** L1 protocol integration with an in-process daemon and a real PTY-backed shell carrying no pane environment ID.
- **Agent:** synthetic Codex history-only event attached to a pane-less `/bin/sh` target.
- **Asserts:** `KIND_STREAM_IN` returns a typed non-empty rejection and writes no marker bytes when the attach handle resolves to the no-pane sentinel.
- **Does not assert:** visible TUI feedback after consuming the rejection, covered by `prompt/pane-input/008`.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/019 — Guarded-send generation remains monotonic under delayed prior-session and same-session events (PRD #20 Greptile findings #3/P1).
- **Layer:** L1 protocol integration with an in-process daemon and real PTY-backed shells.
- **Agent:** synthetic Codex lifecycle generations sharing one pane and agent identity.
- **Asserts:** delayed activity cannot restore an old generation, a delayed `SessionEnd` from either a prior session or an older timestamp cannot clear the current generation, a current `SessionEnd` does clear it, stale prompts remain rejected, and current prompts remain deliverable.
- **Does not assert:** transport-level event reordering before `AppState::apply_event`.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/020 — Guarded sends resolve pane-less writability and routing by agent identity (PRD #20 Greptile P1).
- **Layer:** L1 protocol integration with an in-process Unix-domain socket daemon, held writer mutex, and real PTY-backed shells.
- **Agent:** synthetic Codex live and history-only events bound by agent identity to pane-less `/bin/sh` targets.
- **Asserts:** a pane-less send with no expected agent returns `no-live-target` without bytes, pre-lock history-only sends return `history-only` without bytes, a live-to-history transition while waiting for the writer is rejected after the lock, and an identified live pane-less target still receives its guarded prompt.
- **Does not assert:** visible TUI feedback for the returned result.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/021 — Ctrl+W performs real shell word deletion without closing the pane.
- **Layer:** L2 (PTY-attached real binary and a real interactive Bash/readline pane).
- **Agent:** none (the shell is the genuine user surface under test, not an agent stand-in).
- **Asserts:** after typing two words, Ctrl+W deletes the previous word, the replacement word is what the submitted command visibly prints, and both the rendered pane and daemon agent record still exist.
- **Does not assert:** close confirmation from command mode (covered by `prompt/close-confirm/*`).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/022 — Ctrl+W while editing a real interactive Claude Haiku prompt does not tear down the pane.
- **Layer:** L2 (PTY-attached real binary, runtime-skipped when Claude CLI/credentials are unavailable; flaky-tolerant lane-2 tier).
- **Agent:** REAL interactive Claude Code on `claude-haiku-4-5-20251001`, with onboarding/project trust seeded and `--allowedTools Bash Read`; no `-p`.
- **Asserts:** after the real Claude pane registers under its temp-directory-prefixed display name and the genuine interactive prompt renders, typing two sentinel words and pressing Ctrl+W visibly deletes the final word, proving the keystroke reached Claude; returning to command mode leaves the pane visible and the same daemon-side agent record present.
- **Does not assert:** an LLM response (the safety invariant and native prompt-edit behavior are proven without submitting a model turn).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/023 — Orchestrator prompt writes remain provisional until the matching submission is observed.
- **Layer:** L1 (in-process orchestrator prompt consumer with a controllable `PaneController` and hook-derived state snapshot).
- **Agent:** none.
- **Asserts:** both `Applied` and `Queued` retain the prompt text, delivery identity, retry backoff, non-Working role, and unprompted tab; a matching `UserPromptSubmit`-derived event for that pane clears all provisional state and alone finalizes the role as Working without another write.
- **Does not assert:** how confirmation is correlated internally or the daemon's PTY behavior; only the consumer's observable delivery state contract.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/024 — Seed delivery distinguishes confirmable panes, unconfirmable panes, and both swallowed-CR duplicate shapes.
- **Layer:** L1 (in-process `process_pending_seed_prompts` consumer with a controllable `PaneController` and hook-derived state snapshot).
- **Agent:** none.
- **Asserts:** `Applied`/`Queued` reporting panes remain provisional until matching submission; one Pi status event and a pane with no identity each write exactly once without arming retries; short and >200-byte doubled submissions joined by either a newline or no separator clear retry state before an immediately eligible third write; repetition is bounded to 16 newline-separated copies and is not a wildcard.
- **Does not assert:** orchestration-role status (covered by `prompt/pane-input/023`) or whether the seed came from dispatch versus a configured mode.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/025 — Unconfirmed prompts retry to deadline, including a producer identified only after the readiness fallback.
- **Layer:** L1 (clock-controlled orchestrator prompt consumer and controllable `PaneController`).
- **Agent:** none.
- **Asserts:** an `Applied` write with no matching submission stays pending, retries only after its armed backoff, never marks the role Working, and is abandoned without a final write after `AUTOMATIC_PROMPT_DEADLINE`; an unidentified fallback write stays provisional without retyping and arms a real retry when a late reporting `SessionStart` arrives.
- **Does not assert:** wall-clock scheduling in the render loop or exact tracing-log wording.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/026 — Only fresh matching prompt evidence carrying the target identity confirms provisional delivery.
- **Layer:** L1 (in-process seed-prompt consumer with two pane identities and synthetic hook-derived snapshots).
- **Agent:** none.
- **Asserts:** matching text with no agent id, matching text from another pane, unrelated target-pane text, and matching text already present before the write all leave delivery identity and retry armed; only fresh matching pane/text/identity evidence finalizes the seed.
- **Does not assert:** a particular reconciliation key or algorithm beyond rejecting these observable false matches.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/027 — Attempt-ID rotation crosses a caching delivery ledger without weakening same-attempt idempotency.
- **Layer:** L1 (in-process seed consumer backed by a faithful per-delivery-id caching controller).
- **Agent:** none.
- **Asserts:** a lost response retries the same `#a1` id and replays cached `Applied` without a second physical write; the later unconfirmed retry rotates to `#a2`, reaches the writer physically, and a returned `Ambiguous` terminally clears all delivery state with no further attempt.
- **Does not assert:** daemon socket framing or the registry's ledger implementation internals; the controller reproduces its observable caching contract.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/028 — A provisional retry never reaches a replacement agent or a same-agent conversation ended by clear.
- **Layer:** L1 (in-process seed consumer with an identity-guarding, rebindable `PaneController` and hook-derived generation state).
- **Agent:** none.
- **Asserts:** after the first write, a different registry agent appearing on the pane gets zero bytes and terminally disarms the old delivery; a `SessionEnd` for the bound generation likewise prevents any same-agent retry and clears provisional state.
- **Does not assert:** the detached scheduler/dispatch confirmation task or a real agent's `/clear` command; it pins the same observable identity/generation contract at the TUI controller seam.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/030 — An unbound launcher delivery may bind a live generation before retry, but cannot follow a generation through its end into a successor, even when the first applied write's response was lost.
- **Layer:** L1 (in-process seed and orchestrator consumers with a payload/identity-recording `PaneController`, a first-response-loss mode, and hook-derived generation state).
- **Agent:** none.
- **Asserts:** the first write into a pane with no announced hook session declares no generation; once the real agent's `SessionStart` arrives and remains current, the next retry binds and retains it and a third attempt uses a distinct submit-only probe; separately, both seed and orchestrator TUI write sites send no bytes into a successor when a generation is observed and then ends, when its complete start/end plus the successor start burst between two render passes, or when that burst follows a physically applied first write whose RPC response was lost.
- **Does not assert:** the daemon-side confirmation task's own latch (covered by `scheduler/dispatch/016`) or the PTY bytes an empty payload produces (covered by the registry's submit path).
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/031 — Daemon-synthetic events do not prove a usable prompt-reporting channel.
- **Layer:** L1 (in-process seed consumer with a payload-recording `PaneController` and synthetic hook-derived snapshots).
- **Agent:** none.
- **Asserts:** identified daemon-authored shell-activity and delivery-notice events landing after the write, alongside a real but untagged legacy hook frame, leave the delivery held.
- **Does not assert:** an unauthenticated unmarked producer claim on the TUI path — deliberately not pinned because it is indistinguishable from `prompt/pane-input/025`'s accepted slow-launcher recovery and blocking it would re-open #424 for launchers with no bootstrap event; the detached path is pinned by `scheduler/dispatch/016`. It also does not assert authentication of the delivery-notice metadata key, which grants no privilege.
- **Platform coverage:** mac+linux+windows.

##### prompt/pane-input/032 — User typing after a TUI automatic payload disarms the next retry.
- **Layer:** L1 (in-process seed consumer driving the production registry guard and a real `/bin/cat` byte-observation PTY).
- **Agent:** none.
- **Asserts:** attempt 1 physically reaches the pane before any automatic-write timestamp exists; an unsent user draft after attempt 1 prevents attempt 2 from appending its replacement payload or submitting the draft, and independently a draft after attempt 2 prevents attempt 3's submit-only probe, each proven by an unchanged PTY byte snapshot.
- **Does not assert:** the fix's internal clock-comparison location or the detached spawn watcher (covered by `scheduler/dispatch/018`).
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

##### prompt/new-pane/011 — The new-agent Command field seeds from the last command you spawned when no `default_command` is configured (PRD #196).
- **Layer:** L2 (no public L1 render seam for the new-pane form — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid).
- **Agent:** none (`cat` is a real runnable stand-in command — the spawn succeeds and records a last command, and `cat` blocks on stdin so the pane stays alive; no LLM tokens).
- **Asserts:** with an empty `default_command`, opening the new-pane form the first time leaves the Command field BLANK; after typing `cat` and submitting (spawning a pane), reopening the form pre-fills the Command field with `cat`, seeded from the recorded last command. RED until the feature lands: nothing reads the recorded last command back, so the reopened field renders blank.
- **Does not assert:** persistence of the last command across a full deck restart (the read-back here is in-process); per-directory last commands (the value is global); the exclusion of authoring-mode fallback commands from the recorded last command.
- **Platform coverage:** mac+linux.

##### prompt/new-pane/012 — An explicit `default_command` still wins over the recorded last command in the new-agent form — precedence guard (PRD #196).
- **Layer:** L2 (no public L1 render seam for the new-pane form — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid).
- **Agent:** none (`cat` is a real runnable stand-in command — the spawn succeeds and records a last command; no LLM tokens).
- **Asserts:** with `default_command = "configured-default-cmd"`, the new-pane Command field pre-fills from it; after clearing the field, typing `cat`, and submitting (recording `cat` as the last command), reopening the form STILL pre-fills `configured-default-cmd` — the explicit config value wins over the recorded last command. GREEN today and after the feature lands.
- **Does not assert:** the empty-`default_command` fallback to the last command (covered by `prompt/new-pane/011`); persistence across a restart.
- **Platform coverage:** mac+linux.

##### prompt/new-pane/013 — An authoring-mode spawn's command IS recorded and seeds a later regular form — the exclusion was dropped so all form-launched spawns record their command (PRD #196).
- **Layer:** L2 (no public L1 render seam for the new-pane form — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid).
- **Agent:** none (`cat` is a real runnable stand-in command — the authoring spawn succeeds and `cat` blocks on stdin so the card stays alive; no LLM tokens).
- **Asserts:** with an empty `default_command`, cycling the Mode field to the built-in `schedule` AUTHORING option, clearing the Command field, typing `cat`, and submitting dispatches an authoring-mode spawn; reopening a FRESH regular form (no Mode cycle) then PRE-FILLS the Command field with `cat` — an authoring-mode spawn now records a last command like any other form-launched spawn (the exclusion was dropped for consistency), so the regular form seeds from it. RED until the coder removes the authoring gate.
- **Does not assert:** the plain-spawn seed-from-last-command path (covered by `prompt/new-pane/011`); the `default_command` precedence (covered by `prompt/new-pane/012`); persistence across a restart (covered by `prompt/new-pane/014`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/014 — The recorded last command survives a full deck restart and pre-fills the new-agent form on the next launch (PRD #196).
- **Layer:** L2 (no public L1 render seam for the new-pane form — same constraint as `prompt/new-pane/007`; the real TUI is driven via PTY keystrokes and asserted on the rendered vt100 grid; two launches share one isolated HOME so the persisted session carries over).
- **Agent:** none (`cat` is a real runnable stand-in command — the spawn succeeds and records a last command, and `cat` blocks on stdin so the pane stays alive; no LLM tokens).
- **Asserts:** with an empty `default_command`, launch 1 spawns `cat` and quits cleanly so the session flushes to disk; launch 2 (sharing the same HOME) then PRE-FILLS the new-pane Command field with `cat`, read back from the persisted `session.toml` launch 1 wrote — proving the recorded last command round-trips through persist → reload → seed, not just in-process state. GREEN against the current implementation — a regression guard.
- **Does not assert:** the in-process read-back within one launch (covered by `prompt/new-pane/011`); the `default_command` precedence (covered by `prompt/new-pane/012`); the authoring-mode recording (covered by `prompt/new-pane/013`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/015 — Selecting an agent in the real new-pane form seeds its registry default without a global-config copy (PRD #20, finding 8).
- **Layer:** L2 PTY-attached (the private new-pane form is driven through the real binary and its visible selector is clicked/cycled).
- **Agent:** none (selection rows for Claude Code, OpenCode, Pi, and Codex; no agent process is submitted).
- **Asserts:** with no global `default_command`, the form exposes an `Agent:` selector; selecting each shipped type visibly updates Command to exactly that type's `AgentSpec.default_command`.
- **Does not assert:** launch wrapping (covered by `codex/spawn/*` and `codex/live/001`) or custom command arguments.
- **Platform coverage:** mac+linux.

##### prompt/new-pane/016 — Selecting the "dispatcher" option in the new-pane form opens a live dispatcher dashboard card whose real Claude agent, given a goal, invokes `dot-agent-deck dispatch` itself and the daemon creates the promised sibling git worktree (PRD #220). [reel]
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness with imported Claude credentials — records a `full-stream.cast`). The freshly-built binary's dir is prepended to the PATH the deck → daemon → agents inherit, so the agent's `dot-agent-deck dispatch` resolves to the build under test rather than a host-installed binary that predates the verb.
- **Agent:** Claude Code (interactive `claude`, real Anthropic API — receives the dispatcher seed prompt via gated delivery, acts on the typed goal, and runs the `dispatch` verb itself; no stand-in).
- **Asserts:** the dispatcher surfaces LIVE as a dashboard CARD within 60s of form submission (`1 session(s)`, no tab strip — a mode tab would instead route through `render_mode_tab`'s 50/50 split and render the agent at half width beside an empty column, which is the shape this pins against); that the seed actually reached the pane (a distinctive `DISPATCHER_SEED_PROMPT` phrase, so it cannot pass on an unseeded agent); then, after a directive one-unit goal is typed into the pane, the sibling worktree `../<repo>-dispatch-probe-unit` appears on disk within 180s — proving agent → `dispatch` CLI → daemon → `git worktree add` end to end, at the sibling (never nested) path.
- **Also asserts (added after real use found three defects underneath the original green run):** that the unit comes up as a real AGENT — a second live session whose card carries an agent type — because `SpawnRequest.command: None` reads as `$SHELL` in the spawn path, so the previous assertions passed while the unit was a bash prompt with the task text typed into it. Verified to be capable of failing by reintroducing `command: None`. The typed goal also names `--single`, so the shape selector is exercised end to end rather than steering the agent back onto the legacy config-derived path.
- **Does not assert:** the dispatched unit's own OUTPUT; an `--orchestration` dispatch (covered deterministically by `dispatch::tests::an_orchestration_dispatch_writes_the_delegation_protocol_and_the_task`, which spawns `cat` roles and asserts the orchestrator-context file — no LLM tokens); the return edge (#220's own deferred Phase 2 — NOT #174, which depends on this PRD rather than tracking it); cleanup on tab close (covered by `src/dispatch.rs` unit tests).
- **Platform coverage:** mac+linux.
- **Note:** the fixture repo is given an initial commit by the test — the harness `git init`s fixtures but never commits, and `git worktree add` cannot branch from an unborn HEAD.

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

##### tabs/mode/001 — Selecting a mode on the new-pane form opens a mode tab with the agent pane on the left and persistent side panes stacked on the right; both side panes render SIMULTANEOUSLY under the deck's default (Stacked) global `pane_layout` (PRD #311 regression guard).
- **Layer:** L2 (PTY-attached, `tests/e2e_mode_tab_layout.rs`).
- **Agent:** none (fixture `tests/fixtures/mode-two-side-panes` with TWO persistent side panes, each printing a unique sentinel and idling).
- **Asserts:** the new-pane form's Mode selection opens a Mode tab (tab strip appears); with the deck's default `PaneLayout::Stacked` global, BOTH side panes' sentinels are visible in the grid at the same time — proving the Mode tab's side-pane column (hardcoded `PaneLayout::Tiled` in `render_mode_tab`, `src/ui.rs`) does not read the shared global `pane_layout` field (PRD #311's Open Question 2 risk) and so never collapses a side pane to a titled 1-row frame regardless of the global's value.
- **Does not assert:** the side pane's command output content beyond the sentinel line; the agent pane's exact left-half geometry (covered by `compute_frame_layout_mode_geometry`, a plain unit test in `src/ui.rs`); orchestration/dashboard pane-column geometry (covered by `orchestration/layout/002`).
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

##### tabs/mode/006 — A persistent side pane keeping the default `watch = true` shows its command's output while the command is still running (issue #367).
- **Layer:** L2.
- **Agent:** none (fixture whose single mode has one persistent pane running `printf …; sleep 600` under the default watch wrapper).
- **Asserts:** a sentinel assembled at runtime by the command — so it cannot appear in the command line the pane's shell echoes — is visible in the side pane although the command never exits; the echoed wrapper invocation is gone from the pane, proving the watcher cleared the screen ahead of its first output rather than after process exit.
- **Does not assert:** the 10s re-run interval; the ordering of interleaved stdout/stderr; the buffer-then-clear internals (covered by `watch::tests` unit tests).
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

##### tabs/orchestration/006 — In a real multi-role orchestration tab under `PaneLayout::Stacked`, non-focused roles render no collapsed title-bar frame, a non-focused role's agent keeps running with its sidebar status transitioning live, and switching focus between roles preserves each role's rendered content with no lost scrollback (PRD #311).
- **Layer:** L2 (PTY-attached, `tests/e2e_orchestration_pane_column.rs`).
- **Agent:** none (fixture `tests/fixtures/orch-focus-lifecycle`: a 3-role orchestration — `orchestrator` (start), `alpha`, `beta` — each printing a unique sentinel; `beta`'s script additionally self-posts real `SessionStart`/`PreToolUse` hook events via `dot-agent-deck hook --agent claude-code`, resolved to the freshly built test binary, so its sidebar status transitions Idle -> Working while its pane is not the focused/expanded slot).
- **Asserts:** (a) with `orchestrator` focused/expanded, the settled grid carries no collapsed `Borders::TOP` title-bar frame for either non-focused role (`alpha`, `beta`) — matched by a row that, after trimming only leading blank columns, begins with the bare role name directly followed by border-fill dashes, a pattern only the collapsed-pane block itself can produce; (b) `beta`'s sidebar status card visibly transitions to `Working` purely from its own self-posted hook events while never becoming the focused pane, proving a non-focused role's agent lifecycle (PTY, hook delivery, status) is untouched by the rendering change; (c) driving `j`/`k` (Normal mode) round-trips focus orchestrator -> alpha -> beta -> alpha -> orchestrator, and each role's own sentinel text is visible again once it becomes the expanded pane, proving no lost scrollback or stale fragment across a focus switch.
- **Does not assert:** PTY resizing of the reclaimed area (`resize_panes_to_layout`); the L1 geometry math (covered by `orchestration/layout/002`); a real LLM agent (all three roles are shell stand-ins); dashboard-tab (non-orchestration) collapsed frames.
- **Platform coverage:** mac+linux.

##### tabs/orchestration/007 — In command mode `Ctrl+l` toggles an orchestration tab's sidebar/pane-column split between the default 34/66 and a narrower-sidebar 25/75 and back, while in PaneInput the same chord does nothing to the split (PRD #336).
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness, `tests/e2e_orchestration_pane_column.rs`).
- **Agent:** none (`orch-deck` fixture — two stub `cat` roles, no LLM tokens spent).
- **Asserts:** opening a real orchestration tab on a 120-column PTY renders the role-pane column's left edge at the default 34%-width boundary (col 40/41); `Ctrl+l` pressed while still in PaneInput does NOT move it (the chord belongs to the role pane's agent there); after `Ctrl+d` to command mode, `Ctrl+l` moves the boundary to the narrower-sidebar 25%-width position (col 29/30) and a second press restores 34%. The boundary is read from the `orchestrator` role-pane box's top-left corner (exactly `panes_area.x`), matching any corner glyph — PRD #341 renders the focused pane's border heavier in command mode, so pinning one glyph would break on the mode switch.
- **Does not assert:** the global scope of the toggle across multiple open orchestration tabs (covered by `orchestration/layout/004`); persistence of the toggled state across restart (out of scope per PRD #336); remapping the chord via config.
- **Platform coverage:** mac+linux.

##### tabs/orchestration/008 — `Ctrl+l` still forwards to a live pane's PTY when the active tab is NOT an orchestration tab (PRD #336 scope guard).
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness, `tests/e2e_orchestration_pane_column.rs`).
- **Agent:** none (a `cat -v` pane on the Dashboard, no LLM tokens spent).
- **Asserts:** on a Dashboard (non-orchestration) tab with a live `cat -v` pane in PaneInput mode, typing a unique sentinel then pressing `Ctrl+l` then Enter makes the pane echo `<sentinel>^L` — `cat -v` renders the received `0x0c` as the two characters `^L`, so the echo appears only if the raw byte actually reached the PTY. Pins the PRD #336 scope rule that `Action::ToggleOrchestrationSplit` claims `Ctrl+l` only on an orchestration tab; regression coverage for the Greptile P1 on PR #342, where the global resolver claimed it unconditionally and swallowed the key on every other tab.
- **Does not assert:** the orchestration-tab toggle behavior itself (covered by `tabs/orchestration/007`); Mode-tab or other non-Dashboard tab types (Dashboard is sufficient to prove the missing tab-context check). Deliberately does NOT rely on a shell's readline `clear-screen` side effect, which depends on the host terminal setup and made an earlier version of this test fail where forwarding was in fact correct.
- **Platform coverage:** mac+linux.

##### tabs/orchestration/009 — An orchestration tab's tab-bar label renders in the color of the single highest-priority state among its panes (PRD #333).
- **Layer:** L1 (in-process `TestBackend` render via `render_tab_bar_to_buffer`, `tests/render_tab_strip.rs`).
- **Agent:** none (synthetic `SessionStatus` values, no panes/PTYs).
- **Asserts:** given an orchestration tab whose panes carry a mix of `SessionStatus` values, the rendered tab-bar label's foreground color is `palette::status_color()` of the SINGLE highest-priority status among them, in the fixed order Error(Red) > WaitingForInput(Yellow) > Working(Green) > Thinking/Compacting(Blue) > Idle/Unknown(no tint) — covering (a) one `Error` among several `Idle` panes -> Red; (b) one `WaitingForInput` among `Working`/`Idle` (no `Error`) -> Yellow; (c) all `Idle` -> the SAME base label color as an ordinary tab, NOT `Color::DarkGray` (PRD #333 defect B: PRD #13 reserves DarkGray for purely-decorative elements, not label text); (d) a mix of `Thinking` and `Working` (no higher-priority state) -> Green, since Working outranks Thinking. Also asserts a non-orchestration tab's label is unaffected (same base color as any other unaffected tab).
- **Does not assert:** the aggregate-priority resolver as a standalone pure-function unit test (PRD #333 M1, may land separately); per-pane sidebar status rendering (covered by `focus/orchestration/002`); pane-column geometry (covered by `orchestration/layout/002`/`004`).
- **Platform coverage:** mac+linux+windows.

##### tabs/orchestration/010 — An ACTIVE orchestration tab carries no status tint, and an inactive Idle orchestration tab renders with no grey (PRD #333).
- **Layer:** L1 (in-process `TestBackend` render via `render_tab_bar_to_buffer`, `tests/render_tab_strip.rs`).
- **Agent:** none (synthetic `SessionStatus` values, no panes/PTYs).
- **Asserts:** an orchestration tab made the ACTIVE tab with a non-idle (`Error`) pane renders with NO status `fg` tint at all — its `fg` and modifiers must match an active non-orchestration (Dashboard) tab exactly (`REVERSED | BOLD`, no absolute color), since stacking a status `fg` on `Modifier::REVERSED` would invert the color into a background at display time (defect A). Also asserts an INACTIVE orchestration tab whose aggregate status is `Idle` renders with the same base label color as an ordinary tab, not `Color::DarkGray` (defect B), and that an INACTIVE orchestration tab with a non-idle (`Error`) aggregate status still colors its label text with neither `REVERSED` nor `BOLD` (regression guard, unchanged from before).
- **Does not assert:** the aggregate-priority resolver (covered by `tabs/orchestration/009`); per-pane sidebar status rendering (covered by `focus/orchestration/002`); pane-column geometry (covered by `orchestration/layout/002`/`004`).
- **Platform coverage:** mac+linux+windows.

##### tabs/orchestration/011 — In command mode `z` zooms the focused role pane to the whole frame with a `[Z]` marker on its kept border, while in PaneInput the same key is an ordinary character; every non-focused agent keeps running behind the zoom and a second `z` restores the previous view (PRD #313).
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness, `tests/e2e_orchestration_pane_column.rs`).
- **Agent:** none (`orch-focus-lifecycle` fixture — a 3-role orchestration of shell stand-ins; `beta` self-posts real `SessionStart`/`PreToolUse` hook events through the freshly built binary so its sidebar status transitions Idle -> Working. No LLM tokens spent, so this entry is deliberately UNMARKED for the demo reel per CLAUDE.md rule 4 — the reel clip comes from `tabs/orchestration/012`).
- **Asserts:** opening a real orchestration tab on a 120-column PTY renders the role-pane column's left edge at the default 34%-width boundary (col 40/41); `z` pressed while still in PaneInput moves neither the boundary nor draws a marker, because there it is a plain character the agent is entitled to receive; after `Ctrl+d` to command mode, `z` moves the focused pane's box to column 0 — it still HAS a box, which is what the corner-glyph anchor proves, so the border that carries the title/focus/status channel survived the zoom — and its border title gains a `[Z]` marker; the sidebar (and with it `beta`'s status card) is no longer drawn; while zoomed the daemon's live agent registry still lists all three roles, so hiding a pane touched no agent's lifecycle; and a second `z` restores the 34/66 split with the marker gone and `beta`'s card back and still reading `Working`.
- **Does not assert:** what a REAL agent does across the two PTY resizes (covered by `tabs/orchestration/012`); the per-tab scope of the zoom state across several open tabs (covered by `orchestration/layout/009`); the resolver/scoping of the key itself (covered by `orchestration/layout/007`); persistence across detach/reattach (out of scope per PRD #313 Open Question 5 — zoom is ephemeral).
- **Platform coverage:** mac+linux.

##### tabs/orchestration/012 — A REAL interactive Claude agent reflows and keeps working across a zoom and an unzoom, painting a sentinel it could only have discovered by running a tool (PRD #313). [reel]
- **Layer:** L2 PTY-attached, real-agent tier (records a `full-stream.cast`, so it is demo-reel-eligible per PRD #180). Runtime-skipped when the `claude` CLI or credentials are absent.
- **Agent:** REAL Claude Code (Haiku, `claude-haiku-4-5-20251001`), FULLY INTERACTIVE — no `-p`, no stand-in — as the non-orchestrator `worker` role of the `orch-lock-live` fixture; the orchestrator stays `cat` so the run costs two short real turns. Launched WITHOUT `DOT_AGENT_DECK_EXPERIMENTAL`, so PRD #393's command-entry lock is absent and the directives typed at the worker reach it. Project trust is pre-seeded into the per-test HOME (`with_claude_trust_workdir`) so claude's first-run onboarding/trust gates clear with no keystroke.
- **Asserts:** with the live agent's pane focused via the `2` role jump and rendered at the default 34/66 split, `z` in command mode moves its box to column 0 and marks its border `[Z]`; a directive typed into the ZOOMED pane — asking the agent to `ls` and name the only file ending in `.txt` — results in the sentinel `zoomlive_x7q2m.txt` painting inside the full-width pane column, which is only reachable if the agent survived the PTY resize, kept working, and kept rendering at the new width; a second `z` restores the 34/66 split; and a second directive naming the only `.log` file results in `zoomlive_k4v9p.log` painting after the downward resize too. Neither sentinel token appears in the directive that asks for it, so an echo of the user's own typing can never satisfy either assertion, and the wrap-insensitive search is cropped to the pane column so the sidebar's card text cannot splice a needle apart. This is PRD #313's "resize churn … the thing to verify with a real agent rather than a stand-in".
- **Does not assert:** anything when skipped — where credentials are absent this test executes nothing, so `tabs/orchestration/011` carries the CI-visible coverage; scrollback preservation across the resize beyond the agent continuing to paint; the exact reflowed line breaks (LLM- and terminal-dependent).
- **Platform coverage:** mac+linux (real-agent tier is local-only).

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

#### embed/key-forwarding

##### embed/key-forwarding/001 — Shift+Enter typed into a focused embedded agent pane inserts a NEWLINE into the agent's draft instead of SUBMITTING it (PRD #227).
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness). Mirrors the `scheduler/dispatch/013` reference harness: imported Claude credentials, project-trust pre-seeded into the per-test HOME (`with_claude_trust_workdir`) so the first-run onboarding/trust gates clear with no keystroke, and `--allowedTools Bash` so no permission prompt can block the pane.
- **Agent:** REAL interactive `claude` pinned to Haiku (`claude --model claude-haiku-4-5-20251001 --allowedTools Bash`, NO `-p`). A stand-in cannot cover this case: `cat` has no draft, so it cannot distinguish "inserted a newline" from "submitted" — which is the entire behavior under test.
- **Asserts:** that the deck pushed the enhanced keyboard protocol at startup (`ESC[>1u` in its output stream), so the forwarding behavior below is measured with M2 actually in effect. Then: with the restored pane auto-focused, typing a first draft line, injecting `ESC[13;2u` (the CSI-u encoding of Shift+Enter a kitty-capable terminal emits) into the DECK's PTY, and typing a second line leaves the draft as TWO lines of ONE input box — the second marker renders on the row IMMEDIATELY BELOW the first, and both rows are bracketed by the prompt editor's own horizontal rules. Adjacency is simultaneously the newline proof and the no-submission proof (a submitted first line would have been repainted into the transcript far above the box before the second line was typed); the rule bracketing is what scopes the two markers to the input box, so a submitted draft the agent repainted into the transcript as two consecutive rows cannot satisfy it vacuously. Independently: the uniquely-named sentinel `shiftnl-7f3c.txt` that the first line's directive would create if submitted does NOT exist in the pane cwd, and after a deliberate plain Enter it DOES appear — a gating positive control, without which the absence could hold for the wrong reason (a slow agent, or one that declined the tool call).
- **Does not assert:** which encoding the user's outer terminal emits for a physical Shift+Enter (the keypress is injected already CSI-u-encoded); the push/pop lifecycle itself, which is `embed/key-forwarding/002`.
- **Cost:** the draft assertions submit nothing (zero LLM tokens); only the positive control spends one short Haiku turn.
- **Platform coverage:** mac+linux (lane-2 e2e tier; flaky-tolerant, run once, not looped).

##### embed/key-forwarding/002 — The deck pushes the enhanced (kitty) keyboard protocol at TUI startup and pops it on clean exit, so Shift+Enter reaches the deck with no user-side terminal configuration and no keyboard mode leaks into the user's shell (PRD #227 M2).
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness). Asserts on the deck's raw OUTPUT byte stream (`stream_text`) rather than the rendered grid, because the escape sequences under test are consumed by the vt100 parser and never paint a cell.
- **Agent:** none — the behavior is the deck's own terminal negotiation, so this is fully deterministic and spends zero LLM tokens. The harness's `answer_terminal_queries` replies to the `ESC[?u` / `ESC[c` capability probe, which is what makes `supports_keyboard_enhancement()` return true and the gated push fire, modelling the kitty-capable terminal the fix targets.
- **Asserts:** `ESC[>1u` (crossterm's `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)`) appears once the dashboard is up; after a clean exit via Ctrl+C twice, the matching `ESC[<1u` pop appears, after the push, exactly once. The multiplicity check pins the pop's idempotence — both the normal teardown and the RAII guard's `Drop` run on a clean exit, and a second pop would discard a flag set another program on the terminal's stack owns.
- **Does not assert:** the pop on a `?`-error return or a panic unwind from inside the event loop (both need a real terminal whose I/O fails, so the guard mechanism is covered by the L1 `ui::tests::keyboard_enhancement_*` tests instead); that a real terminal honors the pushed mode.
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

##### hooks/install/004 — A locally-built deck writes a DURABLE binary path into agent config, never its own `target/debug` artifact.
- **Layer:** L2.
- **Agent:** none (the real `target/debug` binary is the subject; a stub `codex` on `PATH` makes the Codex installer fire, and a stub executable at `$HOME/.local/bin/dot-agent-deck` is the durable candidate).
- **Asserts:** with `~/.claude/`, `~/.codex/` and `~/.opencode/` seeded, a durable executable at `$HOME/.local/bin/dot-agent-deck`, and no `dot-agent-deck` on the child's `PATH`, every deck-owned command in `~/.claude/settings.json` and `~/.codex/hooks.json` and the OpenCode plugin's `BINARY_PATH` name that seeded path; none of the three files mentions `target/debug` or `target/release`; no command is a bare command name.
- **Does not assert:** the resolver's `PATH`-lookup fallback (step 2b) or its behaviour when `current_exe()` is already installed (step 1) — both are unit-level; that the seeded executable actually runs.
- **Platform coverage:** linux.

##### hooks/install/005 — With no durable path resolvable, the deck writes no hook rule at all and still starts.
- **Layer:** L2.
- **Agent:** none (a stub `codex` on `PATH` makes the Codex installer fire; nothing else is seeded).
- **Asserts:** with `~/.claude/`, `~/.codex/` and `~/.opencode/` seeded but no `$HOME/.local/bin/dot-agent-deck` and no `dot-agent-deck` anywhere on the child's `PATH`, `settings.json` and `hooks.json` carry no deck-owned command, the OpenCode plugin file is absent, and the dashboard still paints — refusing is not fatal.
- **Does not assert:** the wording of the refusal message or the tracing/stderr surface it is reported on.
- **Platform coverage:** linux.

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

##### lifecycle/stop/005 — Closing an already-stopped daemon agent completes local teardown.
- **Layer:** L1 (real `EmbeddedPaneController` against a synthetic Unix-socket daemon).
- **Agent:** none (synthetic StartAgent / AttachStream; both StopAgent attempts return exact `Agent agent-1 not found`, and ListAgents reports the stable pane slot empty).
- **Asserts:** `close_pane` performs both stale-id attempts, enters the real ListAgents slot-resolution path, returns success for the proven-empty slot, removes the pane, does not re-insert the ghost card, and emits no unverified-close warning.
- **Does not assert:** the dashboard confirmation UI (`prompt/close-confirm/*`); daemon process termination (the synthetic daemon reports the agent already absent).
- **Platform coverage:** mac+linux.

##### lifecycle/stop/006 — A genuine StopAgent failure still retains the pane and surfaces the error.
- **Layer:** L1 (real `EmbeddedPaneController` against a synthetic Unix-socket daemon).
- **Agent:** none (synthetic StartAgent / AttachStream; StopAgent returns a non-NotFound server error).
- **Asserts:** `close_pane` returns the daemon error, re-inserts the pane for retry, and does not apply the NotFound-only retry/classification to other failures.
- **Does not assert:** the timeout arm (the existing retain-and-surface implementation remains unchanged); dashboard status-message layout.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/007 — Unrelated errors containing `not found` retain the live pane and surface the error.
- **Layer:** L1 (real `EmbeddedPaneController` against a synthetic Unix-socket daemon).
- **Agent:** none (synthetic StartAgent / AttachStream; StopAgent returns `pane not found`, `session not found`, another agent's exact NotFound, or a wrapped requested-agent NotFound).
- **Asserts:** every non-exact/non-id-scoped message returns an error containing the daemon reason, re-inserts the pane, sends only one StopAgent request, and never enters ListAgents slot resolution.
- **Does not assert:** presentation of the surfaced message in the TUI status row.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/008 — A replacement agent occupying the pane slot is stopped before local teardown.
- **Layer:** L1 (real `EmbeddedPaneController` against a synthetic Unix-socket daemon).
- **Agent:** none (both stale `agent-1` StopAgent attempts return exact NotFound; ListAgents reports `agent-2` with the same `pane_id_env`; stopping `agent-2` succeeds).
- **Asserts:** the request sequence is `agent-1`, `agent-1`, `agent-2`; replacement discovery uses ListAgents; only then does `close_pane` succeed and remove the pane; no unverified-close warning is emitted.
- **Does not assert:** the asynchronous real-agent respawn mechanism that creates the replacement.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/009 — A replacement appearing near the respawn worst case is stopped before local teardown.
- **Layer:** L1 (real `EmbeddedPaneController` against a timing-controlled synthetic Unix-socket daemon).
- **Agent:** none (the initial AttachStream ends to put pane I/O into reattachment; both stale-id StopAgent attempts return exact NotFound; ListAgents reports the stable slot empty for 4.8 seconds before exposing `agent-2`).
- **Asserts:** close keeps polling through the documented slow-respawn window, sends StopAgent to the late replacement, removes the pane only after that stop succeeds, and emits no unverified-close warning.
- **Does not assert:** a real agent process's SIGTERM/startup timing; the synthetic delay deterministically represents that handover gap.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/010 — A ListAgents error completes close with one unattended-agent warning.
- **Layer:** L1 (real `EmbeddedPaneController` against a synthetic Unix-socket daemon).
- **Agent:** none (both StopAgent attempts return exact NotFound; ListAgents returns `registry unavailable`).
- **Asserts:** close returns success and removes the pane instead of restoring the ghost card; exactly one drainable warning says the pane was closed, daemon verification failed, and an agent may still be running unattended.
- **Does not assert:** rendering the queued warning on the TUI status line.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/011 — A ListAgents timeout completes close with one unattended-agent warning.
- **Layer:** L1 (real `EmbeddedPaneController` against a synthetic Unix-socket daemon).
- **Agent:** none (both StopAgent attempts return exact NotFound; ListAgents accepts the request but never replies).
- **Asserts:** close returns success after the bounded lookup timeout and removes the pane instead of restoring the ghost card; exactly one drainable warning says the pane was closed, verification timed out, and an agent may still be running unattended.
- **Does not assert:** rendering the queued warning on the TUI status line.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/012 — A chained pane-slot handover stops the last owner before teardown.
- **Layer:** L1 (real `EmbeddedPaneController` against a stateful synthetic Unix-socket daemon).
- **Agent:** none (both stale `agent-1` StopAgent attempts return exact NotFound; ListAgents reports replacement B; stopping B returns exact NotFound after replacement C takes the slot; stopping C succeeds).
- **Asserts:** close sends StopAgent to C, returns success, removes the pane only after the final owner is stopped, and emits no unverified-close warning.
- **Does not assert:** an exact number of stop requests; the guard pins the last owner being stopped so alternative depth-handling implementations remain valid.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/013 — Immediate unresolvable pane-slot churn is round-bounded and announced.
- **Layer:** L1 (real `EmbeddedPaneController` against a stateful synthetic Unix-socket daemon, with a 13-second test-side hang ceiling).
- **Agent:** none (every replacement StopAgent returns exact NotFound after handing the stable pane slot to a fresh synthetic agent).
- **Asserts:** immediate churn returns well before the total budget through the three-replacement round cap, removes the pane, and queues exactly one drainable warning saying the slot kept changing owners, the close could not be verified, and an agent may still be running unattended.
- **Does not assert:** rendering the queued warning on the TUI status line; the wall-clock budget path (covered by `lifecycle/stop/014`).
- **Platform coverage:** mac+linux.

##### lifecycle/stop/014 — Slow unresolvable pane-slot churn is wall-clock-bounded and announced.
- **Layer:** L1 (real `EmbeddedPaneController` against a stateful timing-controlled synthetic Unix-socket daemon, with a 13-second test-side hang ceiling).
- **Agent:** none (each replacement StopAgent takes four seconds before returning exact NotFound and handing the stable pane slot to another synthetic agent).
- **Asserts:** the total budget ends resolution after two delayed replacement stops and before the three-round cap, close returns success and removes the pane, and exactly one drainable slot-churn/unattended-agent warning is queued.
- **Does not assert:** rendering the queued warning on the TUI status line; real process stop latency.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/015 — A genuine replacement-agent stop failure retains the pane and surfaces the error.
- **Layer:** L1 (real `EmbeddedPaneController` against a stateful synthetic Unix-socket daemon).
- **Agent:** none (the stale original agent returns exact NotFound, ListAgents reports replacement B, and B's StopAgent returns a permission-denied server error).
- **Asserts:** close reaches B, surfaces its daemon error, retains the pane for retry, and emits no unverified-close warning instead of absorbing the failure into slot churn.
- **Does not assert:** presentation of the surfaced error in the TUI status row; the replacement timeout arm (covered by `lifecycle/stop/016`).
- **Platform coverage:** mac+linux.

##### lifecycle/stop/016 — A replacement-agent stop timeout retains the pane and surfaces the timeout.
- **Layer:** L1 (real `EmbeddedPaneController` against a stateful synthetic Unix-socket daemon, with a seven-second test-side hang ceiling).
- **Agent:** none (the stale original agent returns exact NotFound, ListAgents reports replacement B, and B's StopAgent never replies).
- **Asserts:** close reaches B, exercises the real five-second stop timeout, surfaces the timeout, retains the pane for retry, and emits no unverified-close warning instead of absorbing the timeout into slot churn.
- **Does not assert:** presentation of the surfaced error in the TUI status row; OS-level process termination.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/017 — A partially failed tab close stays visible and succeeds on retry.
- **Layer:** L2 (real-binary PTY against a protocol-faithful scripted daemon).
- **Agent:** none (a hydrated Mode tab with one agent pane and one persistent side pane; the side pane's first StopAgent is denied and its retry succeeds).
- **Asserts:** the first confirmed whole-tab close removes the successful pane, retains the failed pane and its tab/`×`, and renders that the tab was kept; after switching into the retained tab, a second confirmed close removes the failed pane, daemon record, and tab.
- **Does not assert:** an exact count of `close_pane` calls; the observable retry outcome is the contract.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/018 — Already-gone and unverified-success panes do not block whole-tab removal.
- **Layer:** L2 (real-binary PTY against a protocol-faithful scripted daemon).
- **Agent:** none (two hydrated one-pane Mode tabs: exact id-scoped NotFound with an empty slot, then exact NotFound whose ListAgents verification fails).
- **Asserts:** both outcomes remove the tab; the proven-gone close renders no unattended-agent warning, while DoneUnverified renders exactly one such warning on the live status line.
- **Does not assert:** warning expiry timing or terminal styling.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/019 — A six-pane tab closes concurrently while preserving pane order in its outcome.
- **Layer:** L1 (in-process `TabManager` with a delay-scripted `PaneController`).
- **Agent:** none (six hydrated orchestration role panes with staggered 150–400 ms synthetic close delays).
- **Asserts:** the close completes below a 1.0-second wall-clock ceiling versus a 1.65-second sequential sum, reports closed pane ids in original role order rather than completion order, and removes the clean tab.
- **Does not assert:** production daemon/RPC latency; the synthetic delays isolate fan-out semantics.
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

#### lifecycle/sigterm

##### lifecycle/sigterm/001 — A daemon sent SIGTERM (what `daemon stop` / `daemon restart` deliver) exits through its graceful shutdown path AND logs the signal, instead of dying silently under the default disposition.
- **Layer:** L2.
- **Agent:** none (a bare `daemon serve` with idle shutdown disabled, so only the signal handler can end it).
- **Asserts:** after a plain `kill(pid, SIGTERM)` the daemon process terminates within a few seconds, and its `DOT_AGENT_DECK_LOG` file contains a termination line naming `SIGTERM`.
- **Does not assert:** agent teardown ordering under signal shutdown, or `SIGINT` (the handler treats both identically and the CLI only ever sends `SIGTERM`); `--force`'s SIGKILL escalation stays with `lifecycle/stop/003`.
- **Regression origin:** the daemon installed no signal handler at all, so a stopped daemon left no log line — a real session lost seven live agent panes and the daemon's own log said nothing about why.
- **Platform coverage:** linux+mac (Unix signals; the Windows build watches Ctrl-C instead).

##### lifecycle/sigterm/002 — A second SIGTERM during shutdown forces an immediate exit instead of being swallowed, so a wedged daemon is still killable with `pkill`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after the daemon logs the first termination signal, a second SIGTERM leaves the process gone within a few seconds.
- **Does not assert:** the exact exit status (`143`), since a shutdown fast enough to finish before the second signal exits `0` and both outcomes satisfy "the daemon does not linger".
- **Regression origin:** installing a handler replaces the default disposition process-wide, so once the first signal is consumed every later SIGTERM would be absorbed by a stream nobody reads — removing the `pkill` escape hatch that the pre-handler behaviour always provided.
- **Platform coverage:** linux+mac (Unix signals).

#### lifecycle/version

##### lifecycle/version/001 — A build environment that pre-sets `DAD_VERSION` / `DAD_BUILD_ID` produces a binary that reports those values, and changing either one invalidates the cached build (issue #250).
- **Layer:** L2 (three real `cargo build`s into one shared scratch `CARGO_TARGET_DIR`, pinned to the rustc host target and capped at half the machine's cores, then plain subprocess runs of each produced binary — no PTY).
- **Agent:** none.
- **Asserts:** with `DAD_VERSION=42.7.13` / `DAD_BUILD_ID=42.7.13-ginjected0` pre-set only in the *build* environment, the produced binary's `--version` reports `42.7.13` (not the `0.1.0` `CARGO_PKG_VERSION` placeholder, and not the checkout's git tag) and `daemon hello` advertises both injected values as `daemon_version` / `build_version`; then that changing **only** `DAD_VERSION` (to `58.1.2`) and afterwards **only** `DAD_BUILD_ID` (to `58.1.2-ginjected1`) is each picked up by the next build in the same target dir — the one-at-a-time change is what pins each `cargo:rerun-if-env-changed` directive individually.
- **Does not assert:** the full fallback order *below* an injection — an absent or invalid `DAD_VERSION` falling through to git and then to the `CARGO_PKG_VERSION` placeholder — nor the build-script directive-injection rejection (both are pure-data unit tests in `tests/build_version.rs`); the `cargo:warning` text on the placeholder path; that a git-less checkout degrades correctly (would need a second cold build).
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

##### resize/layout/002 — On a terminal wider than `PTY_RESIZE_DIM_MAX`, a pane's local vt100 parser lands on the same capped geometry the daemon gives the child (issue #747).
- **Layer:** L1 (pure-data `compute_frame_layout` + the real `resize_panes_to_layout` sweep over inert seam panes; no PTY, no subprocess, no daemon). Lives in `src/ui.rs`'s own `#[cfg(test)]` module because `compute_frame_layout`, `FrameLayout::pane_target_dims` and `resize_panes_to_layout` are module-private, and because the public `render_orchestration_frame_to_buffer` seam clamps its width to `RENDER_SEAM_DIM_MAX` (1024) and so cannot reach the threshold at all.
- **Agent:** none (inert render/scroll seam panes).
- **Asserts:** at 4200 cols ZOOMED (inner 4198) and at 6400 cols UNZOOMED under the 34/66 split (inner 4222) — the two thresholds issue #747 names — `pane_target_dims` reports the capped 4096 rather than the raw inner width, and after the sweep each pane's vt100 parser sits at exactly `(rows.min(4096), cols.min(4096))`, the geometry `AgentPtyRegistry::resize` applies to the child. The cap is restated inline rather than read from the production helper, so the test states the daemon's rule instead of echoing the code under test. Also drives the `resize_pane_pty` primitive directly, one over-cap axis at a time, since it is a `pub` method whose parser write and wire write must not be able to disagree even from a call site the layout sweep does not own. **Control:** the same sweep at 4200 cols UNZOOMED targets 2770 inner cols, under the cap, and must come through completely untouched — so a regression that clamped every pane rather than only over-cap ones fails here.
- **Does not assert:** the daemon actually applying the clamp to a real child PTY (covered by `tests/daemon_protocol.rs`'s `assert_resize_clamps`, which reads it back through `stty size`); the rendered cells of an over-cap pane (covered by `render/widget/003`); the once-per-process `warn!` either clamp now emits (observability, not behaviour); any width a real terminal reaches — both fixtures are far past any physical display, which is the point.
- **Platform coverage:** mac+linux+windows.

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

The rendering-contract reproducers for the PRD #84 (`prds/done/84-rendering-layer-rework.md`)
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

##### render/widget/003 — A pane drawn wider than `PTY_RESIZE_DIM_MAX` does not trip the invariant-3 contract guard (issue #747).
- **Layer:** L1 (in-process `TerminalWidget` rendered into a `ratatui::buffer::Buffer`; no PTY, no subprocess).
- **Agent:** none.
- **Asserts:** a vt100 screen at exactly the 4096-col cap — the widest a child PTY can be given — rendered with `contract_guaranteed(true)` into an inner area 2 columns WIDER completes without panicking, which in a debug build is the whole assertion: the PRD #84 invariant-3 `debug_assert!` is live there and compares the parser against the inner area, so it must expect the *capped* inner area or a legitimately over-cap pane becomes a debug-build crash. The child's content still renders from the top-left and the columns past the cap stay blank.
- **Does not assert:** the release-mode log-once line; the upstream sizing that produces the capped parser (covered by `resize/layout/002`); the general area-larger-than-screen `min` fallback with the guard OFF (covered by `render/widget/002`).
- **Platform coverage:** mac+linux+windows.

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
- **Asserts:** after a pane is recreated/replaced in place (open a Mode tab, return to command mode, request its close with Ctrl+W, observe the tab-scoped confirmation, then choose Close with Down+Enter), the rendered grid contains the surviving Dashboard and no leftover fragment of the removed pane at a stale position.
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

##### render/layout/006 — A zoomed orchestration tab renders no sidebar and no non-focused role, while the focused pane KEEPS its border and carries a `[Z]` zoom indicator in its title (PRD #313 M5).
- **Layer:** L1 (in-process `TestBackend` via `render_orchestration_frame_to_buffer`, the public full-frame orchestration render seam; no PTY, no subprocess). Structural assertions first, `insta` snapshots of both frames after them — so a wrong rendering fails an assertion before any snapshot can be blessed, and the snapshots exist to be read rather than to carry the contract.
- **Agent:** none (two synthetic role panes, `orchestrator` focused and `worker` not).
- **Asserts:** rendered UNZOOMED at 100x30 the focused role's pane box starts at the 34%-width sidebar boundary, the non-focused `worker` role is visible beside it, and no zoom indicator is drawn; rendered ZOOMED the same frame puts the focused pane's box at column 0 — it still HAS a box, so the border that carries the title, the focus/status colour (PRD #155 M3) and the command-mode weight (`9345a74`) was not dropped along with the sidebar, which is PRD #313 Open Question 2's decision — the `worker` role appears nowhere at all, and the border title carries `[Z]`, mirroring tmux's status-line marker so nobody concludes their other agents disappeared.
- **Does not assert:** the geometry arithmetic itself (covered by `orchestration/layout/008`); the PTY dims the same layout drives (covered by `orchestration/layout/010`); the live rendered grid and the key that produces it (covered by the PTY-attached `tabs/orchestration/011`).
- **Platform coverage:** mac+linux+windows.

#### render/seam-bound

##### render/seam-bound/001 — An exported `*_to_buffer` render seam with two caller-controlled axes bounds its dimensions instead of allocating them; the four one-row seams deliberately do not (issue #748).
- **Layer:** L1 (in-process `TestBackend` through the exported seams themselves; no PTY, no subprocess).
- **Agent:** none (one synthetic `SessionState` fixture shared by the card seams; the command-banner seam renders an inert pane).
- **Asserts:** twelve exported two-axis seams — covering both shared helpers (`draw_to_buffer`, `render_overlay_to_buffer`) and seven of the eight seams that build their own `TestBackend` — return a buffer whose `area` is the requested size when it is in range, and `RENDER_SEAM_DIM_MAX` (1024) once the request exceeds it: on the width axis alone at `u16::MAX`, on the height axis alone at `u16::MAX`, on both axes at `RENDER_SEAM_DIM_MAX + 1`, and finally on both axes at `u16::MAX` — the ~4.3-billion-cell request that is an OOM/abort without the bound. The eighth, `render_dashboard_cards_to_buffer`, is asserted separately because its height is derived from the card count rather than passed. A third test pins the OTHER side of the scope: `render_button_bar_to_buffer`, `render_filter_bar_to_buffer`, `render_rename_bar_to_buffer` and `render_tab_bar_to_buffer` pass a literal `1` as their height, so at `u16::MAX` columns they return that width verbatim — 65,535 cells is not an allocation concern and a cap there would truncate a legitimately wide bar, so the exclusion is asserted rather than left as prose.
- **Does not assert:** what the seams *draw* at the cap (every other `render/*` and `dashboard/*` entry pins content at real terminal sizes); the orchestration frame's own bound and degenerate-input guard, which are inline and predate this (`render/layout/006`); behaviour at degenerate 1x1-style sizes, which is a totality question rather than an allocation one; the daemon-side PTY sibling `PTY_RESIZE_DIM_MAX` (`render/widget/003`, `resize/layout/002`).
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

##### keybindings/remap/003 — Existing `[global] close_pane` remaps survive mode-gated dispatch.
- **Layer:** L1 (TOML parse + in-process production key mapper).
- **Agent:** none.
- **Asserts:** `[global] close_pane = "Ctrl+x"` parses without warnings, the custom chord requests close in command mode, and the same chord remains ordinary `0x18` PTY input in PaneInput.
- **Does not assert:** filesystem loading of `keybindings.toml` (covered by `keybindings/remap/001`); arbitrary per-mode config syntax (out of scope).
- **Platform coverage:** mac+linux+windows.

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

##### keybindings/safety/003 — Ctrl+W is PTY input in PaneInput and a close request in command mode.
- **Layer:** L1 (in-process production key mapper).
- **Agent:** none.
- **Asserts:** the same default Ctrl+W chord yields `ForwardToPane([0x17])` in `UiMode::PaneInput` and `CloseSelected` in `UiMode::Normal`; both halves live in one regression test.
- **Does not assert:** readline's visible editing result or pane survival through the real binary (covered by `prompt/pane-input/021`).
- **Platform coverage:** mac+linux+windows.

##### keybindings/safety/004 — Mode-gating Close does not scope the other global commands.
- **Layer:** L1 (in-process production key mapper).
- **Agent:** none.
- **Asserts:** Dashboard, NewPane, and ToggleLayout still resolve from PaneInput; only ClosePane falls through to PTY input.
- **Does not assert:** each action's downstream UI mutation (covered by its feature-specific tests).
- **Platform coverage:** mac+linux+windows.

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
- **Asserts:** rendered against a `KeybindingConfig` that remaps `toggle_layout` → `Alt+Shift+l` and `help` → `F1`, the help-overlay buffer shows those custom notations and describes Ctrl+D as a command-mode / pane-input toggle, proving the overlay is generated from the active config while retaining the corrected semantics. The default-config content guard lives at `dashboard/help/002`.
- **Does not assert:** the overlay's exact column layout or footer wording beyond what the committed snapshot pins; behaviour with the *default* config (that is `dashboard/help/002`'s job).
- **Platform coverage:** mac+linux+windows.

#### keybindings/hints

##### keybindings/hints/001 — The hints bar is generated from the active keybinding config and shows remapped keys.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, through the same production `render_bottom_bar` path the app draws).
- **Agent:** none.
- **Asserts:** rendered in command mode against a config that remaps `toggle_layout` → `Alt+Shift+l`, the live button bar shows `[Toggle Layout Alt+Shift+L]` and `[Back to Pane Ctrl+D]`; the snapshot pins the complete production bar.
- **Does not assert:** truncation behaviour at narrow widths.
- **Platform coverage:** mac+linux+windows.

##### keybindings/hints/002 — An unbound action is rendered as `(unbound)` in the hints bar, never as a bare `: <label>`.
- **Layer:** L1 (ratatui `TestBackend`, through the production button-bar renderer; asserts on buffer text, no snapshot).
- **Agent:** none.
- **Asserts:** with `new_pane` unbound, the live bar renders `[New Pane (unbound)]` and never `[New Pane ]` with a blank shortcut.
- **Does not assert:** the exact placeholder wording beyond `(unbound)`, behaviour of other simultaneously-unbound actions, snapshot of the full bar.
- **Platform coverage:** mac+linux+windows.

##### keybindings/hints/003 — The hints bar reflects Close's mode scope and makes command-mode exit discoverable.
- **Layer:** L1 (ratatui `TestBackend`, rendered through `render_button_bar_for_mode_to_buffer`, which calls the live `render_bottom_bar` path).
- **Agent:** none.
- **Asserts:** command mode shows enabled `[Back to Pane Ctrl+D]` and `[Close Ctrl+W]`; Help shows `[Command Mode Ctrl+D]` and a DIM Close whose Ctrl+W mapping is inert; PaneInput shows only `[Command Mode Ctrl+D]` and no Close button.
- **Does not assert:** narrow-width wrapping or mouse hit-testing of the disabled button.
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

##### error/socket/003 — `request_from_socket` returns `None` within a bounded wait against a daemon that reads the request and then never replies and never closes, instead of hanging forever.
- **Layer:** L1 (`src/hook.rs`'s `#[cfg(test)] mod tests`; a synthetic stub daemon over a real temp Unix socket, no PTY, no daemon binary, no real agent).
- **Agent:** none (a `std::thread` stub daemon that accepts one connection, reads the request line, then sleeps well past the bound without replying or closing).
- **Asserts:** `request_from_socket`, driven on a worker thread and awaited via `mpsc::recv_timeout` at 15s (comfortably above the production 5s bound the fix adds), returns before the 15s bound and the returned value is exactly `None` — a timed-out/unbounded daemon must fold into the same "no seed" bucket as a daemon that closes without replying, not a distinct outcome. A `RecvTimeoutError::Timeout` is treated as the RED failure (`request_from_socket` is unbounded) and fails the test with an explicit panic message rather than hanging until nextest's own timeout.
- **Does not assert:** the exact timeout duration chosen by the fix (only that it is comfortably under 15s); `SocketReply`'s three-way outcome (only `request_from_socket`'s two-way `None` collapse is exercised here — the richer outcome exists for a not-yet-submitted caller); real daemon behavior; Windows named-pipe timeout semantics.
- **Platform coverage:** mac+linux (Unix-domain socket).

##### error/socket/004 — `request_from_socket` still returns the reply from a daemon that is merely slow, not absent — a bound that fires too eagerly must not be mistaken for "no seed".
- **Layer:** L1 (`src/hook.rs`'s `#[cfg(test)] mod tests`; same synthetic stub-daemon setup as `error/socket/003`).
- **Agent:** none (a `std::thread` stub daemon that accepts one connection, reads the request line, sleeps 300ms — comfortably inside the production 5s bound — then writes one JSON reply line).
- **Asserts:** `request_from_socket` returns `Some("{\"seed\":\"abc123\"}")` — the exact reply line, unmodified — proving the timeout bound added for `error/socket/003` does not fire against a daemon that is merely slow. Passes both before and after the fix; it is a correctness control, not a timing measurement, and the delay is deliberately far from the 5s bound to avoid flaking under scheduler jitter.
- **Does not assert:** the timeout duration itself (`error/socket/003` pins the unbounded-hang failure mode; this test never reaches the bound); daemon behavior beyond a single reply line; real daemon timing.
- **Platform coverage:** mac+linux (Unix-domain socket).

##### error/socket/005 — `request_from_socket` still hangs against a peer that dribbles one non-newline byte just before each per-read timeout, because every byte resets it.
- **Layer:** L1 (`src/hook.rs`'s `#[cfg(test)] mod tests`; same synthetic stub-daemon setup as `error/socket/003`/`004`).
- **Agent:** none (a `std::thread` stub daemon that accepts one connection, reads the request line, then writes a single non-newline byte every 200ms for 20s without ever sending a newline).
- **Asserts:** `request_from_socket`, driven on a worker thread and awaited via `mpsc::recv_timeout` at a 15s ceiling — comfortably above whatever operation-level deadline the fix adds, and comfortably inside the 20s the drip keeps running — returns before the ceiling. A `RecvTimeoutError::Timeout` is the RED failure (the per-read timeout keeps getting reset and never fires) and fails the test with an explicit panic rather than hanging until nextest's own timeout. Deliberately does not pin the exact deadline value so it keeps passing once any sane operation-level bound exists.
- **Does not assert:** the exact operation-level deadline chosen by the fix; the reply-length cap (a separate, deliberately out-of-scope follow-up); any other caller of the shared, vulnerable `request_from_socket_inner` code path with a different timeout value — this test exercises the choke point itself, so any future caller inherits the same coverage.
- **Platform coverage:** mac+linux (Unix-domain socket).

##### error/socket/006 — A daemon that closes without writing any bytes back folds into `SocketReply::NoReply`, not `SocketReply::Line("")`.
- **Layer:** L1 (`src/hook.rs`'s `#[cfg(test)] mod tests`; same synthetic stub-daemon setup as `error/socket/003`/`004`/`005`).
- **Agent:** none (a `std::thread` stub daemon that accepts one connection, reads the request line, then closes without writing a single byte).
- **Asserts:** `request_from_socket_at` returns `SocketReply::NoReply`. Before the fix, an EOF with an empty in-progress buffer returned `Some(String::new())` (`SocketReply::Line("")`), contradicting `SocketReply::NoReply`'s own doc comment, which already names "the daemon closed without answering" as a `NoReply` case.
- **Does not assert:** the *partial*-line-then-EOF case (some bytes written, then closed before the newline) — that is deliberately left returning `Line(partial)`, unchanged by this fix; `SocketReply::Unreachable`; timing.
- **Platform coverage:** mac+linux (Unix-domain socket).

##### error/socket/007 — A `read(2)` interrupted by a signal (`EINTR`) is retried, not folded into "no daemon".
- **Layer:** L1 (`src/hook.rs`'s `#[cfg(test)] mod tests`; same synthetic stub-daemon setup as `error/socket/003`/`004`/`005`/`006`).
- **Agent:** none (a `std::thread` stub daemon that accepts one connection, reads the request line, reports that over a channel, and then holds its reply until the test releases it — while the test `pthread_kill`s the *client* thread with `SIGUSR1` under a handler installed without `SA_RESTART`, so the interrupted read really does surface as `EINTR` rather than being restarted by the kernel).
- **Asserts:** `request_from_socket_at` still returns `SocketReply::Line("{\"seed\":\"abc123\"}")`, and that it did so having survived at least 20 signal deliveries **counted by the handler itself** — the signalling loop keeps sending until that floor is reached rather than sending a fixed number and hoping. Issue #564: `read_reply_line`'s `stream.read(&mut buf).ok()?` made *every* `io::Error` terminal, so a single signal on the reading thread ended an exchange that had ~4.7s of its 5s budget left and reported the daemon as absent. Verified to fail before the fix and pass after.
- **Does not assert:** the macOS failure itself (not reproducible off macOS — this pins the transient-read-error *class* the evidence points at, on any Unix); which errno macOS actually returned; the `WouldBlock`/`TimedOut` half of `is_transient_read_error`, which needs a deadline-vs-socket-clock divergence no portable stub can force; a signal landing *concurrently with the reply's arrival* — that ordering is deliberately excluded here (the reply is withheld until the last signal has been sent) and its consequence is pinned deterministically by `error/socket/008` instead.
- **Timing:** issue #642 removed both wall-clock bets this test used to make. The 50ms lead-in that was "sized to prevent" a signal landing during connect/write is now a **handshake** — the daemon reports having read the request line, which is proof that connect and the write are already behind us — and the 300ms reply delay that had the daemon answering at the exact moment the last signal was sent is now a second handshake, so the reply cannot race the storm. Both bets lost on a loaded macOS runner, red-flagging four required checks each time. The only wall clock left is a 2s backstop on the signalling loop, which exists so a machine that cannot deliver 20 signals fails saying *that* instead of confusing the reader with the client's own `DeadlineExpired`.
- **Platform coverage:** mac+linux (Unix-domain socket; `pthread_kill`/`sigaction`).

##### error/socket/008 — A reply already buffered by the kernel survives a per-read timeout that can no longer be re-armed, which on macOS is the ordinary end of every `get-seed`.
- **Layer:** L1 (`src/hook.rs`'s `#[cfg(test)] mod tests`; drives `read_reply_line` directly against a real temp Unix socket, no PTY, no daemon binary, no real agent).
- **Agent:** none — and no second thread either: the test connects, accepts, writes the reply and closes the peer all on one thread, so the ordering it depends on is established by the code rather than by the scheduler.
- **Asserts:** `read_reply_line` returns `Ok("{\"seed\":\"abc123\"}")` when the peer has written its reply and closed *before* the read loop arms its per-read timeout. XNU's `sosetoptlock` refuses every `setsockopt` with `EINVAL` once a socket carries both `SS_CANTSENDMORE` and `SS_CANTRCVMORE` ("the socket has been shutdown, no more sockopt's"); the client sets the first itself via the half-close that precedes every reply read, and the peer sets the second when it closes. `read_reply_line` used to propagate that re-arm failure as `ReplyReadError::Io`, folding into `SocketReply::NoReply` and discarding a complete reply sitting in the receive buffer. This is not a corner case in production: the daemon's hook loop answers `get-seed` and then reads EOF from the client's half-close on its very next pass, so it closes immediately after replying.
- **Does not assert:** anything on Linux, where `setsockopt` has no shutdown rule and the re-arm simply succeeds — this test passes before and after the fix there, and only bites on macOS (issue #642 is the macOS-only evidence it was written from); which of `read_reply_line`'s callers reach this state; the partial-line-then-close case (`error/socket/006` owns the EOF boundary); the `Unreachable` classification of a `set_timeouts` failure in `request_from_socket_at_detailed`'s prelude, which is unchanged.
- **Platform coverage:** mac+linux (Unix-domain socket) — asserted on both, meaningful on macOS.

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

##### orchestration/delegate/007 — A wrapped native-hook agent ignores its fork-time card-surfacing `SessionStart` for delegate readiness (PRD #225 M1).
- **Layer:** fast synthetic real-binary-subprocess integration (a real `dot-agent-deck wrap` child + in-process daemon hook socket + real `handle_delegate` + managed PTY; no vt100 attach, no LLM, no `e2e` feature gate).
- **Agent:** synthetic Codex executable backed by `cat`; the real `dot-agent-deck wrap` emits the early wrapper event and the test later injects the genuine native Codex event.
- **Asserts:** after a `clear = true` respawn, the task pointer is absent from the replacement PTY while only the wrapper's fork-time `SessionStart` has arrived; after the matching native `SessionStart`, the pointer is delivered promptly.
- **Does not assert:** real Codex boot timing or task execution (covered by the real-agent `orchestration/delegate/009`).
- **Survives issue #243 by construction, and the reason is worth knowing before you touch either.** #243 taught the wrapper to announce a wrapped child's interface, which releases exactly the gate this test asserts is still closed — so it looked like the first thing that would invert. It does not, because both facts the wrapper accepts are OBSERVATIONS of the child: this stand-in is `#!/bin/sh\nexec cat\n`, which prints nothing and never touches termios, so neither fires and the wrapper stays silent. That is not luck; it is the constraint that ruled out a time-based or exec-based detector. A future detector that fires on elapsed time or on the child merely existing turns this test red, and correctly so.
- **Platform coverage:** mac+linux.

##### orchestration/delegate/008 — A hookless wrapper-like agent still treats its sole fork-time `SessionStart` as ready (PRD #225 M1 guard).
- **Layer:** fast synthetic PTY integration (real `handle_delegate` + managed PTY + daemon broadcast; no socket or LLM).
- **Agent:** hookless future-wrapper stand-in represented by the neutral registry identity because no shipped hookless Wrapper agent exists yet.
- **Asserts:** a marked wrapper-fork `SessionStart` releases prompt delivery within two seconds — well inside the 30 s `SESSION_START_WAIT_TIMEOUT` fallback, so a pass cannot be the fallback firing — when nothing better can arrive for this agent. **The condition was re-stated in issue #243 and the behaviour is unchanged.** It used to be "the agent has no native hook installer"; the gate now asks the registry's own `PrePromptReadiness` and accepts a fork-time event as readiness only for `Unknown` — an agent whose pre-prompt readiness has not been established, which is what the neutral identity this stand-in resolves to carries. Every agent with a signal of its own keeps waiting, exactly as before. Same outcome for every shipped agent, said in terms of the question actually being asked; `agent/readiness/002` pins that the two predicates are not interchangeable.
- **Does not assert:** a concrete Gemini registry entry or wrapper classifier; those do not exist yet.
- **Platform coverage:** mac+linux.

##### orchestration/delegate/009 — A `clear = true` delegate to a REAL wrapped Codex worker delivers the prompt and the worker acts on it — the user-visible end of PRD #225 (M5). [reel]
- **Layer:** L2 PTY-attached REAL-agent (the real `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness — records a `full-stream.cast`; lane-2 e2e tier per CLAUDE.md rule 5 — the credentialed half, flaky-tolerant).
- **Agent:** a REAL interactive cheap-model Codex (`common::codex_test_model()`, no `-p`, no stand-in) as the `clear = true` `coder` role, wrapped from its first spawn because the role command's basename resolves to Codex; the `orchestrator` role is a deterministic script that invokes the genuine `dot-agent-deck delegate --to coder` CLI over the same hook socket a real orchestrator agent uses (the defect is entirely on the worker side, so a second LLM would add a flaky link without covering another line of the fix).
- **Asserts:** opening the orchestration through the normal Ctrl+N new-pane form surfaces the `coder` role card live; jumping into the worker's role pane shows the REAL Codex TUI up (its header names the pinned model) BEFORE anything is delegated — the readiness precondition, taken on the user-visible surface because codex-cli 0.145.0 posts its native `SessionStart` only when the first turn starts, so gating on that event would deadlock on the delegate that causes it; after the delegate the worker's card visibly enters `Thinking`, the daemon broadcasts the worker's GENUINE native Codex `SessionStart` (no wrapper-fork origin marker, so it is Codex itself and not the wrapper's fork-time card-surfacing event) plus a `Thinking` whose `user_prompt` is the injected `worker-task-coder.md` pointer — a field only Codex's native `UserPromptSubmit` hook sets, the wrapper's line classifier always leaves it `None` — so the pointer was submitted INSIDE the agent rather than echoed away by the launcher's line discipline; and the respawned worker creates the uniquely named sentinel `prd225-codex-delegate-6f21ba.txt` with the requested contents. Pre-fix the wrapper's fork-time event released the readiness gate seconds before the Codex TUI existed, the prompt was lost, and no sentinel ever appeared.
- **Also asserts, since issue #243:** that the delegate is PROMPT, not merely eventual. The wrapper's interface-ready `SessionStart` for the REPLACEMENT worker is captured as the anchor — an event that did not exist before #243 — and no more than 15 s may pass between it and the pointer's submission. This is load-bearing rather than decorative: every other assertion in this test was satisfied by the BROKEN path too, because the 30 s timeout fallback delivered the pointer eventually and Codex then acted on it, so the test passed for months through the defect and would keep passing through a regression to it. The budget is justified from both ends in `READY_TO_SUBMIT_BUDGET`, and round 3 moved one term rather than re-guessing it: it was 10 s while the harness's `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS=0` left no buffer inside the interval at all; this test now pins that variable at the production 5000 ms, so the budget is 5 s wider and the uncontrolled share is unchanged. Half of the ~30.6 s a run still paying the dead wait would measure (the production delegates in the issue burned 31.2–32.3 s), and ~10 s of headroom over the deck's own share, leaving the remainder for codex-cli's `UserPromptSubmit` hook. **Measured 2026-08-26 against real codex-cli 0.149.0 on `gpt-5.6-luna`: 5.758 s** — the 5000 ms buffer plus 758 ms of codex taking the keystrokes and its hook reaching the daemon, landing where the constant's derivation predicted a warm Codex would (~5.6 s).
- **Does not assert:** the work-done leg (logged as a soft observation; hard-covered by `codex/worker/001`); the launch-shape half of PRD #225 (`codex/spawn/007` for the hook-learned badge, `codex/spawn/008` for the respawn wrap decision); the hookless-wrapper guard (`orchestration/delegate/008`); WHICH of the wrapper's two interface facts released the gate — the anchor accepts either, since the point is to measure from whatever did (the synthetic `codex/wrap/006` covers the emit, and only a real Codex TUI exercises the strong `ICANON`/`ECHO` fact at all).
- **Platform coverage:** mac+linux (unix-only — writes an executable role script).

##### orchestration/delegate/010 — An observed replacement `SessionStart` starts, but does not bypass, the delegate readiness buffer (PRD #249 M1).
- **Layer:** fast synthetic PTY integration (real `handle_delegate` + `clear = true` respawn + in-process daemon hook socket; no LLM and no `e2e` feature gate).
- **Agent:** synthetic hook-emitting worker backed by `cat`.
- **Asserts:** with `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS=1000`, the task pointer arrives, and the delay from the replacement agent's matching `SessionStart` to its arrival is at least the whole configured buffer. **Restructured in issue #243 from a wall-clock race into a lower bound.** It used to sleep 350 ms, assert absence, and cap delivery at 2 s — a two-sided wall-clock constraint around a timer the daemon owns, which failed once in three full-tier runs while passing every time in isolation. The measurement now starts BEFORE the hook line is written, so every source of delay (socket latency, runtime jitter, the 20 ms snapshot poll) can only push it up; only a bypassed buffer pushes it down. Verified load-bearing by re-running with the buffer env at `0`: delivery in **24.7 ms** against the 1000 ms floor, a 40x margin. Verified flake-free at 10/10 with all 16 cores saturated.
- **Does not assert:** real-agent startup timing or timeout-fallback behavior (covered by `orchestration/delegate/011` and `/012`); the wrapper-observed branch, where issue #243 replaces this buffer with a longer one measured against a TUI's initialisation (`orchestration/delegate/027`, and `/029` for the latency it costs); that the same unmarked `SessionStart` carrying a FORGED interface marker still pays the buffer (`orchestration/delegate/028`); an upper bound on delivery — the 10 s ceiling only separates "released by the SessionStart" from "released by nothing", since the delegate path's fallback is the bare 30 s `SESSION_START_WAIT_TIMEOUT` with no env override.
- **Platform coverage:** mac+linux.

##### orchestration/delegate/011 — The timeout fallback waits the delegate readiness buffer even when no `SessionStart` arrives, for an agent the deck cannot identify (PRD #249 M1; scoped by issue #243).
- **Layer:** fast synthetic PTY integration (real `handle_delegate` + `clear = true` respawn + daemon broadcast, with Tokio's clock paused to cross the production timeout instantly; no socket, LLM, or `e2e` feature gate).
- **Agent:** hookless `cat` stand-in that never emits `SessionStart`. `AgentType::from_command` resolves `cat` to nothing, so the pane is the neutral registry identity — `PrePromptReadiness::Unknown`, pinned as data by `agent/readiness/001`.
- **Asserts:** after the 30-second fallback expires in virtual time, the pointer remains absent both immediately and 998 ms into the additional 1000 ms readiness buffer, then is delivered after the clock advances to 1001 ms; `1` and whitespace-padded `1` both perform a real wait instead of collapsing to `sleep(0)`; and an integer above `u64::MAX` stays held past the 1000 ms default and releases at the 30 s cap.
- **Read this alongside `orchestration/delegate/030`, which looks like it contradicts it and does not.** The 30 s wait pinned here is no longer what EVERY agent does — since issue #243 it is what an agent whose pre-prompt readiness the deck has NOT established does. `Unknown` answers "is there a signal to wait for?" with `true` on purpose: not knowing what is in the pane is not evidence that skipping the wait is safe, so the conservative behaviour is retained precisely for this case. An agent that has POSITIVELY declared it emits nothing (OpenCode's `NoSignal`, measured in #146) skips the wait entirely — that is `/030`. Both are correct; the difference is whether a measurement exists.
- **Does not assert:** the observed-`SessionStart` branch or whether a real hookless agent is interactive at fallback time; the declared-no-signal skip (`orchestration/delegate/030`); the wrapper-observed release (`/029`).
- **Platform coverage:** mac+linux.

##### orchestration/delegate/012 — A slow-readiness toggle proves the delegate buffer prevents lost payload and submit bytes (PRD #249 M4).
- **Layer:** fast synthetic real-binary-subprocess integration (real `handle_delegate`, respawn, hook socket, managed PTY, and Python raw-mode readiness stub; no LLM and no `e2e` feature gate).
- **Agent:** deterministic slow-readiness stand-in that discards PTY input for 650 ms after `SessionStart`, then echoes accepted bytes in raw mode.
- **Asserts:** changing only `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` loses the pointer at `0`, while `1000` delivers the pointer and its trailing submit CR after the measured input-readiness window.
- **Scope note (issue #243):** the buffer this test is entirely about now applies on a NARROWER set of paths than when it was written, and this test sits on one that kept it. The buffer is scoped by what the readiness gate ESTABLISHED, not by which agent it is: a native `SessionStart`, a hookless wrapper's fork-time event, the wrapper's weaker `wrapper_interface_settled` fact, and the timeout fallback all still hold the prompt (this test injects an UNMARKED `SessionStart`, which the gate treats exactly as a native one); only the wrapper's STRONG `wrapper_interface_ready` fact — the child clearing `ICANON`/`ECHO` — is priced differently, and even then only for an agent the deck itself spawned as a wrapper host and never over an explicitly-set env value. Round 3 retracted the claim that it SKIPS the buffer: raw mode is taken at TUI init, before a composer will accept a submit, so that fact pays the longer `WRAPPER_INTERFACE_READINESS_BUFFER` instead. So the race pinned here is live for every agent — a wrapped one merely pays a different interval against it.
- **Does not assert:** a real Claude or OpenCode timing distribution; the deterministic stub pins the race that real-agent timing cannot reproduce reliably. Nor the wrapper's interface buffer, the one path this 1000 ms value no longer covers (`orchestration/delegate/027`, `codex/wrap/006`).
- **Platform coverage:** mac+linux (unix-only — Python `termios` raw-mode stub).

##### orchestration/delegate/013 — A worker that receives a delegate and then emits no event produces a submitted, actionable orchestrator notice REPORTING WHAT ITS PANE IS SHOWING (PRD #249 M3; issues #686 and #702).
- **Layer:** fast synthetic PTY integration (real `handle_delegate`, managed worker and orchestrator PTYs, the production silence watch, and a shortened worker-response window; no LLM and no `e2e` feature gate).
- **Agent:** none — two silent worker stand-ins plus a raw no-echo orchestrator observer whose scrollback is exactly what the daemon wrote into it. Both workers set `stty raw -echo`, divert everything written into their PTY to a FILE rather than echoing it (a real agent TUI puts a typed prompt into its own input widget, not into the scrollback), and emit no agent event of any kind. They stand in for the agents that emit nothing until their first prompt arrives — Codex and OpenCode, measured; Claude and Pi emit at boot — which is why a silent pane needs no LLM to reproduce: the pane's bytes and the absence of events are both established by the fixture.
- **Asserts:** two arms sharing one delegate path. **Ready-prompt arm:** a worker sitting at a booted agent's own ready prompt receives the task pointer (proved against the delivery log, not the scrollback), and its silence then produces a CR-submitted daemon notice in the ORCHESTRATOR's pane that names the remediation options to keep waiting, re-delegate, reassign, or notify the user. The notice quotes the line that pane is actually rendering — carrying a nonce, so the text provably came from that pane — inside the `[UNTRUSTED-PANE-TEXT: … :END-UNTRUSTED-PANE-TEXT]` frame, and no longer asserts "It may never have received the prompt". **Blank-pane arm (control):** the same delegate to a worker whose pane has drawn nothing is reported as blank, and does NOT carry the other arm's ready-prompt text — without it a notice that always claimed a ready prompt would pass. Both arms also pin that the daemon-authored prose still interpolates no role name (PRD #249 finding B3). Verified load-bearing: reverting the pane read turns the first arm red, collapsing the blank branch into the generic wording turns the second red, and reverting the delivery to `write_notice_guarded` turns the first arm red on `observed terminator = Some(10)`. **How "submitted" is discriminated, and why it is exact:** the observer prints its readiness marker terminated by a bare LF and the fixture asserts the pane delivered that LF unchanged — under a cooked line discipline ONLCR would rewrite it to CRLF, and since `stty raw` applies its whole flag set in one `tcsetattr`, an observed `-opost` is also proof of `-icrnl`, so neither an output- nor an input-side CR/LF translation can sit between the daemon and the assertion. The terminator itself is then read as the single byte following the notice payload's stable final clause, rather than as the first line break anywhere after the notice's opening clause, so an unrelated line break landing in the pane cannot be mistaken for it and a missing CR cannot be papered over by one.
- **Does not assert:** that the pane text is genuinely unreachable as instructions once it lands in an orchestrator's context — the frame is a mitigation, not a proof, and now the only one, since #702 makes this text an agent turn rather than scrollback; `compose_delegate_silence_notice`'s own doc records why the trade was taken and where it differs from the idle prompt's; that a `vt100` replay is *required* to read a pane (a stand-in writing plain lines would also be legible raw — the cursor-addressed case is pinned by `pane_screen_text`'s unit tests instead); tracing output from the companion `warn!`; any first-run-gate string matching, which the fix deliberately does not do; an actual agent response or recovery after the notice (covered by `/024`); that an orchestrator pane holding an unsent human draft has that draft submitted along with the notice, which is issue #544's accepted limitation on every automatic submit and is shared with the idle-worker prompt; and whether a bare LF is inert on every agent, which is moot for this caller now that it submits with CR but stays live for the two notices still on the `write_notice_guarded` path (`scheduler/idle-worker/015`).
- **Platform coverage:** mac+linux (unix-only — raw-mode shell stand-ins).

##### orchestration/delegate/014 — A `clear = true` delegate reaches a REAL interactive Claude worker and the worker visibly acts on it (PRD #249 M4 real-agent happy path). [reel]
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness; flaky-tolerant lane-2 e2e tier, runtime-skipped when the Claude CLI or credentials are unavailable). Imported Claude credentials plus project trust clear onboarding without a keystroke, and the production delegate CLI drives the daemon through its real socket.
- **Opts back into the production readiness buffer, and since issue #243 round 3 that is load-bearing.** `tests/common/mod.rs` pins `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS=0` for every e2e test; `/014` and `/015` opt back in and say why, and this test did not — its own budget comment cited the `0` as a reason the number could be tight. That was defensible only while the strong interface fact skipped the buffer, making a wrapped Codex at `0` and at the default the same run. Since `56c10dd` they differ, and this is the ONE agent for which the buffer is now load-bearing: at `0` the pointer is written at fork + 100 ms and parks unsubmitted in the composer, which is the red this test itself recorded and which retracted the skip. It pins 5000 ms — the PRODUCTION `WRAPPER_INTERFACE_READINESS_BUFFER`, not `/014`'s and `/015`' 1000 ms, since a wrapped Codex released on the strong fact resolves the interface buffer and pinning 1000 would use guard 3's operator override to buy back the very race round 3 closed.
- **Agent:** REAL interactive Claude Code pinned to Haiku (`claude-haiku-4-5-20251001`, `--allowedTools Bash Read Write`, no `-p`) as the `clear = true` `coder` role; the deterministic orchestrator role only invokes the same `dot-agent-deck delegate` CLI a real orchestrator uses. `Write` is allowed so the task file's `## When done` footer (#303) does not park the worker on an approval prompt after the sentinel is created — the sentinel itself is written with Bash.
- **Asserts:** the worker's real prompt editor is visibly ready before delegation; after the delegate respawns it, the role card visibly traverses Thinking → Working with Bash, its native `UserPromptSubmit` hook carries the injected `worker-task-coder.md` pointer (submission rather than PTY echo), and it creates `prd249-claude-respawn-4d37c1.txt` with exact known contents. This proves the happy path against a current real agent; the deterministic `/012` stand-in pins the race itself.
- **Does not assert:** the exact agent response, the measured readiness threshold (covered by `/012`), the timeout-fallback branch (covered by `/011`), or work-done delivery.
- **Platform coverage:** mac+linux (unix-only PTY/UDS; local real-agent tier).
- **Cost note:** one short Haiku worker turn.

##### orchestration/delegate/015 — Post-fix `clear = true` delivery reaches a REAL interactive OpenCode worker and the worker visibly acts on it. [reel]
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness; flaky-tolerant lane-2 e2e tier, runtime-skipped when the OpenCode CLI or credentials are unavailable). Imported OpenCode credentials and `--auto` prevent a permission prompt from blocking the pane; a test-only forwarding env can repoint the production readiness buffer to any value, which is how the pre-fix observation run was made and how the buffer is re-bracketed on a box the shipped figure was not measured on.
- **Agent:** REAL interactive OpenCode pinned to the cheap mini model `openrouter/openai/gpt-4o-mini` (no `opencode run`, no stand-in) as the `clear = true` `coder` role; the deterministic orchestrator role invokes the genuine delegate CLI.
- **Asserts:** the OpenCode TUI is visibly ready before delegation; after the delegate respawns it, the role card visibly traverses Thinking → Working with its shell tool, the OpenCode plugin's native `session.prompt` event carries the injected `worker-task-coder.md` pointer, and it creates `prd249-opencode-respawn-8a62f4.txt` with exact known contents.
- **Also asserts, since issue #243:** that the pointer is SUBMITTED within 20 s of the delegate being released. OpenCode declares `PrePromptReadiness::NoSignal` (measured in #146), so before the fix every `clear = true` delegate to it sat out the full 30 s `SESSION_START_WAIT_TIMEOUT` waiting for an event that cannot arrive and only the fallback delivered — under which every other assertion here still held, which is why the bound is the one thing that tells the fixed path from the broken one. Justified from both ends in `OPENCODE_DELEGATE_TO_SUBMIT_BUDGET`: a full 11 s under the ~31 s the pre-fix path burned (the figure `orchestration/delegate/030` measures in virtual time), and generous above the deck's own contribution — measured at **257 ms**, since `delegate→submit ≈ buffer + 0.26 s` — plus the 8000 ms `NO_SIGNAL_READINESS_BUFFER` this test now pins. **The caveat is discharged and the bound stays 20 s.** "Nobody has measured how long a REPLACEMENT OpenCode needs to consume a pointer handed to it 1 s after spawn" was answered on 2026-08-26 across 176 real-agent runs: the requirement is one observable boundary, the `Ask anything...` composer paint, at 2.5 s idle / 4.5 s contended / 12 s at 4x oversubscription. At the shipped buffer this leg costs ~8.3 s of the 20 s, leaving 11.7 s of margin; do not widen it, since past ~25 s it stops separating a prompt delivery from the dead wait. A slower box gets the operator override, not a bigger budget.
- **Was RED at the shipped defaults on 2026-08-26 — this test had never been executed by anyone before that — and the product changed in `df11513` rather than the test.** The pin above is now 8000 ms, mirroring the `NO_SIGNAL_READINESS_BUFFER` a declared-`NoSignal` agent resolves in production (1.78x the contended requirement, 3.2x the idle one, 19/19 delivered at the shipped value). What follows is the finding that produced it, kept because it is the derivation. The 20 s bound is not what fails and widening it would move nothing: the deck delivers promptly (`delegate exit=0`, pointer file on disk inside 5 s, every run) and that assertion is never reached. The run dies three assertions earlier, on the worker never entering `Thinking` at all — the pointer is written into a replacement OpenCode still bringing its TUI up and the bytes are swallowed outright, with the composer rendering its empty `Ask anything...` placeholder afterwards (PRD #225 Defect 1's shape, a write into a line discipline that is not yet the agent's — not #663's parked-payload shape). Bracketed one full run per value against `DOT_AGENT_DECK_E2E_DELEGATE_READINESS_BUFFER_MS`: **1000 ms FAIL (2/2), 2000 ms FAIL, 5000 ms PASS (25.3 s), 15000 ms PASS (43.0 s)** — so a replacement OpenCode needs between 2 s and 5 s and the shipped value is under it by at least 2x. **A product finding, and a regression this issue introduced:** before #243 an OpenCode delegate sat out the full 30 s `SESSION_START_WAIT_TIMEOUT` for an event measured never to arrive, which was dead time by every measure except one — it happened to give the replacement 30 s to boot. Deleting that dead wait for declared-`NoSignal` agents is right; it left the 1000 ms `DELEGATE_READINESS_BUFFER` carrying a load it was never sized for (PRD #249's "warm-case 500 ms, doubled for a cold start", derived against a 650 ms stub). The fix landed on the `NoSignal` path rather than in this test: a third default sized across 176 runs against real `opencode` 1.18.23, where delivery tracks the composer paint alone and **zero runs parked** the payload — so unlike the wrapper path the loss is monotonic and a longer interval introduces no second failure mode.
- **Does not assert:** exact model phrasing, a universal OpenCode startup-time distribution from one host, the deterministic race (covered by `/012`), or work-done delivery. The sibling `orchestration/delegate/014` carries NO such bound, deliberately: Claude's readiness path is untouched by #243 and is the healthy baseline its budgets are derived from, not one of its victims.
- **Platform coverage:** mac+linux (unix-only PTY/UDS; local real-agent tier).
- **Cost note:** one short GPT-4o-mini worker turn per observation.

##### orchestration/delegate/016 — The generated orchestrator context names what `binary_name()` resolves for the running process, not a baked-in literal (issue prageethw/dot-agent-deck#253).
- **Layer:** pure-data (in-crate `#[cfg(test)]` unit test on `orchestrator_context::build_orchestrator_context`; no TUI harness, no daemon).
- **Agent:** none.
- **Asserts:** with a synthetic role config, the composed context's `delegate` and `work-done` command examples both contain `platform::paths::binary_name()`'s resolution for the running process — under `cargo test` the throwaway test binary is never on `$PATH`, so this is its own absolute `current_exe()` path, never literally `dot-agent-deck` — proving the text is generated from `current_exe()` rather than a hardcoded string.
- **Does not assert:** the symlink-resolution behavior of `current_exe()` itself (a property of the platform, not this crate); the malformed-`current_exe()` fallback branch (`orchestration/delegate/018`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/delegate/017 — The generated worker task file's `work-done` instruction names what `binary_name()` resolves for the running process, not a baked-in literal (issue prageethw/dot-agent-deck#253).
- **Layer:** pure-data (in-crate `#[cfg(test)]` unit test on `state::compose_worker_task_file`; no TUI harness, no daemon).
- **Agent:** none.
- **Asserts:** the composed worker task file's `## When done` footer's `--task-file` and inline `--task` command examples both contain `platform::paths::binary_name()`'s resolution for the running process (the `cargo test` test binary's own absolute `current_exe()` path, which is never literally `dot-agent-deck`).
- **Does not assert:** the malformed-`current_exe()` fallback branch (`orchestration/delegate/018`); the rest of the footer's shell-safety content (covered by the pre-existing `compose_worker_task_file_appends_work_done_footer`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/delegate/018 — The command-name resolver falls back to the crate's default literal only when `current_exe()` itself is unavailable or unusable (issue prageethw/dot-agent-deck#253).
- **Layer:** pure-data (in-crate `#[cfg(test)]` unit test on `platform::paths::resolve_binary_name`, the pure seam behind `binary_name`; no TUI harness, no daemon).
- **Agent:** none.
- **Asserts:** an `Err` result, a path with no file name (`/`), and (Unix-only) a non-UTF-8 file name all resolve to `DEFAULT_BINARY_NAME` (`env!("CARGO_PKG_NAME")`) rather than panicking or producing an empty string. A well-formed `current_exe()` whose bare file name is merely shell-unsafe or absent from `$PATH` does NOT fall back to this literal — it falls back to the absolute `current_exe()` path instead (`platform::paths::resolve_binary_name_falls_back_to_the_absolute_path_when_the_name_is_shell_unsafe`/`_not_on_path`, plain `#[test]`s alongside this one, not separately cataloged).
- **Does not assert:** a real `current_exe()` failure (not reproducible on demand); the happy path (`orchestration/delegate/016`–`017`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/delegate/019 — A same-named binary shadowing the running executable earlier on `$PATH` is rejected by identity, not merely resolved (issue prageethw/dot-agent-deck#253 `$PATH`-identity tightening).
- **Layer:** pure-data (in-crate `#[cfg(test)]` unit test on `platform::paths::path_identity_match` and `platform::paths::resolve_binary_name`; no TUI harness, no daemon).
- **Agent:** none.
- **Asserts:** with a synthetic `$PATH` value (never the real process-global `PATH`) listing two directories that each hold an executable file sharing one basename — a "shadow" file first, the "real" (`current_exe()`-standing-in) file second — `path_identity_match` reports no match for the shadow-first ordering and a match once the roles are reversed (proving the rejection is genuinely about file identity, not mere absence), and `resolve_binary_name` driven through that shadow-first `$PATH` falls back to the shell-quoted absolute `current_exe()` path rather than emitting the bare name a consuming shell would resolve to the shadowing binary.
- **Does not assert:** the real process-global `$PATH` (a synthetic value is used throughout); the empty/relative-`$PATH`-entry branch (`platform::paths::is_untrustworthy_path_entry_rejects_empty_and_relative_but_accepts_absolute`, a plain `#[test]` alongside this one, not separately cataloged).
- **Platform coverage:** mac+linux+windows.

##### orchestration/delegate/020 — The bare-name success branch is reached against a REAL `current_exe()` on a REAL `$PATH` — PR #520's whole motivating scenario, previously untested (prageethw/dot-agent-deck#253 round-4 verification, finding 1).
- **Layer:** L2 (in-process daemon whose `handle_delegate` fan-out composes the worker task file; a `cat`-stub worker PTY via `AgentPtyRegistry::spawn_agent`, no real agent — the `e2e` tier, no LLM call). Entry point is a sync `#[test]` that `block_on`s an async body (the linkage-check scanner links `#[spec]` to the next PLAIN `fn`, so a `#[tokio::test] async fn` would misbind — same pattern as `chain-smoke/pi/002`).
- **Agent:** none (`cat` stub; only the generated file is under test).
- **Asserts:** with the built deck binary's own directory prepended to this process's `$PATH` (the deck's normal on-`PATH` install shape) and `spawn_inprocess_daemon`'s test-current-exe override injecting the real built `dot-agent-deck` binary as `binary_name()`'s effective `current_exe()`, delegating a task writes `.dot-agent-deck/worker-task-coder.md` whose `work-done` instruction names the BARE binary (`dot-agent-deck work-done --task-file …`) — not the quoted absolute-path fallback every other `binary_name()` test in this repo exercises, and not the running libtest binary's own path (the regression this issue's round-4 verification found: without the override, an in-process daemon's `handle_delegate` runs in the TEST process, so `binary_name()` correctly-for-that-process named the libtest binary, and a real worker following the generated command hit libtest's CLI parser instead of the deck's).
- **Does not assert:** a real agent following the generated command (covered, for the two real-agent arms this regression broke, by `delegate_work_done_chain_claude` and `chain-smoke/pi/002`, both now fixed by the same override); the malformed-`current_exe()` fallback (`orchestration/delegate/018`); the `$PATH`-identity-shadowing rejection (`orchestration/delegate/019`).
- **Platform coverage:** mac+linux (unix-only PTY/UDS; `spawn_inprocess_daemon` is `#[cfg(unix)]`).

##### orchestration/delegate/021 — Work-done completion does not make the next same-pointer delegate disappear after the user types an unsent draft.
- **Layer:** fast synthetic PTY integration (real `handle_delegate` and `handle_work_done`, managed worker and orchestrator PTYs, and production silence-watch accounting; no socket or LLM).
- **Agent:** none (`cat` worker stand-in plus a raw no-echo orchestrator observer).
- **Asserts:** delegation A's fixed `worker-task-coder.md` pointer physically reaches the worker, real work-done handling retires A, an unsent user draft then physically reaches the same pane, and delegation B produces another observable copy of the same pointer; independently, a late completion for an older of two live delegations leaves the newer delivery's no-event notice armed.
- **Does not assert:** the payload guard's records or refusal reason, exact task-file contents, or which safe mechanism admits B; the outcome is solely that B is physically delivered after A completed.
- **Platform coverage:** mac+linux.

##### orchestration/delegate/022 — A `clear = true` delegate that lands while the worker's pane is mid-close brings the role back instead of killing it for the session (issue #606).
- **Layer:** fast integration (the daemon's real `StopAgent` handler over its attach socket, the real `handle_delegate_with_state`, and daemon-owned PTYs; no LLM and no `e2e` feature gate).
- **Agent:** none — a worker stand-in that IGNORES SIGTERM (`trap '' TERM; exec cat`), so `close_agent` spends its full 3 s `AGENT_TERMINATE_GRACE` and the close is genuinely still in flight 200 ms later. A plain `cat` dies on the first signal and the whole close is over well inside that window, so it cannot reproduce the race at all — the stand-in's stubbornness is the fixture's entire point. The replacement's readiness signal is injected over the hook socket, as the rest of the fast delegate suite does for a stand-in.
- **Asserts:** the precondition that the close IS still in flight when the delegate lands (`pane_close_in_flight`), so the test cannot silently degrade into an ordinary post-close delegate; then that the worker pane gets a live agent again, that agent physically receives the `worker-task-coder.md` pointer, and the role is STILL registered in `pane_role_map` afterwards — the last one because without it the delivery succeeds while the NEXT delegate is rejected with `reached no worker for role(s)`, which is the permanent breakage #606 reports. Verified load-bearing: reverting the recreate, or only the settle-wait that precedes it, turns it red.
- **Does not assert:** what the TUI paints for the recovered card (the daemon-side re-creation is what is under test; live surfacing of a daemon-spawned pane is PRD #222's); the close's own outcome, which is awaited only so the test does not outrun it.
- **Platform coverage:** mac+linux (unix-only — the stand-in is a POSIX shell script).

##### orchestration/delegate/023 — A `clear = true` replacement worker that dies before it is ready is REPORTED to the orchestrator, promptly, instead of costing 31 s of silence and then nothing (issue #584).
- **Layer:** fast integration (real `handle_delegate` against daemon-owned PTYs; no LLM and no `e2e` feature gate).
- **Agent:** none — a stand-in that refuses to start once the TEST drops a `die` marker beside it, so "the replacement dies before it is ready" is a fact the test establishes rather than a race it hopes for. The `cat` orchestrator observes what the daemon writes into its pane.
- **Asserts:** the precondition that the FIRST worker is up before the refusal is armed; then that a notice naming the worker's pane appears in the ORCHESTRATOR's own pane, within a budget of **5 s** — so the assertion covers both halves of the fix, the report and the promptness. Also that nothing was written into the dead pane, and that the notice interpolates no role name (PRD #249 finding B3's precedent for this notice family). Verified load-bearing: reverting either the EOF-driven end to the readiness wait or the liveness gate turns it red.
- **Budget re-derived in issue #243, from a measurement instead of from the alternative.** It was 20 s, justified as "well under the `SESSION_START_WAIT_TIMEOUT` + readiness buffer (31 s) the pre-fix path burned" — a ceiling picked to sit under the thing it replaced. That reasoning expired twice: #584 itself ended the readiness wait on the replacement's PTY reaching EOF, and #243 then removed the dead wait outright for declared-no-signal agents, so 31 s is nobody's behaviour and a 20 s ceiling on a ~0.1 s operation asserted approximately nothing. Measured on this branch at **103.1-104.1 ms** idle and **54.4-108.4 ms** across eight runs with all 16 cores saturated and a concurrent full fast tier — the figure is set by the fixture's own 50 ms poll and barely moves under load, because the notice is driven by the child's exit and not by a timer. 5 s is ~46x the slowest measurement (room for a CI runner an order of magnitude slower) and still 6x under the 30 s a reverted EOF-driven wait would cost. Deliberately not tighter: this is an upper bound on a fast event, so unlike `orchestration/delegate/010`'s lower bound it IS the load-sensitive direction.
- **Does not assert:** WHY a real replacement dies — #584's own trigger was environment-side and is not reproduced here (see `orchestration/dispatch/003` for the parity control that rules out the reported hypothesis); any retry of the delegate, which this fix deliberately does not add; the daemon-log `warn!`, which carries the role and command the notice omits.
- **Platform coverage:** mac+linux (unix-only — the stand-in is a POSIX shell script).

##### orchestration/delegate/024 — A real interactive Haiku orchestrator visibly acts on a delegated-worker silence notice without a human pressing Enter (issue #702). [reel]
- **Layer:** L2 PTY-attached (real `dot-agent-deck` binary and lazy daemon, with a restored orchestration rendered through the vt100 `TuiDeck` harness). Flaky-tolerant lane-2 tier; run once, not looped.
- **Agent:** REAL interactive Claude Code orchestrator pinned to Haiku (`claude-haiku-4-5-20251001`, `--allowedTools Bash`, no `-p`) plus a long-lived `cat` worker that receives the delegate pointer and emits no agent event. Runtime-skipped when the Claude CLI or credentials are unavailable — set `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` to turn that skip into a hard failure on a run that must genuinely exercise the agent.
- **Asserts:** the real orchestrator follows a directive to run the genuine `dot-agent-deck delegate` CLI (proved by the daemon-created worker task file), visibly acknowledges that it is waiting, returns to Idle, and has not prematurely created the action sentinel. After the no-event window expires, with no test keystroke sent at any point, the orchestrator's card visibly traverses Thinking then Working, creates a uniquely named sentinel with exact contents, and produces a response turn carrying the directive's unique completion marker. The post-Idle lifecycle plus sentinel proves the daemon notice was submitted as a new actionable turn rather than merely written into the pane.
- **Does not assert:** exact model prose beyond the directive's unique completion marker, which remediation choice a production orchestrator should prefer, or a real silent worker agent — `orchestration/delegate/013` and `/025` deterministically pin the notice bytes, pane evidence, and generation accounting without an LLM.
- **Platform coverage:** mac+linux.

##### orchestration/delegate/025 — A silence watch cannot report a worker generation after a newer `clear = true` respawn has taken over its pane (issue #687).
- **Layer:** fast synthetic PTY integration (real `handle_delegate`, two `clear = true` respawns on one managed worker pane, the production readiness wait and silence watches, and a raw no-echo orchestrator observer; no LLM and no `e2e` feature gate).
- **Agent:** none — a deterministic shell stand-in renders a distinct nonce-bearing sentinel on generation A and generation B, accepts submitted payload bytes without emitting any agent event, and lets the test identify which generation a notice quoted.
- **Asserts:** generation A renders its sentinel, receives its pointer and arms a silence watch; generation B then visibly takes ownership of the same pane before A's short window expires and remains inside its longer readiness wait with no payload of its own. No silence notice may reach the orchestrator during that gap and no later notice may quote A's sentinel. As the non-vacuous control, B subsequently receives its own pointer and its own watch remains armed long enough to fire exactly one notice quoting B's distinct sentinel.
- **Does not assert:** internal silence-watch sequence numbers, retirement variants, or the implementation seam where supersession is recorded; the observable contract is solely which generation's pane evidence reaches the orchestrator.
- **Platform coverage:** mac+linux (unix-only — raw-mode POSIX shell stand-ins and managed PTYs).

##### orchestration/delegate/026 — The wrapper's WEAK interface fact (output settled) does not release the delegate gate; the pointer is held until the STRONG one upgrades it, and then priced as an interface observation (issue #243, `INTERFACE_UPGRADE_WINDOW`).
- **Layer:** fast synthetic PTY integration (real `handle_delegate` + `clear = true` respawn + in-process daemon hook socket + the REAL `dot-agent-deck wrap` rewrite at the common spawn boundary; no LLM and no `e2e` feature gate).
- **Agent:** a `codex`-named stand-in that paints a nonce banner, stays in COOKED mode for 2 s, and only then runs `stty raw -echo` — the production launch shape in miniature. `devbox run codex-big` prints one banner at ~0.1 s and computes its shellenv in silence for a measured 2750–4132 ms before `codex` is exec'd at all, so the weak fact fires while a LAUNCHER still owns the line discipline and the strong one arrives 2005–3370 ms behind it, 21 times out of 21. This is the only fixture in the suite that exercises the upgrade, and `InterfaceWatch::claim` latching per FACT rather than per session is what makes a second event possible at all.
- **Asserts:** two controls — that the FIRST fact was `wrapper_interface_settled` (a run where the strong one came first has silently degraded into `/027`'s single-fact case) and that the strong fact was stamped strictly AFTER it (else the dwell failed to separate them); then the bound, that the pointer landed at least 5000 ms after the STRONG fact. That one bound does two jobs: the gate did not release on the guess that beat it here by ~1.26 s, and what it did release on was priced as an interface observation. Plus the user-visible half, that a worker whose banner has been on the pane the whole time still waited. Measured **5.007 s** past the strong fact, ~6.27 s past the weak one.
- **Re-founded in round 3, because the old test passed with the guard deleted.** It used to assert a 1000 ms LOWER bound under a 10 s ceiling, on the theory that the weak fact releases the gate and pays the ordinary buffer. `46ccca1` made the upgrade window `SESSION_START_WAIT_TIMEOUT`, so a weak fact that never upgrades is released by window-expiry at ~30 s and *then* pays 1000 ms — landing in the same instant as "released by nothing at all", which no ceiling can separate from it, and satisfying a 1000 ms floor whether or not any guard exists. Anchoring on WHICH FACT released the gate, rather than on how long the release took, is what makes it falsifiable again.
- **`DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` is deliberately UNSET, and that is load-bearing rather than tidiness.** Guard 3 makes an explicitly-set value win over BOTH defaults, so with it pinned the two buffers collapse to one number and the bound stops distinguishing them.
- **Verified load-bearing:** deleting the upgrade window (`interface_upgrade_window` returning `ZERO` unconditionally) releases the gate on the guess and delivers the pointer **6.23 ms** after the strong fact against the 5000 ms bound — and 1.269 s after the weak one, i.e. the ordinary buffer paid off a launcher's silence.
- **Does not assert:** that a real Codex produces both facts in this order (measured in the issue at 21/21 for `devbox run codex-big`, not re-derived here; `orchestration/delegate/009` is where a real one runs); what the window's VALUE should be, only that the weak fact does not release before the strong one arrives; the single-fact path (`orchestration/delegate/027`); the window EXPIRY fallback, where no strong fact ever comes — that shape is what `/029`'s old fixture became and no test now pins it, since its outcome is indistinguishable in time from an unready fallback.
- **Platform coverage:** mac+linux (unix-only — the stand-in is a POSIX shell script and both facts depend on pty line discipline).

##### orchestration/delegate/027 — The wrapper's STRONG interface fact (the child took raw input mode) is priced at the 5000 ms interface buffer rather than the ordinary 1000 ms, and an operator-pinned buffer replaces it in both directions (issue #243, guards 1–3).
- **Layer:** fast synthetic PTY integration (same shape as `/026`; two arms in one test process so both see the same fixture).
- **Agent:** a `codex`-named stand-in that runs `stty raw -echo` BEFORE writing a byte and then paints its banner. Ordering is deliberate: the watch checks line discipline first and the settle window only as a fallback, so clearing `ICANON`/`ECHO` with no output yet makes fact 1 the only fact that CAN fire, instead of leaving it to a race between the wrapper's 50 ms supervisory poll and its 750 ms settle window.
- **Asserts:** in both arms, the control that the announced origin really is `wrapper_interface_ready` AND that `agent_spawned_as_wrapper_host` admits this replacement — the second is the fail-closed alarm, and round 3 did not weaken it: guard 2 refuses toward the SHORTER buffer, so a version that turned down every honest agent would leave the deck writing into a still-initialising codex-cli with every other assertion in the suite green. **Arm 1** (variable REMOVED, so the deck's own defaults decide): the pointer is held at least 5000 ms past the interface event, under a 10 s ceiling that separates "released by this fact" from "released by the timeout" — measured **5.008 s**. **Arm 2** (`DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS=1500`, deliberately neither default): the same fact on the same fixture is held at least 1500 ms and at most 3 s — measured **1.509 s**.
- **Round 3 inverted arm 1 and made arm 2 two-sided.** Arm 1 asserted `held <= 700 ms` for two rounds — that the strong fact SKIPPED the buffer — and the premise was measured false: a full-screen TUI takes raw mode at INIT (real codex-cli 85 ms after exec, `orchestration/delegate/009` at fork + 100 ms), so writing then is the earliest and worst instant available and `/009` lost the pointer into an unsubmitted composer exactly as production did. There is no skip left to pin. Arm 2's lower bound was likewise vacuous once the skip became a second buffer — with guard 3 deleted the strong path pays 5000 ms, which clears a 1500 ms floor — so it now bounds above as well, which is also the honest statement of what guard 3 promises: the operator's value OVERRIDES both defaults rather than being max()-ed against them, and a value above the pin violates that as much as one below it.
- **Verified load-bearing, per arm and per guard.** Pricing the strong fact with `delegate_readiness_buffer()` (guard 1 dropped, and the same shape a fail-closed guard 2 produces) turns arm 1 red at **1.023 s** against its 5000 ms bound. Ignoring the operator's setting on the interface path (guard 3 dropped) turns arm 2 red at **5.012 s** against its 3 s ceiling, while arm 1 stays green — which is what isolates the two. Neutering `agent_spawned_as_wrapper_host` outright is caught earlier still, by arm 1's control.
- **Does not assert:** a real Codex releasing on this fact (`orchestration/delegate/009`); the fact-2-then-fact-1 upgrade, which this fixture cannot produce (`orchestration/delegate/026`); that 5000 ms is ENOUGH for any particular machine — it is a mitigation sized from measurement, not a bound, which is why the operator override exists; the scheduler's copy of the gate, which has no interface path.
- **Platform coverage:** mac+linux (unix-only — POSIX shell, `stty`, and pty line discipline).

##### orchestration/delegate/028 — A forged `wrapper_interface_ready` marker for a pane the daemon never spawned as a wrapper host releases the gate but is priced as an ORDINARY readiness fact, never as a real wrapper's observation (issue #243 audit F1, guard 2).
- **Layer:** fast integration (real `handle_delegate` + `clear = true` respawn + in-process daemon hook socket; no wrapper, no pty fixture, one crafted `AgentEvent`).
- **Agent:** a plain `cat` worker — a command `AgentType::from_command` cannot resolve, so the frozen `spawn_agent_type` is `None` and the daemon has no standing to believe anything about that pane's interface.
- **Asserts:** the control that the pane really is not a wrapper host (else the marker would be honest); then that the pointer still ARRIVES — releasing the gate was forgeable before #243 by a bare unmarked `SessionStart` and still is, so withholding delivery is not the property under test — and then a TWO-SIDED bound on how long it was held, measured from before the line even hits the socket: at least the deck's own 1000 ms, and at most 3 s. Measured **1.006 s**. This is the audit's own reproduction (a bare `python3` with no deck environment writing one JSON line to the daemon socket) turned into a regression test.
- **The upper bound is the guard-2 assertion; the floor is not.** This entry claimed "dropping guard 2 delivers in 21.1 ms" and that proof expired with `56c10dd`: once the SKIP became a second buffer, dropping guard 2 stopped delivering instantly and started delivering after `WRAPPER_INTERFACE_READINESS_BUFFER`, which sails over a 1000 ms floor. Measured — the test stayed green with `agent_spawned_as_wrapper_host` deleted from the seam. What guard 2 is worth is now ATTRIBUTION rather than privilege (a forgery can no longer suppress a buffer, only mis-price one toward the value every other agent already gets), so telling 1000 ms from 5000 ms is the only way to observe it at all.
- **Verified load-bearing (re-measured round 3):** dropping the `agent_spawned_as_wrapper_host` term from the seam holds the pointer **5.017 s**, red against the 3 s ceiling. Like `/026`, the buffer variable must stay UNSET for that to be true — with it pinned, guard 3 collapses both defaults to one number and the test would pass with guard 2 deleted.
- **Does not assert:** that the marker is unforgeable — it is not, and the fix is provenance rather than authentication; the honest case guard 2 also refuses (a role command that already names the wrapper, whose frozen identity `from_command` cannot recover — which since round 3 costs it the LONGER buffer rather than the fast path, i.e. it waits the same interval every non-wrapper agent has always waited, documented at the oracle and logged with a `warn!`); the gate-release path itself (`orchestration/delegate/010`).
- **Platform coverage:** mac+linux (unix-only — daemon-owned PTYs).

##### orchestration/delegate/029 — A wrapped worker that emits no pre-prompt native `SessionStart` still gets its delegated pointer promptly, instead of paying the 30 s fallback while sitting visibly at its ready prompt (issue #243).
- **Layer:** fast synthetic PTY integration (real `handle_delegate` + `clear = true` respawn + in-process daemon hook socket + the REAL `dot-agent-deck wrap` rewrite applied at the common spawn boundary; no LLM and no `e2e` feature gate).
- **Agent:** a `codex`-named `cat` stand-in that clears `ICANON`/`ECHO`, paints a nonce-carrying ready prompt and then accepts input, emitting no hook event of its own — codex-cli's measured shape, where the native `SessionStart` fires when the first TURN starts and is therefore CAUSED by the prompt the gate is withholding. The `stty raw -echo` is round 3's repair and it is load-bearing: as a bare `printf …; exec cat` this fixture never left cooked mode, so once the upgrade window became `SESSION_START_WAIT_TIMEOUT` it modelled a line-oriented REPL waiting the timeout out BY DESIGN rather than a worker at its prompt, and went red at 30.98 s. A real Codex clears the flags, so the fixture does too.
- **Asserts:** the control that the replacement genuinely painted its ready interface — the user-visible "the agent is booted and healthy at its prompt" this issue reports the deck ignoring — and then that the task pointer reaches that pane within 10 s of it. The budget is justified from both ends in `READY_TO_POINTER_BUDGET`, and round 3 moved one term rather than re-guessing it: it was 6 s against a 1000 ms buffer, i.e. 5 s of slack over everything the deck does on this leg; the fixture now releases on the strong fact and pays the 5000 ms `WRAPPER_INTERFACE_READINESS_BUFFER`, so the same 5 s of slack lands at 10 s. Three times under `SESSION_START_WAIT_TIMEOUT` and three times under the ~31 s every unreleased path costs. On failure it keeps looking up to 34 s purely to MEASURE the delay, so a red carries a before-number rather than only a verdict. GREEN since the fix, at **5.044 s** measured ready→pointer (was 30.98 s before it, and 30.98 s again on the old cooked-mode fixture after `46ccca1`).
- **Verified load-bearing:** making the gate refuse to release on either interface fact (`session_start_means_ready` returning `false` for a wrapper interface start) turns this red at **30.98 s** against its 10 s budget — the exact pre-fix figure.
- **Does not assert:** which mechanism supplies the readiness (the wrapper-side signal is `codex/wrap/006`); WHICH buffer the strong fact is priced at, only that delivery is prompt (`orchestration/delegate/027`); the fact-2-then-fact-1 upgrade this fixture deliberately cannot produce, since it goes raw before writing a byte (`orchestration/delegate/026`); real codex-cli boot timing and the latency of the real path (`orchestration/delegate/009`); the buffer's own behaviour once released (`orchestration/delegate/010`, `/012`); the registry data the gate reads (`agent/readiness/001`, `/002`); the OpenCode half, which has no signal to wait for at all (`orchestration/delegate/030`).
- **Platform coverage:** mac+linux (unix-only — the stand-in is a POSIX shell script).

##### orchestration/delegate/030 — A worker whose agent has NO pre-prompt readiness signal skips the 30 s dead wait entirely and is delivered after a bounded buffer only (issue #243, the OpenCode half of #146).
- **Layer:** fast synthetic PTY integration (real `handle_delegate` + `clear = true` respawn + daemon broadcast, with Tokio's clock paused so 31 s of gate is crossed in a 2 s test; no socket, LLM, or `e2e` feature gate).
- **Agent:** an `opencode`-named `cat` stand-in, asserted through `AgentType::from_command` so it is the OpenCode CONFIGURATION rather than an anonymous stub — a Plugin-strategy agent the deck does not wrap, whose plugin bus was measured carrying no pre-prompt event at all (`session.created` arrives 16 ms AFTER the prompt is accepted, #146). Nothing in the test ever emits an event, which is exactly that agent's cold-boot stream.
- **Asserts:** the control that the fixture resolves to `AgentType::OpenCode`, then that the pointer reaches the pane within 12 virtual seconds — and, since round 4, no sooner than 7. Virtual time is walked forward a second at a time rather than jumped, both so a failure reports the real figure and so a correct two-stage fix (a shortened wait that only then arms a buffer) cannot read as a false red, which a single `advance` would produce. GREEN since the fix: **31 s → 8 s** of virtual time, i.e. the whole `SESSION_START_WAIT_TIMEOUT` is gone and what remains is exactly the buffer.
- **Since round 4 this is also the ONLY test that pins the CALL SITE, and `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` is UNSET so that it can be.** Nothing else asserts that the declared-no-signal skip resolves `no_signal_readiness_buffer()` rather than the ordinary `delegate_readiness_buffer()`. This test pinned the variable to 1000 ms for three rounds, which collapses all three defaults to the pinned number and stays green through exactly that revert; its real-agent sibling `orchestration/delegate/015` pins it too, so it cannot catch it either. Unset, the run resolves the shipped 8000 ms `NO_SIGNAL_READINESS_BUFFER` for real, and the 7 s floor reads it back — chosen to reject the two wrong answers (1 s ordinary, 5 s wrapper-interface) by a whole 1 s step either side rather than pinning 8 exactly. **Verified load-bearing:** reverting the seam in `src/state.rs` to `delegate_readiness_buffer()` delivers at **1 s** of virtual time and turns this red. The 12 s ceiling is split out from `/029`'s `READY_TO_POINTER_BUDGET` for the same reason — that 10 s is derived as "the 5000 ms interface buffer plus 5 s", which is not this path's buffer — and is still 2.6x under the ~31 s a regression to the dead wait costs.
- **Does not assert:** that OpenCode has no such signal (measured upstream in #146, not re-derived here — the registry value it rests on is pinned as data by `agent/readiness/001`); the wrapper half, which needs a different mechanism (`orchestration/delegate/029`, `codex/wrap/006`); how the 8000 ms figure was DERIVED, which is 176 real-agent runs recorded in `state::NO_SIGNAL_READINESS_BUFFER` and exercised end to end by `orchestration/delegate/015` — this test only reads back which constant the seam selected; the same skip on the SCHEDULER's copy of this gate (`src/spawn.rs`), which no test covers and which the issue measured separately at 30.3 s → 1.5 s; the real-agent end of it (`orchestration/delegate/015`).
- **Platform coverage:** mac+linux (unix-only — the stand-in is a POSIX shell script).

#### orchestration/work-done

##### orchestration/work-done/001 — A `work-done` from a worker with NO outstanding delegation is reported to the orchestrator as unsolicited, and does not overwrite the last commissioned report (issue #448).
- **Layer:** fast integration (real `handle_work_done` against daemon-owned PTYs; `cat` stand-ins, no LLM and no `e2e` feature gate).
- **Agent:** none (a raw no-echo `cat` orchestrator observer, so one daemon submission appears exactly once in its snapshot, plus a `cat` worker).
- **Asserts:** with an earlier delegation's report already parked at `.dot-agent-deck/work-done-coder.md` and nothing delegated, the worker's `work-done` produces feedback carrying the daemon's unsolicited label (`you have no outstanding delegation to that worker`); the happy-path pointer (`Read .dot-agent-deck/work-done-coder.md for their full report.`) is ABSENT; the worker's own report still reaches the orchestrator inline, framed as `[UNTRUSTED-WORKER-REPORT: … :END-UNTRUSTED-WORKER-REPORT]`; and the earlier report is still on disk byte-for-byte.
- **Does not assert:** what the orchestrator then DOES with the label (an LLM decision); the commonest sibling trace — a worker redirected by a human mid-delegation, which produces a well-formed completion for a diverged task and involves no defect (#445/#369); the ledger's own arithmetic (unit-tested on `AgentPtyRegistry::retire_delegation_commission`).
- **Platform coverage:** mac+linux (unix-only — raw-mode shell observer).

##### orchestration/work-done/002 — With the idle detector switched OFF (`worker_response_timeout_minutes = 0`) a genuinely delegated completion is still reported normally, and is never labelled unsolicited (issue #448's decisive regression guard).
- **Layer:** fast integration (real `handle_delegate` + `handle_work_done` against daemon-owned PTYs; `cat` stand-ins).
- **Agent:** none (raw no-echo `cat` orchestrator observer plus a `cat` worker).
- **Asserts:** on a project config whose `worker_response_timeout_minutes = 0` leaves BOTH delegation watches arming nothing at all, a delegate followed by the worker's `work-done` still submits the unchanged pointer into the orchestrator pane, carries no unsolicited label and no inlined report frame, and writes the fresh report to `.dot-agent-deck/work-done-coder.md`. This is the case that rules out inferring "never delegated" from `DelegationRetirement::Nothing`: doing so would have silently stopped reporting completions for every project that has turned the detector off.
- **Does not assert:** the detector's own firing behaviour with a positive timeout (`scheduler/idle-worker/*`); the millisecond test seam; the exact task text delegated.
- **Platform coverage:** mac+linux (unix-only — raw-mode shell observer).

##### orchestration/work-done/003 — When the summary file cannot be written, the orchestrator is told so and receives the report inline instead of a pointer to a path the daemon never wrote (issue #433).
- **Layer:** fast integration (real `handle_delegate` + `handle_work_done` against daemon-owned PTYs; `cat` stand-ins).
- **Agent:** none (raw no-echo `cat` orchestrator observer plus a `cat` worker).
- **Asserts:** with `.dot-agent-deck` occupied by a regular FILE — so `create_dir_all` and the write both fail, for uid 0 as well — a delegated worker's `work-done` produces feedback stating the deck `could not write .dot-agent-deck/work-done-coder.md`, with the happy-path pointer ABSENT and the worker's report inlined inside the untrusted-report frame. The daemon still holds the summary in memory at the moment it gives up on the file, so the failure degrades to a worse-formatted report rather than to a confidently wrong one.
- **Does not assert:** the no-cwd and mkdir-only variants of the same failure (unit-tested on `write_work_done_summary`); the stale-file READ itself (the pointer's absence is what makes it unreachable); recovery of the file on a later delegation.
- **Platform coverage:** mac+linux (unix-only — raw-mode shell observer).

##### orchestration/work-done/004 — On the REAL binary, an unsolicited `work-done` renders its label and the worker's framed report in the attached TUI's orchestration surface, with no pointer to a file that was never written (issues #448 + #433).
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness, with its lazy daemon; the completion is issued by running the REAL `dot-agent-deck work-done` CLI against the deck's own hook socket, so the spawned-binary → hook-socket → daemon → rendered-pane boundary is covered end to end). Synthetic (`cat` roles, no LLM), so deliberately NOT demo-reel-marked.
- **Agent:** none (the `orch-deck` fixture's two `cat` roles: `orchestrator` start + `worker`). Both delegation watches are switched off via the millisecond seams so no detector competes for the surface under assertion.
- **Asserts:** with the orchestration opened through the production new-pane flow and NOTHING delegated, the real `work-done` CLI exits 0 and the rendered orchestration surface visibly carries the daemon's unsolicited label (`you have no outstanding delegation to that worker`) plus its provenance clause, and the worker's own report inside `[UNTRUSTED-WORKER-REPORT: … ]` (sentinel `e2e-unsolicited-report-4b7d`); the happy-path pointer (`Read .dot-agent-deck/work-done-worker.md …`) is ABSENT; and no `work-done-worker.md` exists on disk. Needles are matched with whitespace squeezed out of both sides, because a long daemon-injected line wraps at whatever column a role pane happens to be — the failure mode `scheduler/idle-worker/011` demonstrates.
- **Does not assert:** the failed-summary-write branch (deterministic only by sabotaging the coordination path, so it stays fast-tier as `orchestration/work-done/003`); the commission ledger's arithmetic (unit-tested); a real agent reading and acting on the label (an LLM decision, not a rendering fact).
- **Platform coverage:** mac+linux (unix-only PTY/UDS).

##### orchestration/work-done/005 — A `clear = true` delegate whose respawn FAILS releases its commission, so the next uncommissioned `work-done` from that worker is still reported as unsolicited (issue #448 review, round 2).
- **Layer:** fast integration (real `handle_delegate` + `handle_work_done` against daemon-owned PTYs; `cat` stand-ins).
- **Agent:** none (raw no-echo `cat` orchestrator observer; the worker's `cat` is live, and the role COMMAND points at a binary that does not exist).
- **Asserts:** with the `coder` role at `clear = true` and BOTH delegation watches switched off (`worker_response_timeout_minutes = 0`), a role command naming a nonexistent single-word binary makes the respawn dispose of the live worker and then fail deterministically at `spawn_agent`, and the dispatch takes its respawn-error return. (Until issue #606 the failure was forced by EVICTING the worker's agent so the respawn failed `NotFound`. That is no longer a failure — a `clear = true` delegate to a pane whose agent is simply gone now re-creates the worker instead of leaving the role unreachable — so the mechanism moved to a genuine spawn failure, which is also the production hazard this test describes, stated literally. The release behaviour under audit is unchanged.) The orchestrator first sees the daemon's `⚠ respawn failed for role 'coder'` notice — the test's synchronization edge, so no timing is guessed at — and a subsequent `work-done` from that same worker is then reported with the unsolicited label, with the happy-path pointer ABSENT and no `work-done-coder.md` on disk. Confirmed red with only the release removed: the completion arrives as `Worker coder has completed their task. Read .dot-agent-deck/work-done-coder.md …`, which is #448 and its clobber reproduced through the ledger added to prevent them. Both detectors are off on purpose — the release must be independent of them, exactly as the arming is.
- **Does not assert:** the guarded-send refusal arm's release (unit-tested arithmetic plus a manual real-binary run; forcing an undelivered guarded send here would depend on the spawned dispatch task winning a race); the pi-native seed return, which is a DELIVERY path and correctly keeps its commission; the readiness-buffer close return, where `begin_pane_close`'s sweep is the discharge.
- **Platform coverage:** mac+linux (unix-only — raw-mode shell observer).

#### orchestration/identity

##### orchestration/identity/001 — Opening an orchestration whose form/display name (worktree dir basename) differs from the TOML config orchestration name stamps the CANONICAL config name as the daemon IDENTITY, not the basename (PRD #107 regression).
- **Layer:** L1 (in-process — dispatch the real `Action::SpawnPane` through `dispatch_action` against a recording `PaneController`; no daemon, no PTY).
- **Agent:** none (stub role commands; orchestration_config carries `name = "dot-agent-deck"` with a `coder` role at `clear = true`).
- **Asserts:** when the new-pane form's Name field defaults to the worktree basename (`dot-agent-deck-prd-113-foo`) while the config name is `dot-agent-deck`, every role pane's `TabMembership::Orchestration.name` (the IDENTITY the daemon's `lookup_orchestration_role` compares) equals the canonical config name `dot-agent-deck` — so the role resolves and `clear = true` respawn fires — while the tab TITLE (`Tab::Orchestration.name`) still shows the basename. Pre-fix the PRD #107 SpawnPane override copies the basename into `orch_config.name`, so the identity is the basename and the lookup misses.
- **Does not assert:** the daemon-side `pane_orchestration_map` recording or the live delegate respawn (L2 path); the on-disk config reload inside `lookup_orchestration_role`.
- **Platform coverage:** mac+linux+windows.

##### orchestration/identity/002 — Selecting the form's orchestration (Right arrow) suggests `<folder>-orchestrator-1` in the Name field in place of the bare directory basename it was pre-filled with, when no orchestration is live yet; a single further keystroke (Enter, no character typed) accepts it as-is at submit.
- **Layer:** L1 (`src/ui.rs`'s own `#[cfg(test)] mod tests` — the real `handle_new_pane_form_key` path against a `NewPaneFormState` built with the bare-basename pre-fill `transition_after_dir_pick` produces today; no daemon, no PTY).
- **Agent:** none.
- **Asserts:** a form built with Name `"myproj"` (the basename pre-fill) and one orchestration, after a Right-arrow selects that orchestration, has `form.name == "myproj-orchestrator-1"`, not `"myproj"`; submitting from there (Enter Mode→Name, Enter to submit) with no further edit yields `Action::SpawnPane` carrying `req.name == "myproj-orchestrator-1"` unchanged.
- **Does not assert:** the daemon round-trip `live_orchestration_cwds_and_titles()`/`transition_after_dir_pick` performs to learn live names (not unit-testable without a live daemon); rendering of the suggestion (no L1 render seam asserts the Name field's literal text here).
- **Platform coverage:** mac+linux+windows.

##### orchestration/identity/003 — With `<folder>-orchestrator-1` already live (injected via a test-only `NewPaneFormState::with_live_orchestration_names` builder), selecting the orchestration suggests `<folder>-orchestrator-2` next, skipping the taken slot; submitting a name a live orchestration already holds is REFUSED — no `Action::SpawnPane`, form stays open.
- **Layer:** L1 (`src/ui.rs`'s own `#[cfg(test)] mod tests`, as `orchestration/identity/002`).
- **Agent:** none.
- **Asserts:** a form built with Name `"myproj"`, one orchestration, and `with_live_orchestration_names(vec!["myproj-orchestrator-1".into()])`, after a Right-arrow selects the orchestration, has `form.name == "myproj-orchestrator-2"`; overwriting the Name field back to the taken `"myproj-orchestrator-1"` and submitting via `handle_new_pane_form_key` does NOT yield `Action::SpawnPane`, and `ui.mode` stays `UiMode::NewPaneForm`.
- **Does not assert:** the exact refusal UI copy/rendering; what N is counted over across multiple cwds (scoped global-over-live); a real-binary/PTY-attached end-to-end pass (no L2 test accompanies this port).
- **Platform coverage:** mac+linux+windows.

##### orchestration/identity/004 — Re-clicking the already-selected orchestration chip, or arrowing off it and back, must not clobber a Name the user has typed over the suggestion (Greptile P1; the suggestion must only ever replace a generated default, never a human edit).
- **Layer:** L1 (`src/ui.rs`'s own `#[cfg(test)] mod tests` — the real `handle_new_pane_form_key` path plus a real `dispatch_action(Action::FormSelectMode(...))` dispatch against a `CapturingPaneController`/`TabManager`; no daemon, no PTY).
- **Agent:** none.
- **Asserts:** a form with one plain mode and one orchestration, after selecting the orchestration and typing `"my-custom-name"` over the suggestion one keystroke at a time through the real key handler, dispatching `Action::FormSelectMode` at the SAME (already-selected) index leaves `form.name == "my-custom-name"` unchanged; a subsequent arrow-away-to-a-mode-and-back sequence (a genuine selection change, which a `idx != selection_index` guard would not catch) also leaves the name unchanged.
- **Does not assert:** the click-to-`Action::FormSelectMode` routing itself (covered elsewhere); rendering of the Name field's literal text.
- **Platform coverage:** mac+linux+windows.

##### orchestration/identity/005 — An empty Name field is checked against the RESOLVED title it will actually submit (the canonical config name), not the raw empty string — clearing the field can no longer silently bypass the collision guard.
- **Layer:** L1 (`src/ui.rs`'s own `#[cfg(test)] mod tests` — the real `handle_new_pane_form_key` path for the state transition, plus the `render_new_pane_orchestration_name_collision_to_buffer` seam and a direct `render_overlay_to_buffer`/`render_new_pane_form` render of the driven form for the render assertions; no daemon, no PTY).
- **Agent:** none.
- **Asserts:** a form with one orchestration and `with_live_orchestration_names(vec!["review".into()])`, after selecting the orchestration and clearing the Name field to empty via real `KeyCode::Backspace` events, `form.name_collision()` is `true` and submitting via `handle_new_pane_form_key` does NOT yield `Action::SpawnPane` (`ui.mode` stays `UiMode::NewPaneForm`); separately, rendering the dedicated collision seam with an empty typed name against a live title matching the seam's fixture orchestration name confirms `[Submit]` is absent from the action row (dropped, not dimmed); and rendering the form those keystrokes actually produced — focus on Name, the one field where Enter reaches the guard, which the seam does not cover because it opens focused on Mode — confirms the footer omits `Enter: submit` entirely (issue #589), while typing `review-2` over the empty name restores both `[Submit]` and the `Enter: submit` promise.
- **Does not assert:** the daemon-side authoritative check deferred to a follow-up issue (form-time uniqueness stays advisory); a real-binary/PTY-attached end-to-end pass; the footer wording on a field where Enter only advances focus, or on the mode-locked schedule form (both covered by the `new_pane_form_footer_hint` unit test beside it).
- **Platform coverage:** mac+linux+windows.

##### orchestration/identity/006 — Cycling AWAY from the orchestration restores the directory-basename pre-fill, so a plain pane, a workload-mode pane or a `schedule`/`dispatcher` card is never left holding the `<folder>-orchestrator-N` suggestion (issue #638).
- **Layer:** L1 (`src/ui.rs`'s own `#[cfg(test)] mod tests` — the real `handle_new_pane_form_key` arrow-key path against a `NewPaneFormState` built with the bare-basename pre-fill `transition_after_dir_pick` produces; no daemon, no PTY).
- **Agent:** none.
- **Asserts:** a form built with Name `"myproj"` and one orchestration, after Right selects the orchestration (control: `form.name == "myproj-orchestrator-1"`, so the failure below is attributable to the leave path and not to the whole suggestion), Left back to "No mode" restores `form.name == "myproj"` and submitting yields `Action::SpawnPane` with `req.name == "myproj"`; separately, two Rights landing on the built-in `schedule` option — the cycler orders orchestrations BEFORE it, so reaching it means passing over one — also leaves `form.name == "myproj"`.
- **Does not assert:** that a name the user TYPED survives the same cycling (that is `orchestration/identity/004`, whose `name_touched` guard this shares and does not change); the render of the Name field's literal text; the daemon round-trip that learns live orchestration names.
- **Platform coverage:** mac+linux+windows.

#### orchestration/guard

##### orchestration/guard/001 — Opening an orchestration in a cwd that already hosts a live orchestration shows a non-blocking shared-resource warning pointing at worktrees (PRD #140).
- **Layer:** L1 (in-process `TestBackend` via `render_new_pane_orchestration_guard_to_buffer`; no PTY, no subprocess).
- **Agent:** none (the render seam supplies synthetic live-daemon orchestration cwd records).
- **Asserts:** an orchestration selected for a cwd matching an existing live orchestration renders a warning containing `.dot-agent-deck` and `worktree` while retaining `[Submit]`; the same form for a fresh cwd renders neither warning substring.
- **Does not assert:** exact warning copy or styling; daemon `list_agents` transport; worktree creation; blocking spawn behavior (the warning is informational).
- **Platform coverage:** mac+linux+windows.

#### orchestration/lock

##### orchestration/lock/001 — `scope_command_entry_lock` claims `Ctrl+E` only on an Orchestration tab in command mode.
- **Layer:** L1 (pure function, `src/ui.rs`'s own `#[cfg(test)]` module — the scoping helper is module-private).
- **Agent:** none.
- **Asserts:** table-driven over the full cross product of `is_orchestration_tab` (true/false) × every `UiMode` variant × the action being `ToggleOrchestrationLock`, some other action (`Quit`), or `None`: the toggle survives ONLY at `(true, UiMode::Normal)`; every other action passes through untouched in EVERY cell (including `(false, non-Normal)`, ruling out a blanket "drop the action" implementation); `None` in always yields `None` out. The `UiMode` list is guarded by an exhaustive match so a new variant cannot silently drop out of the cross product.
- **Does not assert:** anything about a real pane — this is a mechanism test, present so a later failure localises. The real-pane proof is `orchestration/lock/009`.
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/002 — A freshly opened orchestration tab observes the deck-global command-entry lock LOCKED.
- **Layer:** L1 (real `Action::SpawnPane` dispatched through `dispatch_action` against a capturing pane controller).
- **Agent:** none (two-role `cat` stub orchestration config).
- **Asserts:** after a real spawn, the active tab is a `Tab::Orchestration` and `ui.command_entry_locked` is `true`. Locked-by-default is load-bearing: a lock you must remember to engage protects nothing.
- **Does not assert:** the gate's own behaviour (`orchestration/lock/006`/`008`); persistence across restarts (the lock is not persisted).
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/003 — `Ctrl+e` resolves to the toggle from command mode and flips the deck-global lock both ways.
- **Layer:** L1 (`key_action_for_mode`, the production `KeyEvent -> Action` seam, plus two real `dispatch_action` calls).
- **Agent:** none.
- **Asserts:** with the DEFAULT keybinding config, `Ctrl+e` in `UiMode::Normal` resolves to `Action::ToggleOrchestrationLock`; dispatching it once unlocks and twice re-locks `ui.command_entry_locked`.
- **Does not assert:** the full `is_orchestration_tab × mode` matrix (that is `orchestration/lock/001`); a user-remapped chord.
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/004 — Toggling the lock on ANY Orchestration tab changes what EVERY Orchestration tab observes, and a new tab adopts the current value.
- **Layer:** L1 (two real orchestration tabs spawned through `dispatch_action`, plus real `switch_to` round-trips).
- **Agent:** none.
- **Asserts:** tab A starts locked and toggling on A unlocks; a brand-new tab B ADOPTS the unlocked value rather than resetting to locked; switching back to A observes the same unlocked value; toggling FROM B and returning to A shows A observing B's change. Pins that unlocking never has to be repeated per tab.
- **Does not assert:** that the lock reaches beyond Orchestration tabs — deck-global storage moves where the value lives, not how far it reaches (`orchestration/lock/005`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/005 — Dashboard and Mode tabs are never gated, even while the deck-global lock is engaged.
- **Layer:** L1 (`gate_pane_input_key` called directly against a real Dashboard tab and a real spawned Mode tab).
- **Agent:** none.
- **Asserts:** with `ui.command_entry_locked = true` (the strongest case) and an EMPTY status map (so the `WaitingForInput` carve-out cannot fire and the pass-through can only come from the tab-kind match), `Action::ForwardToPane` passes through UNCHANGED on both tab types. Guards the obvious mis-reading of deck-global storage as deck-global reach.
- **Does not assert:** the Orchestration-tab gate itself (`orchestration/lock/006`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/006 — A locked non-orchestrator pane reporting `WaitingForInput` passes keystrokes through, and the gate re-engages the moment the status clears.
- **Layer:** L1 (`gate_pane_input_key` against a real two-role orchestration and a focus-echoing pane controller).
- **Agent:** none (synthetic `pane_id -> SessionStatus` maps).
- **Asserts:** walking both edges on the SAME worker pane — no recorded status (dropped, the baseline) → `WaitingForInput` (passes through unchanged) → `Working` (dropped again, so the hole cannot outlive the status that opened it). Also that the orchestrator pane's own input is never gated whatever status is attached to it (proving the never-gated rule is not reordered behind the new check), and that an unlocked deck ignores `WaitingForInput` entirely.
- **Does not assert:** that any particular agent actually emits `WaitingForInput` — that is the agent's contract, not this feature's. An agent that never reports it gets no carve-out and still needs a deliberate unlock.
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/007 — An ambiguous pane status (two sessions sharing one `pane_id`) DENIES the carve-out — fail closed, not fail open.
- **Layer:** L1 (`build_pane_status_for_gate` feeding the unchanged `gate_pane_input_key`, against a real locked orchestration with its worker focused).
- **Agent:** none (two synthetic `AppState`s standing in for the daemon-observed collision).
- **Asserts:** two sessions colliding on one `pane_id` and DISAGREEING on `WaitingForInput`-ness resolve to no exemption and the keystroke is dropped; a single, unambiguous `WaitingForInput` session still resolves to `WaitingForInput` and still passes the keystroke through — so failing closed cannot be bought by breaking the carve-out outright. The guard has to live in the producer: a `HashMap<&str, SessionStatus>` cannot represent the collision, so by the time the gate reads the map the ambiguity is already gone.
- **Does not assert:** the collision semantics of `build_pane_status` itself, which is deliberately left as-is — its consumers are cosmetic, and only the lock's feed hardens. The rule here is "any duplicate", not "any disagreeing duplicate".
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/013 — A `WaitingForInput` written by a producer that named no generation does NOT open the lock (issue #398, PR #443 review).
- **Layer:** L1 (in-process gate resolution against a real orchestration tab).
- **Agent:** none (a synthetic single `WaitingForInput` session plus the pane's untagged-status mark).
- **Asserts:** with exactly ONE unambiguous `WaitingForInput` session on the focused locked worker pane, `build_pane_status_for_gate` still omits the pane while its status provenance is untagged, and `gate_pane_input_key` drops the keystroke; clearing the mark — what an identified hook does — restores the carve-out and the keystroke passes through unchanged.
- **Does not assert:** the duplicate-session denial, which is a separate rule (`orchestration/lock/007`); that untagged status is hidden from cards or borders (it deliberately still renders).
- **Platform coverage:** mac+linux+windows.

##### orchestration/lock/008 — On a real locked orchestration tab the orchestrator's own input still reaches its PTY while a worker's does not, and `Ctrl+d`,`Ctrl+e` reverses that.
- **Layer:** L2 PTY-attached (the real binary through the vt100 `TuiDeck` harness).
- **Agent:** none (fixture `tests/fixtures/orch-deck`: two `cat` stub roles, no LLM tokens spent).
- **Asserts:** a sentinel typed into the focused orchestrator pane echoes on the grid even though the deck is LOCKED by default; after jumping to the non-orchestrator `worker` role, a second sentinel does NOT appear within 2s; after `Ctrl+d` → `Ctrl+e` → `Ctrl+d`, a third sentinel typed into the still-focused worker pane echoes normally.
- **Does not assert:** what a real agent does with the forwarded bytes (`orchestration/lock/012`); the `WaitingForInput` carve-out (`orchestration/lock/011`).
- **Platform coverage:** mac+linux.

##### orchestration/lock/009 — `Ctrl+e` reaches a focused role pane's PTY in `PaneInput`, is claimed by the deck in command mode, and toggles the lock there.
- **Layer:** L2 PTY-attached (the real binary through the vt100 `TuiDeck` harness; rendered-grid observation).
- **Agent:** none (fixture `tests/fixtures/orch-deck`: two `cat` stub roles, no LLM tokens spent).
- **Asserts:** with a partial line typed into the focused orchestrator pane, `Ctrl+e` makes a literal `^E` appear immediately after it — the tty line discipline's own caret echo (`ECHOCTL`), proving `0x05` genuinely reached the PTY rather than being claimed as `Action::ToggleOrchestrationLock`. Then `Ctrl+d` into command mode and `Ctrl+e` again: the deck reports `Pane entry: unlocked`, NO second `^E` joins the first (claimed there means not forwarded — the mirror of the first half), and jumping to the worker role with `2` lets a sentinel reach its PTY, proving the chord still toggles the lock from the mode it IS claimed in.
- **Does not assert:** what a given program does with `0x05` once it arrives — that is the program's business. The oracle is deliberately the terminal's caret echo, not readline: an earlier revision drove a real `bash --noprofile --norc -i` role and asserted readline's `beginning-of-line`/`end-of-line` cursor moves, which fails outright wherever bash is built without readline (this repo's own devbox bash offers no `emacs` option, so `Ctrl+a` echoed `^A` and moved the cursor two columns the wrong way).
- **Platform coverage:** mac+linux.

##### orchestration/lock/010 — Global chords still fire while a worker pane is focused and the deck is locked.
- **Layer:** L2 PTY-attached.
- **Agent:** none (fixture `tests/fixtures/orch-deck`).
- **Asserts:** with the non-orchestrator worker role focused and the deck LOCKED, `Ctrl+t` (`toggle_layout`) surfaces its `Layout:` status message — global chords resolve before the PTY-forward fallback the lock gates. Regression guard against an overly-broad gate.
- **Does not assert:** the layout change itself (covered by the layout tests).
- **Platform coverage:** mac+linux.

##### orchestration/lock/011 — On a real locked pane, a reported `WaitingForInput` opens the gate and the status clearing closes it again.
- **Layer:** L2 PTY-attached; the status is injected as a bare `AgentEvent` over the hook socket — the SAME wire the real `dot-agent-deck agent-event` CLI rides.
- **Agent:** none, deliberately. A real agent would self-skip wherever credentials are absent, leaving this headline behaviour with ZERO automated CI coverage; the status arrives over the genuine production wire either way, and what a stand-in gives up is only proof that some particular agent emits that status.
- **Asserts:** the baseline drop with no status recorded; then, after injecting `WaitingForInput` for the worker's real `(pane_id_env, agent_id)` pair, a keystroke reaches the worker's PTY and echoes; then, after injecting `Thinking`, a re-focused worker drops keystrokes again. The injector blocks on `ListAgents`' live-status join rather than the daemon's broadcast, so the daemon's own state — not just its wire — is known to reflect the change before focus/echo is asserted.
- **Does not assert:** that any real agent emits `WaitingForInput`; the auto-focus steering that the same status also drives (`orchestration/focus/*`) — the worker is re-focused explicitly so this cannot ride that as a proxy.
- **Platform coverage:** mac+linux (unix-only: the injector writes to a Unix-domain hook socket).

##### orchestration/lock/012 — A REAL Claude agent never receives a directive typed at a locked worker pane, and does receive it once unlocked.
- **Layer:** L2 PTY-attached, real-agent tier. Runtime-skipped when the `claude` CLI or credentials are absent.
- **Agent:** REAL Claude Code (Haiku, `claude-haiku-4-5-20251001`, `--allowedTools Bash`) as the non-orchestrator `worker` role; the orchestrator stays `cat` (already proven never-gated) to keep the run to a single real agent turn. Fixture `tests/fixtures/orch-lock-live`.
- **Asserts:** a create-a-sentinel-file directive typed into the locked worker pane never results in that file existing (20s); after `Ctrl+d` → `Ctrl+e` → `Ctrl+d`, a second directive with a DIFFERENT sentinel does result in its file being created (120s); and the first sentinel STILL does not exist afterwards, proving gated keystrokes are dropped outright rather than queued for delivery once unlocked. On-disk file presence is the observable, so the assertion survives LLM phrasing and terminal-redraw variance.
- **Does not assert:** anything when skipped — where credentials are absent this test executes nothing, so `orchestration/lock/008`/`011` carry the CI-visible coverage.
- **Platform coverage:** mac+linux (real-agent tier is local-only).

##### orchestration/lock/014 — With the `experimental` flag OFF (the default), the command-entry lock surface is absent entirely.
- **Layer:** L2 (PTY end-to-end).
- **Agent:** none (`orch-deck` fixture, two stub `cat` roles). Deliberately launched WITHOUT `DOT_AGENT_DECK_EXPERIMENTAL`, unlike every other test in the file.
- **Asserts:** on a real orchestration tab a keystroke typed at the focused non-orchestrator worker reaches its PTY with no unlock chord at all; the `Pane locked` message never appears; and `Ctrl+e` sent in command mode is not claimed, so no `Pane entry:` report is produced. This is the other side of the PRD #393 gate — a regression that shipped the lock unconditionally fails here rather than reaching every user silently.
- **Does not assert:** the locked behaviour itself (`orchestration/lock/008`); that the focus steering is gated too (no automatic focus movement is asserted here).
- **Platform coverage:** mac+linux.

#### orchestration/focus

##### orchestration/focus/001 — Auto-focus follows the lowest-order `WaitingForInput` role pane on the active tab, and never touches another tab.
- **Layer:** L1 (`TabManager::auto_focus_waiting_pane` driven with synthetic `SessionStatus` maps; `src/tab.rs`).
- **Agent:** none (three-role orchestration: `orchestrator` < `alpha` < `beta`).
- **Asserts:** nothing waiting leaves manual focus alone; a newly-waiting pane steals focus; ties resolve to the LOWEST-order waiting pane, even stealing focus mid-input from a higher-order pane that is itself still waiting; an already-lowest focused pane is a no-op (no flicker); resolving the focused pane advances to the next-lowest still-waiting pane. A second orchestration tab then proves a background tab's newly-waiting pane has zero effect and never flips which tab is active.
- **Does not assert:** the all-clear return move (`orchestration/focus/002`); ordering by wait time — ascending `role_pane_ids` order is the contract, and "longest blocked first" would need a new per-pane timestamp.
- **Platform coverage:** mac+linux+windows.

##### orchestration/focus/002 — The all-clear move back to the orchestrator is edge-triggered, fires exactly once per waiting episode, and re-arms for the next.
- **Layer:** L1 (the real per-frame sequence — `observe_waiting_panes`, then `auto_focus_waiting_pane` → `auto_focus_all_clear` — gated exactly as the `src/ui.rs` render-loop site gates it).
- **Agent:** none.
- **Asserts:** a manual focus is left alone while nothing is waiting; a newly-waiting pane steals focus; once it resolves, focus snaps back to the orchestrator role exactly ONCE — not on every subsequent frame, and not again for a later manual focus change until a NEW pane starts and resolves waiting. A level-triggered version would pin focus to the orchestrator every frame and the human could never look at another pane at all. A second (background) orchestration tab proves the move never touches an inactive tab or switches which tab is active.
- **Does not assert:** the single-frame episode (`orchestration/focus/003`); the render-loop application of the returned id (`orchestration/focus/007`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/focus/003 — A waiting episode observed in a SINGLE frame still edge-triggers the all-clear move.
- **Layer:** L1 (the real per-frame sequence).
- **Agent:** none.
- **Asserts:** a role goes `WaitingForInput` on one frame and resolves by the next, with no intervening frame in which it is both still waiting and already focused. The first frame steers focus onto it — so `auto_focus_waiting_pane` WINS the chain and `auto_focus_all_clear` never runs on the only frame the episode is observed — and the second frame must still fire the all-clear. This is why the observation lives OUTSIDE the chain: recording the edge inside `auto_focus_all_clear` loses it entirely and strands focus on the resolved pane.
- **Does not assert:** the multi-frame episode (`orchestration/focus/002`), which always has a still-waiting frame in between and is exactly where a dropped edge hides.
- **Platform coverage:** mac+linux+windows.

##### orchestration/focus/004 — While LOCKED, focus visits every waiting role in ascending order and returns to the orchestrator on the all-clear.
- **Layer:** L1 (the real per-frame sequence; four-role orchestration `orchestrator` < `alpha` < `beta` < `gamma`).
- **Agent:** none.
- **Asserts:** all three non-orchestrator roles go `WaitingForInput` together and focus lands on `alpha` first, advancing to `beta` then `gamma` as each resolves, then returning to the orchestrator once nothing is left waiting, with a further quiet frame moving nothing. Three concurrent waiters are needed: with fewer, "picked one" and "advanced through them in order" are indistinguishable.
- **Does not assert:** the unlocked half (`orchestration/focus/005`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/focus/005 — While UNLOCKED no auto-focus branch fires at all, and re-locking must not replay a stale all-clear edge.
- **Layer:** L1 (the real per-frame sequence with the call site's `locked` gate modelled explicitly, plus `TabManager::clear_waiting_pane_latch`).
- **Agent:** none.
- **Asserts:** a waiting pane already in flight does not steal focus while unlocked, and its later resolution fires no all-clear either, so a manual focus choice survives the whole stretch untouched. Then THE STALE-LATCH ASSERTION: re-locking must NOT fire an all-clear for the episode the human already handled by hand — without the latch clearing, `observe_waiting_panes` compares its frozen `had_waiting_pane == true` against the now-idle status and misreads it as a fresh edge, yanking focus off where the human left it. Finally, re-locking resumes normal steering and all-clear pinning for a fresh episode.
- **Does not assert:** an episode that both begins AND ends inside the unlocked stretch — that case is already safe with no fix (the chain is fully skipped, so nothing touches the latch), which is why this test is written against the STRADDLING trace instead. A test written against the simpler wording passes without the fix and proves nothing.
- **Platform coverage:** mac+linux+windows.

##### orchestration/focus/006 — The locked→unlocked transition clears EVERY Orchestration tab's latch, not just the active one.
- **Layer:** L1 (two orchestration tabs; the real per-frame sequence with the `locked` gate modelled).
- **Agent:** none.
- **Asserts:** tab A latches a waiting episode while active and locked; the user switches to tab B and unlocks, so the deck-global toggle's latch-clearing call fires with B active, not A; A's worker resolves unobserved; on re-lock and return to A, A's first locked frame must treat the resolved role as old news rather than a fresh edge, leaving focus where the user left it. This is `orchestration/focus/005`'s bug reappearing across tabs whenever the clearing is scoped to the active tab instead of the deck-global lock it compensates for.
- **Does not assert:** the mechanism used to reach that outcome — only that every Orchestration tab's edge state is reset on the transition.
- **Platform coverage:** mac+linux+windows.

##### orchestration/focus/007 — The experimental command-entry-lock surface's whole focus contract on the real binary.
- **Layer:** L2 PTY-attached (the real binary through the vt100 `TuiDeck` harness), asserted purely on the rendered grid via the expanded-pane header `┌<role>`, which only the currently focused role ever draws.
- **Agent:** none (fixture `tests/fixtures/orch-focus-lifecycle`: `orchestrator` + `alpha` + `beta`, all `printf`+`sleep` stubs). Three roles are required: the "manual focus sticks" half needs a role OTHER than the one going `WaitingForInput`, since where the focused and waiting role are the same pane a genuine stick is indistinguishable from `auto_focus_waiting_pane`'s own same-pane no-op. `WaitingForInput` is injected over the hook socket exactly as `orchestration/lock/011` does.
- **Asserts:** (1) with the experimental command-entry-lock surface enabled, a freshly opened tab starts LOCKED and shows the orchestrator's expanded box; (2) injecting `WaitingForInput` for `alpha` visibly steers focus onto ITS box; (3) injecting `Thinking` visibly returns focus to the orchestrator — the all-clear edge; (4) `Ctrl+d`,`Ctrl+e` surfaces `Pane entry: unlocked`; (5) manually jumping to `beta` and then injecting a fresh `WaitingForInput`/`Thinking` pair for `alpha` moves focus NOWHERE — `beta`'s box survives both events, and a sentinel typed at the end appears inside `beta`'s own box, proving it still holds live PTY focus rather than merely still being drawn.
- **Does not assert:** the `TabManager`-level contract in isolation (`orchestration/focus/001`-`006`); the keystroke gate (`orchestration/lock/*`).
- **Platform coverage:** mac+linux (unix-only: the injector writes to a Unix-domain hook socket).

##### orchestration/focus/008 — The waiting-focus branch defers a focus steal, rather than applying it immediately, while a keystroke is still queued for the currently-focused waiting pane.
- **Layer:** L1 (in-process unit test; `src/tab.rs`, alongside `orchestration/focus/001`-`006`).
- **Agent:** none (mock `PaneController`; synthetic `SessionStatus` map, no panes/PTYs).
- **Asserts:** with a real `TabManager`-opened 3-role Orchestration tab (`orchestrator` < `alpha` < `beta`), `beta` (higher role order) goes `WaitingForInput` and steals focus with no input pending, as `orchestration/focus/001` pins; `alpha` (LOWER role order than `beta`) then ALSO goes `WaitingForInput` on a frame where `input_pending` is true (modeling a keystroke still queued for `beta`) — the steal to `alpha` must be deferred, returning `None` and leaving focus on `beta`, not yanked away from the pane the queued keystroke is aimed at; once `input_pending` clears on a later frame, the deferred steer to `alpha` must still fire, proving the guard DEFERS the move rather than dropping it, mirroring `TabManager::auto_focus_all_clear`'s existing "no one-shot latch" contract. Drives `TabManager::auto_focus_locked(pane_status, input_pending)`, the seam that folds both `auto_focus_waiting_pane` and `auto_focus_all_clear` behind ONE shared `input_pending` guard mirroring the real per-frame call site's shape.
- **Does not assert:** the real `src/ui.rs` per-frame call site actually computing `input_pending` from `crossterm::event::poll` or applying the result via `pane.focus_pane` (out of L1 `TabManager` reach — it would need a PTY-attached L2 test, and an L2 test was evaluated and rejected: the underlying terminal race is not economically reproducible there, since it requires a keystroke to be sitting in the terminal's input queue on the exact frame a lower-order pane transitions to `WaitingForInput`); the deck-global lock gate itself (`ui.command_entry_locked`, covered by `orchestration/focus/005`/`006`); the multi-waiter ordering contract, covered exhaustively by `orchestration/focus/001`/`004`.
- **Platform coverage:** mac+linux+windows.

##### orchestration/focus/009 — Turning the `experimental` flag OFF mid-session clears the waiting-episode latch, so re-enabling it replays no stale all-clear edge.
- **Layer:** L1 (in-process unit test; `src/tab.rs`, alongside `orchestration/focus/001`-`006`/`008`).
- **Agent:** none (mock `PaneController`; synthetic `SessionStatus` map, no panes/PTYs).
- **Asserts:** with the flag on and the deck locked, `alpha` goes `WaitingForInput`, latching the episode and stealing focus; the flag then flips OFF (the watcher re-reads `.dot-agent-deck.toml` roughly every 2s, so this is reachable without a restart) and `alpha` resolves unobserved; on the first frame after the flag returns, no focus move may be produced. A latch left standing while the flag was off would read there as a stale `true` -> `false` all-clear and yank focus to the orchestrator for an episode already dealt with. Mirrors `orchestration/focus/006`, which pins the same contract for the `Ctrl+E` unlock — the flag is simply a second way to stop observing.
- **Does not assert:** the real `src/ui.rs` per-frame call site (out of L1 `TabManager` reach, as `orchestration/focus/008` records); the gating of the keystroke path or the `Ctrl+E` binding (`orchestration/lock/014`).
- **Platform coverage:** mac+linux+windows.

#### orchestration/remit

##### orchestration/remit/001 — A `Compacting` event on the orchestrator start-role pane re-delivers the remit pointer a second time.
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness; the daemon's own `AppState::apply_event` and `deliver_orchestrator_prompt` render-loop path handle the injected event and the redelivery for real).
- **Agent:** none (`remit-reassert-orchestration` fixture — the `orchestrator` start role runs a synthetic script that declares itself live over the raw hook socket at boot and tees its stdin to `orchestrator-prompt.log`; `worker` is a plain `cat` stub; no LLM tokens spent).
- **Asserts:** after the spawn-time remit pointer (`Read .dot-agent-deck/orchestrator-context.md`) delivers once (confirmed via the log), injecting a synthetic `Compacting` `AgentEvent` for the SAME start-role pane/agent identity — confirmed applied via the daemon's own `ListAgents` live-status join before proceeding — causes the log to show the pointer a second time within 10s.
- **Does not assert:** that the trigger is scoped to compaction alone versus any other event type (that is the pure-data unit test `remit_reassert_fires_only_for_compacting_status`, `src/ui.rs`); the guard against firing on a non-start-role pane (`002`); the readiness-gating/delivery-confirmation discipline of the re-assertion itself (`003`).
- **Platform coverage:** mac+linux (`#[cfg(unix)]` — the fixture script's `emit_target` helper is a POSIX shell function calling `python3`).

##### orchestration/remit/002 — A `Compacting` event on a non-start `worker` role's pane re-asserts nothing, while the same event on the orchestrator start role in the same orchestration still re-asserts.
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness).
- **Agent:** none (`remit-reassert-orchestration` fixture, as `001`).
- **Asserts:** after the spawn-time remit pointer delivers once, injecting `Compacting` for the non-start `worker` role's pane/agent identity does not push the start role's delivery log to a second `Read .dot-agent-deck/orchestrator-context.md` line within a 900ms bounded wait; injecting `Compacting` immediately afterward for the orchestrator START role's own identity, in the SAME orchestration, DOES push the log to a second line within 10s — the positive control that makes the negative check meaningful rather than a vacuous pass against an unimplemented feature.
- **Does not assert:** a genuinely non-orchestration (plain agent/mode) pane's compaction re-asserting nothing — deliberately not exercised here since the worker-role case already proves the guard does not key off "any pane in the orchestration" and the settled scope names only the start role as a trigger; the readiness-gating/delivery-confirmation discipline of the re-assertion itself (`003`).
- **Platform coverage:** mac+linux (`#[cfg(unix)]`, matching `001`).

##### orchestration/remit/003 — A re-assertion triggered while the start-role pane is history-only does not write blindly: the pointer stays undelivered (with the same `History-only session cannot accept live input` feedback the spawn-time seed already surfaces for a non-applied `SendResult`) until the pane later reports itself live again, at which point the deferred re-assertion completes.
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness).
- **Agent:** none (`remit-reassert-orchestration` fixture; the orchestrator role's script additionally toggles its own declared liveness live -> history-only -> live on cue from control files the test writes into the fixture workdir).
- **Asserts:** with the start-role pane confirmed history-only, injecting `Compacting` for its identity does not push the delivery log to a second line within a 900ms bounded wait, and the rendered grid surfaces `History-only session cannot accept live input` within 5s; once the SAME pane subsequently reports itself live again, the log reaches a second `Read .dot-agent-deck/orchestrator-context.md` line within 10s — proving the re-assertion is gated on confirmed delivery rather than a direct, unconfirmed pane write. Deliberately asserts only on the rendered grid and the delivery-log line count — both pre-existing, stable observables — never on an internal helper or `SendResult` variant, so this test's correctness does not depend on internal refactoring of the delivery-confirmation machinery.
- **Does not assert:** the pure liveness-toggle mechanism in isolation (covered generally by `prompt/pane-input/007`'s identical `emit_target` technique at spawn time); a genuinely dropped/lost re-assertion attempt distinct from a merely-deferred one (not constructible without comparing against a broken implementation).
- **Platform coverage:** mac+linux (`#[cfg(unix)]`, matching `001`/`002`).

##### orchestration/remit/004 — A `/clear`-originated `SessionStart` event on the orchestrator start-role pane re-delivers the remit pointer a second time, exactly once.
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness; the daemon's own `AppState::apply_event` and `deliver_orchestrator_prompt` render-loop path handle the injected event and the redelivery for real).
- **Agent:** none (`remit-reassert-orchestration` fixture, as `001`).
- **Asserts:** after the spawn-time remit pointer (`Read .dot-agent-deck/orchestrator-context.md`) delivers once (confirmed via the log), injecting a synthetic `SessionStart` `AgentEvent` stamped `AgentType::ClaudeCode` and carrying the `dot_agent_deck::event::CLEAR_SESSION_START_METADATA_KEY`/`_VALUE` marker for the SAME start-role pane/agent identity — confirmed applied via the daemon's own `ListAgents` live-status join reporting `Idle` before proceeding — causes the log to show the pointer a second time within 10s; a bounded 900ms wait afterward confirms the count then STAYS at 2 rather than climbing to 3 (pins non-repetition, not just arrival).
- **Does not assert:** that `ClaudeCodeHookInput`/`build_event_typed` actually parse and forward a real hook JSON payload's `source` field into this metadata key (that is the pure-data unit test `clear_session_start_source_forwards_narrowly`, `src/hook.rs`); the guard against firing on a non-start-role pane (`005`); the guard against firing for a non-Claude-Code `agent_type` (`006`); the compaction trigger this mechanism is reused from (`001`).
- **Platform coverage:** mac+linux (`#[cfg(unix)]` — the fixture script's `emit_target` helper is a POSIX shell function calling `python3`).

##### orchestration/remit/005 — A `/clear`-originated `SessionStart` event on a non-start `worker` role's pane re-asserts nothing, while the same event on the orchestrator start role in the same orchestration still re-asserts.
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness).
- **Agent:** none (`remit-reassert-orchestration` fixture, as `001`).
- **Asserts:** after the spawn-time remit pointer delivers once, injecting the `/clear`-originated `SessionStart` marker (stamped `AgentType::ClaudeCode`) for the non-start `worker` role's pane/agent identity does not push the start role's delivery log to a second `Read .dot-agent-deck/orchestrator-context.md` line within a 900ms bounded wait; injecting the same marker immediately afterward for the orchestrator START role's own identity, in the SAME orchestration, DOES push the log to a second line within 10s — the positive control that makes the negative check meaningful rather than a vacuous pass against an unimplemented feature.
- **Does not assert:** a genuinely non-orchestration (plain agent/mode) pane's `/clear` re-asserting nothing — deliberately not exercised here, matching `002`'s own scope decision; the readiness-gating/delivery-confirmation discipline of the re-assertion itself (covered generally by `003` for the compaction trigger, reused unchanged here); the guard against firing for a non-Claude-Code `agent_type` (`006`, a different axis — pane identity here, agent type there).
- **Platform coverage:** mac+linux (`#[cfg(unix)]`, matching `001`/`004`).

##### orchestration/remit/006 — A `/clear`-originated `SessionStart` event on the orchestrator start-role pane, stamped with a non-Claude-Code `agent_type`, re-asserts nothing.
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness).
- **Agent:** none (`remit-reassert-orchestration` fixture, as `001`).
- **Asserts:** after the spawn-time remit pointer delivers once, injecting the `/clear`-originated `SessionStart` marker stamped `AgentType::Codex` for the orchestrator START role's own pane/agent identity does not push the delivery log to a second line within a 900ms bounded wait. Deliberately negative-only, unlike `002`/`005`'s same-guard-different-axis pattern: chasing this check with a same-pane positive-control injection (a second `SessionStart`, ClaudeCode-tagged, on the SAME pane) would ride `pane_hook_session` (`src/state.rs`) — the bookkeeping `delivery_target_changed` (`src/ui.rs`) compares against — into reading the pane as stale after two hops — an artifact of sequencing two `SessionStart`s onto one pane, a shape no real pane (whose `agent_type` is fixed for life) ever produces. The positive-control role is instead filled independently by `004` (a genuine single-hop ClaudeCode-tagged injection on this same pane shape), the same relationship `001`'s positive proof bears to `002`'s negative check on a different pane, applied here across the agent-type axis instead of the pane-identity axis.
- **Does not assert:** the pane-identity scope guard, covered by `005`; the readiness-gating/delivery-confirmation discipline of the re-assertion itself, covered by `003`.
- **Platform coverage:** mac+linux (`#[cfg(unix)]`, matching `001`/`004`/`005`).

##### orchestration/remit/007 — A compaction re-assertion on a start role whose context file already carries a `## Your task` section re-delivers the TASK-CARRYING pointer variant and leaves the task itself intact on disk, rather than silently replacing both with the no-task "wait for instructions" form.
- **Layer:** L2 (real-binary PTY via the vt100 `TuiDeck` harness).
- **Agent:** none (`remit-reassert-orchestration` fixture, as `001`).
- **Asserts:** after the spawn-time remit pointer delivers once, a `## Your task` section carrying a sentinel is seeded onto `.dot-agent-deck/orchestrator-context.md` — reproducing byte-for-byte the shape `prepare_orchestrator_prompt(config, cwd, Some(task))` leaves on disk for a `dispatch --task` orchestration (`src/spawn.rs`) — without launching a second, separately-dispatched fixture. Injecting `Compacting` for the start role's own identity then causes the delivery log to show the task-carrying pointer (containing "Then carry out that task") within 10s, and the context file on disk still contains the sentinel afterward. Regression coverage for the maintainer review on the fork's upstream PR #789 ("Required 1"): before the fix, the re-arm called `prepare_orchestrator_prompt(config, cwd, None)` directly, which wiped the `## Your task` section and delivered the no-task pointer instead, silently discarding a dispatched orchestration's task on every compaction.
- **Does not assert:** the daemon dispatch path (`src/spawn.rs`) itself producing that seeded shape at spawn (covered by unit tests in `src/orchestrator_context.rs`: `reassert_preserves_an_existing_dispatched_task`, `reassert_with_no_prior_task_reproduces_no_task_behavior`, `reassert_with_no_existing_file_falls_back_to_no_task`); the equivalent guard on the `/clear` re-arm site, which shares the same `reassert_orchestrator_prompt` helper and is therefore covered by the same unit tests rather than a second, near-identical L2 case.
- **Platform coverage:** mac+linux (`#[cfg(unix)]`, matching `001`/`004`/`005`/`006`).

#### orchestration/layout

##### orchestration/layout/001 — Seven decks fit the single-column orchestration card area without scrolling (PRD #147).
- **Layer:** L1 (ratatui `TestBackend`, buffer inspection + capacity math via the public `rendered_height` seam).
- **Agent:** none.
- **Asserts:** in the ~34%-width single-column orchestration card area at a typical ~48-row card height, the renderer's `visible_rows = available / card_height` fits all 7 decks with no scrolling and the 7th deck actually renders in the visible slice; a much larger deck count (20) still engages scrolling, so right-sizing the card height does not remove the scroll fallback.
- **Does not assert:** the full orchestration-tab frame (tab bar, side panes, stats bar); the `ORCHESTRATION_LEFT_PERCENT` width split or `grid_columns` thresholds (out of scope per PRD #147).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/002 — In a 7-role orchestration tab with `PaneLayout::Stacked`, the focused pane's rect covers the full pane-column height and no collapsed title-bar frames are drawn for the other 6 roles (PRD #311).
- **Layer:** L1 (in-process `compute_frame_layout` + `render_frame` driven through a real `ratatui::Terminal<TestBackend>`, via `EmbeddedPaneController::for_render_only_tests()`; no PTY, no subprocess). Lives in `src/ui.rs`'s own `#[cfg(test)]` module (same pattern as `tabs/orchestration/003-005`) because the geometry helpers under test (`pane_stack_rects`, `stacked_expanded_index`, `render_terminal_panes`) are module-private and unreachable from `tests/*.rs`.
- **Agent:** none (7 synthetic role pane ids, no backing PTYs).
- **Asserts:** with no pane explicitly focused (so `stacked_expanded_index` falls back to the first role, `orchestrator`), the expanded role's OUTER rect height equals the full pane-column height with no rows ceded to collapsed frames; none of the other 6 roles' pane ids appear anywhere in the rendered grid (i.e. no `Borders::TOP` collapsed title block is drawn for a non-focused pane).
- **Does not assert:** PTY resizing of the reclaimed area (`resize_panes_to_layout`); mode-tab side-pane geometry (covered by `tabs/mode/001`); the sidebar deck-card capacity math (covered by `orchestration/layout/001`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/003 — `Ctrl+l` resolves to `Action::ToggleOrchestrationSplit`, and an orchestration tab's frame geometry is the default 34/66 split untoggled and the narrower-sidebar 25/75 split toggled (PRD #336).
- **Layer:** L1 (pure-data `compute_frame_layout` geometry + `key_action_for_mode`, the public L1 seam over the same mode-aware resolver chain the event loop runs; no PTY, no TestBackend render). Lives in `src/ui.rs`'s own `#[cfg(test)]` module because `compute_frame_layout` and `ActiveTabView` are module-private. Note the resolver is deliberately tab-agnostic — the orchestration-tab scoping is a separate step, covered by `orchestration/layout/005`.
- **Agent:** none.
- **Asserts:** resolving a simulated `Ctrl+l` `KeyEvent` through `key_action_for_mode` with the default keybinding config yields `Action::ToggleOrchestrationSplit` specifically (not merely "some action"); an untoggled orchestration tab's `dashboard_area`/`panes_area` widths are 34/66; a toggled one's are 25/75. The split travels on `ActiveTabView::Orchestration::split_narrow`, so the test sets it directly on the render snapshot with no shared state to seed or reset.
- **Does not assert:** the visible rendered grid (covered by the PTY-attached `tabs/orchestration/007`); the real dispatch and its global scope across tabs (covered by `orchestration/layout/004`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/004 — The orchestration split is GLOBAL: toggling one orchestration tab changes every other one too, including a tab opened later, which adopts the current global split instead of the 34/66 default (PRD #336).
- **Layer:** L1 (`dispatch_action` dispatched directly against a `CapturingPaneController`; no PTY, no TestBackend render).
- **Agent:** none (two `cat`-role orchestration tabs opened through `TabManager::open_orchestration_tab`).
- **Asserts:** dispatching `Action::ToggleOrchestrationSplit` on the Dashboard tab is a no-op (it cannot mutate unrelated tab state); opening tab A alone starts at the default (`split_narrow == false`) and toggling it flips the flag to `true`; opening tab B AFTER that toggle shows B starting already narrow, not reset to the default; toggling from B (now active) flips BOTH tabs back to `false`; switching back to A and toggling again flips both to `true`. This is PRD #336's revised "toggling one tab changes all of them, and a new tab adopts the live global value" criterion, asserted on the real field.
- **Does not assert:** the resulting rendered geometry (covered by `orchestration/layout/003`); spawn-time PTY dims (covered by `orchestration/layout/006`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/005 — `scope_orchestration_split` claims the split toggle only on an orchestration tab in command mode, un-resolving it everywhere else so `Ctrl+l` reaches the pane's PTY, and passes every other action through untouched (PRD #336).
- **Layer:** L1 (pure function, no PTY, no render).
- **Agent:** none.
- **Asserts:** `Some(ToggleOrchestrationSplit)` survives only for (orchestration tab, `UiMode::Normal`); it becomes `None` off an orchestration tab in any mode, and on an orchestration tab in `PaneInput`/`Filter`/`Help`/`NewPaneForm` (so the key falls through to the `PaneInput` forwarding path). The mode half mirrors `close_pane` (PRD #241 M1), which is command-mode only so `Ctrl+w` still reaches the PTY as word-delete. `ToggleLayout`, `DetachToNormal` and `None` pass through unchanged for every tab/mode pair, proving the guard is surgical rather than a general-purpose filter.
- **Does not assert:** that the event loop actually calls it (covered end-to-end by `tabs/orchestration/008`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/006 — A role pane spawned while the GLOBAL orchestration split is already narrow gets its PTY sized at the 75%-width column, not the 66%-width default (PRD #336, PR #342).
- **Layer:** L1 (`dispatch_action` dispatched directly against a `CapturingPaneController` extended to record each spawn's `(rows, cols)`; no PTY, no TestBackend render).
- **Agent:** none (two 2-role `cat` orchestration tabs opened through the real `Action::SpawnPane` dispatch path).
- **Asserts:** open tab A through `Action::SpawnPane` at the default split; toggle the GLOBAL split narrow via `Action::ToggleOrchestrationSplit` dispatched from tab A (the only way a real user reaches the narrow state, since the toggle only resolves on an active orchestration tab); then open tab B, also through `Action::SpawnPane`, while the global is already narrow. Every role pane spawned for tab B must be recorded with `cols == 73` (the 75%-width inner column on a 100-wide frame), not `64` (the 66%-width default) — the `dispatch_action` new-tab-open branch must read `tab_manager.orchestration_split_narrow()` rather than a hardcoded `false` when computing `spawn_dims`. This is the spawn-time-dims gap `orchestration/layout/004` explicitly left uncovered. Verified with teeth: reverting the seed to a hardcoded `false` at the call site reproduces the pre-fix bug and fails this test with `left: 64, right: 73`.
- **Does not assert:** the restore/hydrate call site in `run_tui`'s `apply_snapshot` branch, which reads the same `tab_manager.orchestration_split_narrow()` at its own spawn-dims call — not reachable at L1 (embedded in `run_tui`, which needs a real `Terminal` and a disk-loaded `SavedSession`, with no factored-out testable seam) and, per the current call graph, not distinguishable at ANY layer today: that branch runs exactly once, before the event loop starts, when `TabManager`'s global is still at its just-constructed default (`false`) — so `tab_manager.orchestration_split_narrow()` and the old hardcoded `false` currently always agree there, and no test could show them diverging without also changing when restore can run.
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/007 — `scope_zoom` claims the plain `z` zoom toggle only on an orchestration tab in command mode, un-resolving it everywhere else so the letter reaches the agent, and `Ctrl+Z` never becomes a second zoom binding (PRD #313).
- **Layer:** L1 (pure function plus `key_action_for_mode`, the public L1 seam over the same mode-aware resolver chain the event loop runs; no PTY, no render). Lives in `src/ui.rs`'s own `#[cfg(test)]` module because `scope_zoom` is module-private.
- **Agent:** none.
- **Asserts:** a simulated plain-`z` `KeyEvent` resolves through `key_action_for_mode` to `Action::ToggleZoom` specifically, not merely to "some action"; `Some(ToggleZoom)` survives `scope_zoom` only for (orchestration tab, `UiMode::Normal`) and becomes `None` off an orchestration tab in any mode and on an orchestration tab in `PaneInput`/`Filter`/`Rename`/`Help`/`NewPaneForm`, so the keystroke falls through to the PTY-forwarding path. The mode half matters more here than it does for `orchestration/layout/005`'s `Ctrl+l`: `z` is an ORDINARY CHARACTER, so without it nobody could type the letter z at an agent. Also pins PRD #313 Open Question 1's decision by the other side — `Ctrl+Z` must NOT resolve to `ToggleZoom` in any mode and must still forward `0x1a` to the pane's PTY as job control — and that `ToggleLayout`, `ToggleOrchestrationSplit` and `None` pass through untouched for every tab/mode pair, so the guard is surgical rather than a general-purpose filter.
- **Does not assert:** that the event loop actually calls it (covered end-to-end by `tabs/orchestration/011`, whose PaneInput arm is the live form of the same claim); the geometry the action produces (covered by `orchestration/layout/008`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/008 — A zoomed orchestration tab gives the sidebar zero width and the pane column the whole frame, zoom wins over the narrow split, and unzooming restores exactly the split that was in force (PRD #313 M1).
- **Layer:** L1 (pure-data `compute_frame_layout` geometry; no PTY, no TestBackend render). Lives in `src/ui.rs`'s own `#[cfg(test)]` module because `compute_frame_layout` and `ActiveTabView` are module-private.
- **Agent:** none.
- **Asserts:** on a 100x40 frame an unzoomed orchestration tab resolves to the 34/66 default and a narrowed one to 25/75 (the PRD #336 states); zoomed, from BOTH of those states, it resolves to `(0, 100)` — a zero-width sidebar and a pane column spanning the whole frame. Asserting it from both split states is what makes this fail against a plausible-but-wrong implementation that merely widened the pane column another notch instead of reclaiming the frame. Clearing `zoomed` then returns the frame to exactly the split it came from, not to the 34/66 default — PRD #313's "the same key restores the previous view exactly".
- **Does not assert:** what is DRAWN into those rects (covered by `render/layout/006`); the PTY dims the same layout drives (covered by `orchestration/layout/010`); which tab the state belongs to (covered by `orchestration/layout/009`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/009 — Zoom is PER-TAB, not global — a tab opened later starts unzoomed and toggling one tab never moves another — and it FOLLOWS FOCUS across a `1`-`9` role jump (PRD #313 Open Questions 3 and 4).
- **Layer:** L1 (`dispatch_action` dispatched directly against a `CapturingPaneController`, plus one `compute_frame_layout` pass for the follows-focus half; no PTY, no TestBackend render).
- **Agent:** none (two 2-role `cat` orchestration tabs opened through `TabManager::open_orchestration_tab`).
- **Asserts:** tab A opens unzoomed and one `Action::ToggleZoom` dispatch zooms it; tab B, opened while A is zoomed, comes up UNZOOMED — the deliberate contrast with `orchestration/layout/004`, where a later tab ADOPTS the current global split, because a tab the user never zoomed must not lose its sidebar behind their back; toggling from B moves B and only B, in both directions; a `ToggleZoom` dispatched on the Dashboard is a no-op that mutates no orchestration tab's state. Then the follows-focus half: dispatching the real `Action::FocusCard(1)` (the `1`-`9` role jump) on the zoomed tab A lands on role 2 and re-enters `PaneInput` while leaving the tab ZOOMED, and re-resolving the layout with role 2 focused hands THAT pane the whole frame width from column 0 — so zoom is not pinned to whichever role happened to be focused when it was toggled.
- **Does not assert:** the split percentages themselves (covered by `orchestration/layout/008`); what is drawn (covered by `render/layout/006`); persistence across detach/reattach or session restore (out of scope per PRD #313 Open Question 5 — zoom is ephemeral and nothing about it is written to the saved session).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/010 — A zoomed orchestration tab's role panes get PTY dims for the FULL-width column, so the agent actually reflows (PRD #313).
- **Layer:** L1 (`compute_frame_layout` + `FrameLayout::pane_target_dims`, the exact seam the per-frame `resize_panes_to_layout` sweep reads; no PTY, no TestBackend render).
- **Agent:** none (two synthetic role pane ids).
- **Asserts:** on a 100-wide frame every role pane's target PTY size is 64 inner columns unzoomed (the 66% column less its border) and 73 narrowed (matching `orchestration/layout/006`), while a ZOOMED tab targets 98 (the whole frame less its border) from either split state — including the non-focused `Stacked` role, which `pane_target_dims` sizes "as if focused" so it does not reflow twice on the way back. This is the seam that makes PRD #313's "PTY resize on zoom and unzoom" work: `agent_pty::resize()` is already called by the per-frame sweep and early-returns when neither dimension changed, so an implementation that widened only the drawn rect would leave every agent rendering at 64 columns inside a 98-column pane. Also pins that zoom reclaims WIDTH only — the row budget is identical zoomed and unzoomed — which is PRD #313's scope discipline: zoom gets the toggle and the indicator, and nothing else.
- **Does not assert:** the SPAWN-time dims path that `orchestration/layout/006` covers for the narrow split. That path is deliberately out of reach here rather than skipped: zoom is per-tab and a newly opened tab always starts unzoomed (`orchestration/layout/009`), so a role pane can never be spawned into a zoomed tab and the spawn site has no zoomed case to get wrong. The per-frame sweep asserted here is the only seam through which a zoom reaches a PTY. Nor does it assert which panes are DRAWN: the "as if focused" dims this pins for the non-focused `Stacked` role are target PTY dims, not a claim that the pane appears on screen — it does not, and `orchestration/layout/011` is where "only the focused pane is drawn while zoomed" is pinned (there under Tiled, which is the case PRD #311 does not already cover).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/011 — A zoomed orchestration tab draws ONLY the focused role pane even under `PaneLayout::Tiled`, and leaves the deck's stored Tiled toggle untouched (PRD #313 M1).
- **Layer:** L1 (`dispatch_action` against a `CapturingPaneController` for the real `Ctrl+t` / `z` toggles, plus two `compute_frame_layout` passes for the geometry; no PTY, no TestBackend render). Lives in `src/ui.rs`'s own `#[cfg(test)]` module because `compute_frame_layout`, `ActiveTabView` and `PaneLayout` are module-private.
- **Agent:** none (a THREE-role `cat` orchestration tab opened through `TabManager::open_orchestration_tab` — with only two panes "only the focused one is drawn" and "the other one happens to be empty" are the same observation, and a frame that dropped just one slot would still pass).
- **Asserts:** pins PRD #313 M1's "sidebar and other panes are not drawn" half specifically under **Tiled**, which is the half Stacked gets for free from PRD #311 and Tiled does not. `Action::ToggleLayout` puts the deck in Tiled; `Action::ToggleZoom` then zooms the tab; resolving the layout with that stored `ui.pane_layout` hands the FOCUSED role pane the entire pane column (which `orchestration/layout/008` pins to the entire frame) while both non-focused role panes collapse to ZERO reserved height — the same "not drawn" convention PRD #311 M2 gives Stacked and `orchestration/layout/002` pins there. Re-resolving the same tab UNZOOMED still tiles all three panes across the column, so zoom overrode the frame without restacking the deck underneath. Between and after those, `ui.pane_layout` must still read `Tiled` — the half a plausible-but-wrong implementation fails, since flipping the stored toggle to Stacked on zoom and back on unzoom produces identical geometry and only breaks PRD #313's "the same key restores the previous view exactly" once anything else reads the toggle.
- **Does not assert:** the PTY dims the undrawn Tiled panes end up with. `orchestration/layout/010` covers that seam for Stacked and its answer there is "as if focused", which is NOT in tension with the zero-height rect asserted here — target dims and drawn rects are different questions, and `pane_target_dims` exists precisely to answer the first one differently from the second. Under Tiled+zoomed both resolutions are coherent (size the undrawn pane as if focused, or leave it skipped at its pre-zoom Tiled slice), so pinning one would pin a mechanism rather than the behaviour. Also does not assert the split percentages (`orchestration/layout/008`), what is rendered into the rects (`render/layout/006`, Stacked only), or which tab the zoom belongs to (`orchestration/layout/009`).
- **Platform coverage:** mac+linux+windows.

##### orchestration/layout/012 — The Dashboard zooms on the same terms as an orchestration tab: `(0, 100)` geometry, only the focused pane drawn, and the 33/67 default restored exactly on unzoom (PRD #313).
- **Layer:** L1 (three `compute_frame_layout` passes over an `ActiveTabView::Dashboard`; no PTY, no TestBackend render). Lives in `src/ui.rs`'s own `#[cfg(test)]` module for the same reason its siblings do — `compute_frame_layout`, `ActiveTabView` and `FrameContent` are module-private.
- **Agent:** none (two synthetic pane ids; the layout pass is a pure function of its inputs and spawns nothing).
- **Asserts:** the Dashboard is the SAME SHAPE as an orchestration tab — a card sidebar beside a stack of agent panes, sharing `right_column_pane_dims` — so zoom is worth the same there and resolves the same way. Unzoomed the split is the Dashboard's own **33/67**, not orchestration's 34/66, which is what fails this test if zoom is wired to `orchestration_layout_percents` instead of `dashboard_layout_percents`. Zoomed it is `(0, 100)`: zero-width sidebar, pane column across the whole 100-column frame. Unzooming restores **33/67 exactly** rather than leaving the tab on some third geometry — PRD #313's "the same key restores the previous view exactly", asserted on the Dashboard half. Finally, under `PaneLayout::Tiled` a zoomed Dashboard hands the FOCUSED pane the entire pane column while the non-focused pane collapses to ZERO reserved height, so M1's "other panes are not drawn" half holds here through the same effective-`Stacked` resolution `orchestration/layout/011` pins for the Orchestration arm.
- **Does not assert:** the key that produces the toggle or its scoping (`orchestration/layout/007`, which covers both card-shaped tab kinds through the `tab_has_card_sidebar` predicate); that the Dashboard's zoom flag is independent of an orchestration tab's (they are separate fields on separate `Tab` variants, so no shared value exists to diverge); the `[Z]` marker on a zoomed Dashboard pane (`render/layout/006` pins the marker on the orchestration path, and the indicator is resolved once in `render_frame` for both); Mode tabs, which are deliberately excluded — two pane regions rather than sidebar-plus-panes, so "hide the sidebar" has no meaning there.
- **Platform coverage:** mac+linux+windows.

#### orchestration/dispatch

##### orchestration/dispatch/001 — An agent-callable `dispatch --orchestration <name>` makes a full orchestration TAB surface live on the deck, and that orchestration can actually DELEGATE to its own workers (PRD #220 / #222).
- **Layer:** L2 PTY-attached (`TuiDeck` on the `orch-deck` fixture) driving the REAL `dot-agent-deck dispatch` and `dot-agent-deck delegate` CLIs against the deck's own hook socket, exactly as an agent in a pane does — so the CLI parse, the wire hop, the daemon's shape resolution, the role spawn, the live tab surfacing and the delegate routing are all in the path.
- **Agent:** none — the fixture's two roles run `cat`, which stays alive on stdin. No LLM tokens.
- **Asserts:** the CLI exits 0; the orchestration TAB labelled `demo-orch` appears on the tab strip within 90s WITHOUT a reconnect; the sibling worktree `../<repo>-dispatch-<name>` exists; and `.dot-agent-deck/orchestrator-context.md` inside it carries the delegation protocol plus the caller's task under `## Your task`. Then the DELEGATION round trip, in both directions of the comparison: the same orchestration is ALSO opened the normal way (`Ctrl+N`) as a **control**, and `delegate --to worker` is run from each orchestrator's pane id — both workers must receive the daemon-authored pointer `Read .dot-agent-deck/worker-task-worker.md for your task.` in their own PTY. The control runs FIRST and its failure message says so, because a broken control means the harness is wrong and the dispatched result proves nothing. Finally the LOUD-FAILURE half: `delegate` from a pane the daemon holds no role for, and `delegate --to <role that has no pane>` from a valid orchestrator, must each exit NON-ZERO with stderr naming the pane id / the role — while a HALF-landed `delegate --to worker --to <role that has no pane>` must exit ZERO and name BOTH sides, because the worker really did receive it and a retry aimed at the whole delegation would dispatch it twice.
- **Why it exists:** three PRD #220 defects shipped green because the only dispatch coverage asserted a file on disk or the worktree's existence — never the tab the user actually looks at. A dispatched orchestration that comes up with no tab, or with an orchestrator that was never told it is one, passes every other assertion in this suite (the `reproduce-first` skill / CONTRIBUTING's "Reported bugs start with a failing test"). It then caught a FOURTH, reported by a user: a dispatched orchestration came up perfect and completely inert. `crate::spawn::spawn` reaches `spawn_agent` directly, and only the `AttachRequest::StartAgent` handler was populating the daemon's `pane_role_map` / `orchestrator_pane_ids`, so every `delegate` from a dispatched (or scheduled, or issue-dispatched) orchestrator was dropped with `delegate from unknown pane` — while `delegate` itself, being fire-and-forget, printed nothing and exited 0, so the orchestrator announced phantom progress and waited forever. Both halves are pinned here; reverting either fix alone turns this test red (verified), and reverting the registration fix now fails at the CLI's exit code rather than 90s later at the pointer, because the two fixes compose.
- **Does not assert:** the roles' own output, or an agent DECIDING to delegate — `cat` cannot initiate one, so the test invokes the real CLI with the orchestrator's `DOT_AGENT_DECK_PANE_ID` exactly as that pane's shell would (`orchestration/dispatch/002` owns the real-agent decision path, and the worker actually doing the delegated work). Also not asserted: the `work-done` return edge; cross-orchestration isolation (`orchestration/route/001` owns that).
- **Platform coverage:** mac+linux.

##### orchestration/dispatch/002 — A dispatched orchestration whose roles are REAL agents brings every role in the toml up as a live agent, names each one on its own card, and can delegate work its worker actually DOES (PRD #220).
- **Layer:** L2 PTY-attached (`TuiDeck` on the `dispatch-orch-real` fixture) driving the REAL `dot-agent-deck dispatch <name> --orchestration real-team` CLI against the deck's own hook socket, then reading the daemon's `ListAgents`, the rendered orchestration tab, and the dispatched worktree on disk.
- **Agent:** THREE real, fully interactive Claude Code panes pinned to Haiku (`orchestrator`, `coder`, `reviewer`) — no `-p`, no `cat` stand-in. Cost is three cold boots plus two short turns (the orchestrator decides to delegate; the coder does the work), plus — since issue #584 — a FOURTH cold boot, because the `coder` role is now `clear = true`. That flip is load-bearing: a dispatched orchestration's delegate goes through `respawn_agent_for_pane`, and with `clear = false` this test delegated to an already-running worker and never touched the respawn, so the dispatch path's `clear = true` delegate had no real-agent coverage anywhere in the suite — which is how #584 shipped.
- **Asserts:** the CLI exits 0; a pane exists for every toml role; every role's own PTY shows a REAL agent booted (the Claude Code banner, which no shell or `cat` can print); and on the dispatched orchestration's TAB every role appears on a card as `<AgentType> · <role>`, so the user can tell the orchestrator from a worker. Then the whole point of an orchestration: the real orchestrator is asked (through the daemon's production `WriteAndSubmit`, the same path a user's keystrokes take) to delegate a sentinel-file task to its `coder`, and the `coder` must actually create the uniquely-named sentinel in the dispatched worktree. That last assertion is the user's altitude — "I dispatched an orchestration and the team got something done" — and it is the half only real agents can show: an orchestrator *deciding* to shell `dot-agent-deck delegate`, and a worker receiving the task and acting on it. Since #584 the worker is `clear = true`, so the sentinel can only appear if the delegate's RESPAWN produced a live agent that then received the pointer — the genuine spawn → delegate → respawn → work path, with a real agent, on the dispatch path. Verified load-bearing: with the daemon-side role registration reverted, the coder never does the work and the assertion fails at its full 300s budget. Finally (issue #663) the role cards are asserted AGAIN, after the work landed: a `clear = true` delegate replaces the worker's session, and only a post-delegation check can see the replacement card revert to the new agent's session UUID.
- **Why it exists:** `orchestration/dispatch/001`'s `cat` roles start instantly, need no credentials and have no cold start, so they cannot tell an agent from a `$SHELL` — which is how three PRD #220 defects shipped green. This test found a fourth: a dispatched orchestration labelled every card with claude's session UUID (`ClaudeCode · 6134822e-f2`) while the daemon knew all three role names, because only the interactive `Ctrl+n` path set a per-role display name. Fixed by naming the role on the spawn AND emitting the per-role synthetic `SessionStart` that carries the name to an already-attached TUI.
- **Runs the PRODUCTION delegate readiness buffer** (`DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS=1000`), overriding the harness pin of `0` exactly as `orchestration/delegate/014` and `/015` do. Every other real-agent `clear = true` scenario needs it and this one inherited the pin when the fixture flipped, which is issue #663: `SessionStart` means "a session exists", not "the TUI accepts a submit", so at `0` the task pointer was written into a still-booting Claude and dropped. It also raises `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` to 900 so the daemon outlives the test's own 300s work budget — at the harness default of 300 the daemon self-exited before the assertion fired and the failure dump then reported every role as `NO PANE — never spawned at all` while the same dump rendered all three alive.
- **Does not assert:** `AgentRecord.live` — deliberately. It is `Some(Idle)` for every role within ~1.5s of the dispatch, before a byte reaches any of those PTYs (measured), so it is a pane-level fact and an assertion on it is vacuous. Also not asserted: the `work-done` RETURN edge (the worker's completion signal back to the orchestrator, and the feedback line the daemon writes into the orchestrator pane); delegation to more than one role, or fan-out to `reviewer`, which stays a booted-but-unused role here; cross-orchestration routing isolation (`orchestration/route/001` owns that); and the `delegate` CLI's failure exit codes, which `orchestration/dispatch/001` pins cheaply without spending tokens.
- **Platform coverage:** mac+linux.


##### orchestration/dispatch/003 — A `clear = true` respawn relaunches a worker identically whether the orchestration came up through the daemon's dispatch primitive or through the TUI's `StartAgent` path (issue #584's control).
- **Layer:** fast integration (the REAL `crate::spawn::spawn` dispatch primitive on one side and the `StartAgent` spawn shape on the other, both against one in-process daemon, plus the real `handle_delegate_with_state`; no LLM and no `e2e` feature gate).
- **Agent:** none — a recorder stand-in that appends its argv, cwd, pane id, hook socket and `$SHELL` to a log on every invocation and then behaves like `cat`, so what each replacement was actually LAUNCHED with is on disk rather than inferred.
- **Asserts:** within each path, the respawn's recorded launch equals its initial launch; across the two paths, the two respawns' recorded launches are equal after normalising the legitimately per-pane values (cwd, pane id, hook socket); and BOTH workers physically receive the `worker-task-coder.md` pointer, the control first-in-failure-message so a broken control cannot be read as a dispatch defect. This exists because #584's leading hypothesis was that the two paths preserve DIFFERENT relaunch parameters and the issue asked for that to be reproduced before it was believed. It is not: they agree, which is why #584's fix is about what the delegate does when a replacement is not live, not about how it is launched.
- **Does not assert:** PTY GEOMETRY, which legitimately differs (the daemon-side primitive opens 24×80 and the TUI forwards its viewport) and is replayed faithfully from each pane's own last-known size; the `DOT_AGENT_DECK_AGENT_ID` each child sees, which differs by construction — it is what makes a respawn a new generation; anything about a REAL agent, which is `orchestration/dispatch/002`'s job.
- **Platform coverage:** mac+linux (unix-only — the recorder is a POSIX shell script).

##### orchestration/dispatch/004 — `dispatch --list-targets` marks the orchestration a repo DECLARES as its default, reports an `extends`-inherited role count, and the bare `--orchestration=` dispatch then opens that same one (issues #704 / #705).
- **Layer:** L2 PTY-attached (`TuiDeck` on the `orch-multi` fixture) driving the REAL `dot-agent-deck dispatch --list-targets` and `dot-agent-deck dispatch --orchestration=` CLIs against the deck's own hook socket, exactly as an agent in a pane does — so the CLI parse, the wire hop, the daemon's config load, the `extends` resolution, the default selection and the role spawn are all in the path.
- **Agent:** none — the fixture's roles run `cat`, which stays alive on stdin. No LLM tokens.
- **Asserts:** the listing offers both orchestrations; `[default]` sits on `gpt-side` (which declares `default = true` while being SECOND) and not on `claude-side`; no "comes first in the file" note appears, because the config said what it meant; `gpt-side` is reported as **2 roles** although its block restates one, which only holds if `extends` resolved on the daemon's own config load; and a bare `--orchestration=` dispatch then surfaces an orchestration tab named `gpt-side`.
- **Why it exists:** the listing and the spawn are two independent readings of the same config, and #704 is precisely the case where they disagreed. Asserting them in one test is what makes "the marker is honest" a claim rather than a hope. The `2 roles` assertion is deliberately the inheritance check: a count computed from the block as written is 1, so it cannot pass without the resolution having happened in the daemon rather than in a unit test.
- **Does not assert:** the ambiguity diagnostic for an UNDECLARED default (the `default_orchestration` / `list_targets_response` unit tests own the wording); the scheduler path's half of the same rule (`scheduler/spawn/008`); delegation, role cards or the orchestrator context (`orchestration/dispatch/001`).
- **Platform coverage:** mac+linux.

#### dispatch/close

##### dispatch/close/001 — A dispatched single-agent card closes on the FIRST confirmed Ctrl+W, instead of surviving until the user closes it a second time (PRD #220 follow-up).
- **Layer:** L2 PTY-attached (`TuiDeck` on the `minimal` fixture) driving the REAL `dot-agent-deck dispatch --single` CLI, then closing the resulting card through the production Ctrl+W → confirm path.
- **Agent:** a REAL interactive Claude Code (Haiku) as the dispatched unit, launched through a **wrapper script** (`default_command = "agent-wrapper"`), never prompted — it only has to be running when the close lands, so the cost is one cold boot and no turns. The caller pane is `cat`; it is the caller, not the thing under test. The wrapper is load-bearing, not convenience: it mirrors the reported config, where every command is `devbox run agent-<role>`, which the deck cannot infer an agent type from and therefore does not wrap. A bare `claude` IS recognised and takes a different path through the session machinery — which is exactly why an earlier `cat`-based version of this test passed while the reported bug was live.
- **Stand-in, named:** a PATH `git` stub that sleeps on `status --porcelain` (and ONLY on that — the dispatch's own `git worktree add` runs at full speed). It supplies the one property of a real dispatched worktree a fixture cannot cheaply have: an agent has been working in it, so the status walk takes seconds, not milliseconds.
- **Asserts:** the dispatched agent really starts (its own PTY prints the Claude Code banner — NOT the card's `ClaudeCode` badge, which is inferred from the command at spawn and is on the card before the agent has executed anything); the CALLER card (which owns no worktree) closes on its first confirm — the control, so a later failure is attributable to the dispatched card specifically; then, after ONE confirmed close, NO card for the dispatched worktree remains. Matched on the worktree basename from the card's `Dir:` line rather than on its title, because the ghost card is titled `pane-sched-…` and a name-bound needle misses it.
- **Why it exists:** a user reported closing a dispatched agent leaving its card behind. It reproduced THREE independent defects, and the failure message distinguishes the first two by whether the daemon still holds the agent: (a) a daemon-spawned card has no local pane until focused, so `close_pane` returned `Pane <id> not found`, the PRD #92 F4 policy preserved the card, and the agent kept running; (b) with that fixed, the daemon still awaited the worktree cleanup before answering, blowing the TUI's 5s `CTRL_W_STOP_TIMEOUT`; (c) with BOTH fixed and a real agent behind a non-inferable command, the close removed only the session its card was built from and left the pane's *other* session rendering as a ghost card badged `No agent` — the symptom as reported. Reverting any one fix alone turns this test red (verified).
- **Does not assert:** the worktree's own removal (`KeepIfDirty` leaves a dirty one in place by design); the orchestration close path, where the last role's close is the cleanup trigger.
- **Platform coverage:** mac+linux.

##### dispatch/close/002 — Closing a dispatched card whose worktree holds uncommitted work announces the keep, with its path, both before the keystroke and after it (issue #717).
- **Layer:** L2 PTY-attached (`TuiDeck` on the `minimal` fixture) driving the REAL `dot-agent-deck dispatch --single` CLI, then the production Ctrl+W → confirm path against the resulting card.
- **Agent:** none (`cat` for both the caller pane and the dispatched unit). Deliberate, and the reason is narrower than convenience: the sentence under test is decided by the daemon's worktree registry and one `git status --porcelain`, and no agent participates in either. `dispatch/close/001` next door owns the real-agent close path.
- **Asserts:** the control close of the caller pane — which owns no worktree — renders no warning at all; the dispatched card's confirmation renders `Uncommitted work here is KEPT, not deleted:` together with the worktree's absolute path BEFORE the destructive answer; after confirming, the status line repeats the path; the daemon record for the card is gone; and the uncommitted file is still on disk, so the promise the deck made was true.
- **Why it exists:** the keep decision existed only as a `tracing::warn!` on the `RemovalPolicy::KeepIfDirty` path, so closing a dispatched tab with uncommitted work looked identical to closing a clean one while a directory quietly stayed on disk holding the work. It cannot be reported after the fact on the surfaces that exist: `remove_worktree` runs in a task spawned AFTER `close_agent` and `unregister_pane`, and the client drops the session the moment `close_pane` returns, so `DeliveryNotice`'s pane-ownership guard drops the report by construction. The control assertion is load-bearing — a dialog that warned on every close would satisfy every other assertion here and help nobody.
- **Does not assert:** the `KeepIfDirty` policy itself (keeping the tree is correct behaviour and unchanged); the `Force` path, where a dispatched tree is removed regardless; `worktree list` / `reclaim`.
- **Platform coverage:** mac+linux.

##### dispatch/close/003 — A dispatched worktree that becomes CLEAN while the close confirmation is open is removed and reported as removed, not replayed from the dialog's stale prediction (issue #717).
- **Layer:** L2 PTY-attached (`TuiDeck` on the `minimal` fixture) driving the REAL `dot-agent-deck dispatch --single` CLI, then the production Ctrl+W → confirm path, with the worktree mutated on disk while the modal is up.
- **Agent:** none (`cat`). The mutation stands in for the one thing a live agent does that matters here — committing its work between the dialog and the close — and doing it from the test makes the timing deterministic instead of hoping an agent commits inside a window.
- **Asserts:** a freshly dispatched worktree starts clean (the premise that makes the cleanup meaningful); the dialog warns while the tree is genuinely dirty; after the file is removed and the tree verified clean again, confirming REMOVES the worktree and the deck makes no `KEPT, not deleted` claim.
- **Why it exists:** the close dialog's warning is necessarily a PREDICTION — the agent is still running while the modal is open. Reusing that frozen answer as the post-close report produces a confident lie in both directions: a "kept at `<path>`" message about a directory that was deleted, or silence about one that was newly dirtied and kept. This pins the fix, which is that the post-close report comes from the daemon's own post-cleanup verdict (`remove_worktree`'s return value, broadcast as `BroadcastMsg::WorktreeKept`), measured after `close_agent` reaped the agent so nothing can still be writing to the tree.
- **Does not assert:** the newly-dirtied direction (the dialog stays silent and the daemon still keeps the tree — same mechanism, opposite sign); the warning's wording (`prompt/close-confirm/007`).
- **Platform coverage:** mac+linux.

#### orchestration/route

##### orchestration/route/001 — Two tabs of the SAME orchestration opened in the SAME directory are separate routing groups: each orchestrator's delegate reaches only its own worker and each worker's work-done reaches only its own orchestrator, with no cross-delivery in either direction (PRD #140 M5.1). [reel]
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness — records a `full-stream.cast`, so it is demo-reel-eligible per PRD #180). Both tabs are opened through the PRODUCTION new-pane flow (`Ctrl+N` → picker → Space → Right → Enter → Enter) against the deck's own cwd, so their `(name, cwd)` identities are byte-identical and only the per-tab `orchestration_id` (PRD #140 M1.2, echoed back through `StartAgent` → daemon registry → `ListAgents`) tells them apart. Each delegate is issued by a REAL orchestrator agent that shells `dot-agent-deck delegate` after the production `WriteAndSubmit` RPC types the directive into its pane; each work-done is issued by the REAL worker agent from its task-file footer. Per-pane observation is the daemon's own `AttachRequest::Snapshot`, normalized wrap-insensitively (escape sequences stripped, then everything but `[A-Za-z0-9._/-]` dropped) so a pointer hard-wrapped inside a narrow role card still matches. The freshly-built binary's dir is prepended to the deck → daemon → agents PATH; Claude project-trust for the per-test tempdir cwd is seeded into the deck's HOME after launch (the cwd does not exist before it), so the six panes clear their first-run gates with no keystroke.
- **Fixture:** `tests/fixtures/orchestration-route` — one `[[orchestrations]] name = "route-iso"` with THREE roles (`orchestrator` start + `coder` + `reviewer`), all REAL interactive Haiku `claude` (`--allowedTools Bash Read Write`, no `-p`), workers at `clear = false` so their agent ids and scrollback stay stable across the delegate. Three roles rather than two because `.dot-agent-deck/worker-task-{role}.md` / `work-done-{role}.md` are keyed by ROLE within a cwd (PRD #140 keeps that layer explicitly out of scope), so two same-cwd tabs sharing a role name share those files: driving tab A through `coder` and tab B through `reviewer` makes every no-cross-delivery check a presence/absence question about a pane that would otherwise have received NOTHING, and makes the two work-done feedback strings role-qualified and thus distinguishable inside one orchestrator pane — no occurrence-counting in a redrawing agent TUI.
- **Agent:** REAL Claude Code (Haiku, `claude-haiku-4-5-20251001`) ×6 interactive role panes across the two tabs; four short turns actually run (two orchestrators delegate, two workers create one file each). Flaky-tolerant lane-2 tier (real LLM) — run once, not looped (rule 4/5). Runtime-skipped (Decision 26) when the `claude` CLI/credentials are absent.
- **Asserts:** the second open of the same orchestration in the same directory renders PRD #140 M4.0's non-blocking same-cwd warning pointing at `/worktree-prd` (the M4.0 surface, live in the real form rather than through the L1 render seam); the daemon reports two orchestration tabs with DISTINCT `orchestration_id`s and three role panes each; then, with a task started in EACH tab CONCURRENTLY (the issue's own repro, and the state in which the pre-#140 `HashSet`-ordered work-done lookup was most non-deterministic), tab A's delegate pointer `worker-task-coder.md` lands in tab A's `coder` pane and NEVER in tab B's identically-named `coder` pane; tab A's coder really does its own task (uniquely-named sentinel `route_alpha_5f3c.txt` plus the daemon-written `.dot-agent-deck/work-done-coder.md`); its work-done feedback (`Worker coder has completed their task`) reaches tab A's orchestrator pane and NEVER tab B's; and symmetrically for tab B → `reviewer` (`worker-task-reviewer.md`, `route_beta_9d21.txt`, `work-done-reviewer.md`, `Worker reviewer has completed their task`), with a final sweep re-checking all four absences after both chains have run.
- **Does not assert:** WHICH pane wrote a shared coordination file — `worker-task-{role}.md` / `work-done-{role}.md` are role-and-cwd keyed by design (PRD #140 "Deferred: full same-directory isolation"), so the routing proof is the per-pane delegate/work-done delivery, not the file contents; the hydration round trip of two same-`(name, cwd)` tabs across a detach/reattach (M3.1, covered by the `partition_hydrated_panes` unit tests); the `NameCwd` older-client fallback (M5.2, the cross-version manual test); the exact task text each orchestrator forwards (only the literal sentinel filename has to survive LLM phrasing); the deterministic routing decision itself (mutation-checked unit tests on `delegate_targets` / `orchestrator_for_worker` in `src/state.rs`).
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** four short interactive Haiku turns (two delegates, two one-file tasks) — well under Decision 23's <$0.05/run bound.

##### orchestration/route/002 — Detach/reattach of two same-`(name, cwd)` orchestration tabs rebuilds TWO distinct tabs, each keeping its own routing group, while a token-less (pre-#140) pair still rebuilds as ONE (PRD #140 M3.1).
- **Layer:** L1/synthetic (warm in-process daemon + real attach socket, no PTY-attached binary and no LLM). Drives the production reattach chain end to end: `start_agent` stores `TabMembership` on the daemon's `AgentRecord` → `EmbeddedPaneController::hydrate_from_daemon` reads it back through `ListAgents` + `validate_tab_membership` → `partition_hydrated_panes` buckets by `OrchestrationIdentity` → `resolve_orch_config_for_hydration` / `OrchestrationConfig::synthesize_from_bucket_metadata` → `TabManager::open_orchestration_tab_with_existing_role_panes`. Synthetic is the right tier because the claim is about a hydration round trip, not about agent behaviour; the real-agent two-tab case is `orchestration/route/001`, which never detaches.
- **Agent:** none (six `sh -c 'sleep 30'` stand-ins: `orchestrator` + `coder` for each of tab A, tab B, and a token-less legacy pair, all sharing one orchestration name and one cwd).
- **Asserts:** every pane round-trips its own `orchestration_id` through the daemon echo; the partition yields THREE buckets (tab A, tab B, legacy) rather than one merged bucket, each holding exactly its own two panes; the two tokened buckets' `OrchestrationIdentity`s differ while the token-less bucket falls back to `NameCwd { name, cwd }`; rebuilding every bucket produces three orchestration tabs with each pane owned by exactly one tab; and (PRD #140 review) a dead role slot in each tokened tab mints a DISTINCT synthetic dead-slot id with its own placeholder card — pre-fix the `(cwd, orchestration_name)`-keyed id aliased across the two partitioned tabs onto one shared card — while the legacy identity keeps the pre-review byte format.
- **Does not assert:** live delegate/work-done routing across the reattach (that is `orchestration/route/001` and the `src/state.rs` routing unit tests); PTY attach or scrollback replay of the rebuilt panes; the same-cwd spawn warning (`orchestration/guard/001`); the on-disk snapshot restore branch.
- **Platform coverage:** linux+mac (the suite is `#![cfg(unix)]` — the mock attach servers bind Unix-domain sockets; Windows port tracked by #164).

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

##### session/restore/014 — A restored pane whose command identifies a supported agent immediately shows that agent as Idle.
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (a test-owned executable named `opencode` runs `sleep 600`; no LLM or OpenCode hook event).
- **Asserts:** restoring a saved plain pane whose command basename is `opencode` immediately renders an `Idle` card and never requires a hook event to replace the `No agent` placeholder identity.
- **Does not assert:** OpenCode plugin delivery or later working/waiting transitions; restore fallback paths after a mode-tab failure.
- **Platform coverage:** mac+linux.

##### session/restore/015 — A `session_warnings` entry flushed after `ratatui::restore()` escapes control characters in the daemon-supplied value it interpolates (issue #576).
- **Layer:** L2 (real-binary PTY; `DOT_AGENT_DECK_SESSION` redirected to a test-owned path; daemon freshly spawned and empty).
- **Agent:** none (the staged pane is skipped before anything spawns; no LLM).
- **Asserts:** with a hand-staged `session.toml` whose single saved pane points at a directory that does not exist — the restore loop's "skipping pane … directory … not found" branch, which interpolates the saved pane NAME — and whose name carries an ANSI escape, a CR and an LF each bracketed by a unique sentinel, a clean detach-quit flushes that warning to the real terminal (post-`ratatui::restore()`, so no widget layer filters it) with the sentinels present but NO raw ESC/CR/LF following any of them, and with the whole warning on ONE line. Pins that the exit flush cannot be driven by an attacker-influenced pane name, orchestration `display_name` or `agent_id` to repaint the shell the user is dropped back into or forge an extra line of deck output — the property `ratatui-core`'s `!symbol.contains(char::is_control)` filter already gives the in-session sink.
- **Does not assert:** the exact escape spelling (`\u{1b}` vs `\e` vs stripping — pinned by the `escape_control_chars` unit tests in `src/ui.rs`); that every one of the fifteen push sites is reachable (the fix is at the single flush loop, so one site proves the sanitisation point); non-control Unicode trickery (bidi overrides, homoglyphs), which the in-session sink does not filter either.
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

##### session/live/010 — Rehydration preserves history-only and view-only writability across detach/reconnect (PRD #20, blocker 4).
- **Layer:** L1 state/wire integration (`SessionSnapshot` JSON deserialize + `AppState::seed_hydrated_session`).
- **Agent:** synthetic Codex snapshots.
- **Asserts:** reconnect snapshots carrying `history-only` and `none` live targets rebuild sessions with `Writable::HistoryOnly` and `Writable::None`, rather than reverting to Live.
- **Does not assert:** real socket reconnect rendering; the snapshot-to-state seam is the capability-loss boundary.
- **Platform coverage:** mac+linux+windows.

##### session/live/011 — Reconnect restores the live status produced by the real `agent-event` CLI path.
- **Layer:** fast synthetic real-binary-subprocess integration (the REAL `dot-agent-deck agent-event --type running` CLI + a full in-process daemon's hook and attach sockets + `EmbeddedPaneController::hydrate_from_daemon` and the production card-seeding seam; no PTY-attached TUI, no LLM, no `e2e` feature gate).
- **Agent:** none (synthetic — a `cat`-stub pane spawned through the TUI's real `StartAgent` attach request, with the real lifecycle CLI receiving the exact daemon-injected pane and agent ids).
- **Asserts:** the lifecycle subprocess reaches the daemon as a raw `Thinking` event carrying the expected pane and agent ids; a fresh controller hydrates that managed agent; and seeding the rebuilt card exactly as TUI startup does restores `Thinking` instead of the snapshot-absent `Idle` placeholder.
- **Does not assert:** a rendered vt100 grid or literal detach keystroke; active-tool restoration; real LLM behavior.
- **Platform coverage:** mac+linux.

##### session/live/012 — A real TUI reconnect preserves `Thinking` reported by the real non-`SessionStart` `agent-event` CLI for an ordinary `StartAgent` pane.
- **Layer:** L2 PTY-attached (a real headless daemon, two successive real TUI processes driven through the vt100 `TuiDeck` harness, the production `StartAgent` attach request, and the REAL `dot-agent-deck agent-event --type running` CLI; no LLM).
- **Agent:** none (synthetic — a long-lived shell pane explicitly typed as Pi so its ordinary dashboard card is stable while the real lifecycle CLI reports under the daemon-injected pane and registry agent ids).
- **Asserts:** the ordinary pane registers through the production `StartAgent` path; the first attached TUI visibly renders `Thinking` after the real CLI's raw non-`SessionStart` report; after that TUI disconnects, a fresh PTY-attached TUI hydrates the same daemon and the rebuilt card header still renders `Thinking`, never the snapshot-absent `Idle` fallback.
- **Does not assert:** active-tool restoration (`session/live/006` covers a `Working` snapshot with a tool); real LLM behavior; scheduler/dispatch spawning (`scheduler/spawn/007`).
- **Platform coverage:** mac+linux.

##### session/live/013 — `SessionSnapshot.last_activity_ms` reports the session's own recorded instant and is additive in both directions (PRD #745 M9).
- **Layer:** pure-data (serde round-trip plus one `SessionState::live_snapshot` call; no daemon/TUI harness; runs in the fast tier).
- **Agent:** none.
- **Asserts:** a `SessionState` whose `last_activity` is an hour old snapshots as that hour-old instant in epoch milliseconds, not as a timestamp minted at snapshot time (the honesty property that separated `last_activity` from the rejected session duration, whose `started_at` IS invented on hydration); the integer round-trips exactly; an absent activity time has no key in the JSON at all rather than a null; an older peer's snapshot payload lacking the key decodes via `#[serde(default)]` to `None` with every other field intact; and a newer peer's payload carrying the key decodes without disturbing the fields an older reader understands — which is the proof behind the do-not-bump decision (`PROTOCOL_VERSION` stays 8).
- **Does not assert:** the desktop DTO projection or the webview's relative-time wording and clock-skew rule (both covered by the desktop crate's `dto.rs` tests and `AgentOverview.test.tsx`); the `ListAgents` join that carries the snapshot (`session/live/002`, `session/live/003`); the TUI-side seeding of the field, which `seed_hydrated_session` deliberately does not overlay.
- **Platform coverage:** mac+linux+windows.

##### session/live/014 — `AgentRecord.spawned_at_ms` reports when the daemon forked the child, is absent when it did not, and is additive in both directions (PRD #745 M11).
- **Layer:** mixed pure-data / real-PTY (one real `AgentPtyRegistry::spawn_agent` plus serde round-trips; no daemon socket, no TUI harness; runs in the fast tier).
- **Agent:** none (the spawned pane is the default shell; no LLM).
- **Asserts:** an agent spawned through the registry reports a spawn instant that lies inside the spawn call itself, bracketed by `Utc::now()` either side, so a value minted at snapshot time or copied from a session would fail; the integer round-trips exactly; an agent the registry did NOT fork reports no spawn time and has no key in the JSON at all rather than a null; an older peer's `AgentRecord` payload lacking the key decodes via `#[serde(default)]` to `None` with every other field intact; and a newer peer's payload carrying the key decodes without disturbing the fields an older reader understands — which is the proof behind the do-not-bump decision (`PROTOCOL_VERSION` stays 8).
- **Does not assert:** that a respawn mints a fresh instant (structural — `respawn_agent_for_pane_declared` removes the record and `spawn_agent` is the only writer, and the registry's respawn behaviour is covered by `orchestration/delegate/*`); the desktop DTO projection or the webview's uptime wording and clock-skew rule (the desktop crate's `dto.rs` tests and `AgentOverview.test.tsx`); the `ListAgents` handler (`session/live/002`, `session/live/003`).
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
- **Asserts:** with two dashboard panes present and any prior snapshot removed, requesting a pane close with Ctrl+W and choosing Close with Down+Enter writes a fresh `session.toml` that still reflects the (non-empty) workspace — proving the detach path flushes the snapshot mid-session, not only at clean quit.
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

### Remote diagnostics (PRD #345)

#### remote/doctor

##### remote/doctor/001 — A healthy registered remote reports every diagnostic check and exits 0.
- **Layer:** L2 (thin real-binary subprocess spawn with a deterministic `ssh` script prepended to `PATH`; no PTY and no real remote).
- **Agent:** none.
- **Asserts:** `dot-agent-deck remote doctor prod` resolves a staged registry entry, exits exactly 0 for healthy canned observations, and prints exactly one line for each stable check identity (`HostReachable` through `ForwardAgent`), with a `PASS` / `WARN` / `FAIL` / `UNKNOWN` verdict token on that line.
- **Does not assert:** that the `ForwardBound` PASS came from parsing the canned SOCKS5 response (covered explicitly by `remote/doctor/007`), exact headline/fix prose, spacing, colour, a real ssh server, or a real forwarded socket.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/002 — An unknown remote is rejected before any observation session runs.
- **Layer:** L2 (thin real-binary subprocess spawn with test-owned registry and ssh argv recorder).
- **Agent:** none.
- **Asserts:** asking for an unregistered name exits non-zero, names that remote in the error, and leaves the ssh argv recorder absent/empty — registry resolution precedes all probes.
- **Does not assert:** exact error prose or suggestion wording.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/003 — Remote forwarding policy refusal and a port collision produce distinguishable reports.
- **Layer:** L2 (two real-binary subprocess spawns; the PATH-stub independently controls `sshd -T` and the listener response).
- **Agent:** none.
- **Asserts:** both diagnoses exit exactly 1 and render the full check list; `AllowTcpForwarding no` plus a refused listener fails the `AllowTcpForwarding` check, while forwarding-allowed plus a connected non-SOCKS squatter passes `AllowTcpForwarding` and fails `ForwardBound`; both runs issue a live probe against `/dev/tcp/127.0.0.1/1080`; the complete reports differ, and so do their two `ForwardBound` lines specifically, with the collision line naming its port and saying the listener is not this tunnel's.
- **Does not assert:** exact fix/headline sentences, the probe's shell byte-plumbing beyond the `/dev/tcp` endpoint, `blocked`'s own `ForwardBound` verdict (FAIL and UNKNOWN are both honest for a refused connect under a refusing policy), or a live sshd/socket.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/004 — An unavailable remote `sshd -T` is UNKNOWN, exits 2, and does not stop the remaining checks.
- **Layer:** L2 (real-binary subprocess spawn; the PATH-stub returns a permission-denied `sshd -T` result while all other probes succeed).
- **Agent:** none.
- **Asserts:** `AllowTcpForwarding` and `ClientAliveInterval` each report `UNKNOWN`, never `PASS`, the output carries an sshd availability/permission hint, all other stable check identities still render, and the incomplete diagnosis exits exactly 2 rather than the broken-state code 1.
- **Does not assert:** the exact hint text, whether a real host requires root, or the ordering of stderr versus stdout.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/005 — A complete doctor run issues no deck-authored mutation and leaves staged local bytes unchanged.
- **Layer:** L2 (real-binary subprocess spawn with staged `remotes.toml`, `session.toml`, OpenSSH client files, and an argv-recording PATH stub).
- **Agent:** none.
- **Asserts:** a healthy full run leaves the registry, saved session, ssh config, and known-hosts bytes unchanged; ssh is actually invoked, and no recorded argv contains a config/file/service mutation command.
- **Does not assert:** side effects performed by OpenSSH itself, because the test replaces it with a recording script — in particular `known_hosts` writes, `ControlMaster` sockets, and `LocalCommand` / `KnownHostsCommand` execution. The observation-session flags address those risks, but this seam cannot verify their effect; it also does not assert filesystem metadata such as atime, packets emitted by real ssh, or the container-based privileged sshd validation harness.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/006 — A foreign service on the reverse-dynamic port can never produce PASS or exit 0.
- **Layer:** L2 (two real-binary subprocess spawns; the PATH-stub returns healthy policy and configuration plus a connected listener that is not a SOCKS server — one that answers with foreign bytes, one that answers with nothing).
- **Agent:** none.
- **Asserts:** the probe targets the configured remote endpoint; with a listener that answers non-SOCKS bytes, `ForwardBound` is `FAIL`, the aggregate is `FAIL`, and the process exits exactly 1 even though every other observation is healthy; with a listener that accepts the connection and never replies, `ForwardBound` is not `PASS` and the exit code is not 0.
- **Does not assert:** the foreign protocol, exact collision prose, a real socket, or whether the silent listener is `FAIL` or `UNKNOWN` — the observation cannot separate a squatter that ignores the handshake from one whose reply never arrived, so only the absence of a confident PASS is pinned.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/007 — A SOCKS5-verified reverse-dynamic tunnel reports PASS and exits 0.
- **Layer:** L2 (real-binary subprocess spawn; the PATH-stub answers the listener probe with hex bytes `05 00`).
- **Agent:** none.
- **Asserts:** `ForwardBound` is `PASS`, its line says the SOCKS listener was verified or handshaken rather than merely reachable, the aggregate is `PASS`, and the process exits exactly 0.
- **Does not assert:** the exact shell command used to write/read the handshake, exact prose beyond attribution, or a real SOCKS server.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/008 — Nothing listening on a configured reverse-dynamic port never reports all-clear.
- **Layer:** L2 (real-binary subprocess spawn; the PATH-stub returns a connection-refused observation from the live probe).
- **Agent:** none.
- **Asserts:** the probe targets the configured endpoint, `ForwardBound` is not `PASS`, and the process does not exit successfully when no live listener was observed.
- **Does not assert:** whether the honest non-PASS is `FAIL` or `UNKNOWN`, because the observation alone cannot distinguish a disconnected user from a failed bind.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/009 — Unavailable listener-probe tooling is UNKNOWN and exits 2.
- **Layer:** L2 (real-binary subprocess spawn; the PATH-stub returns status 127 for the `/dev/tcp` probe).
- **Agent:** none.
- **Asserts:** `ForwardBound` and the aggregate are `UNKNOWN`, never `PASS`, and the process exits exactly 2 when the remote cannot run the probe.
- **Does not assert:** whether the real missing capability is `bash`, `/dev/tcp`, or the selected hex-rendering tool.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/010 — An accepting listener for a concrete reverse forward remains unattributable and UNKNOWN.
- **Layer:** L2 (real-binary subprocess spawn; `ssh -G` resolves `RemoteForward 1080 db.internal.test:5432` and the PATH-stub accepts the live TCP probe).
- **Agent:** none.
- **Asserts:** `ForwardBound` is `UNKNOWN`, its headline explains that the accepting listener could not be attributed or verified, the aggregate is `UNKNOWN`, and the process exits exactly 2.
- **Does not assert:** the concrete forward's application protocol or any active protocol-specific verification beyond SOCKS5 reverse-dynamic forwards.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/011 — A WARN-only diagnosis exits 0.
- **Layer:** L2 (real-binary subprocess spawn; all observations are healthy except resolved `ForwardAgent yes`).
- **Agent:** none.
- **Asserts:** `ForwardAgent` and the aggregate are `WARN`, and the process exits exactly 0, pinning WARN as advisory rather than incomplete or broken.
- **Does not assert:** exact agent-forwarding advisory prose or any real ssh-agent interaction.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

##### remote/doctor/012 — A remote reporting a different attach protocol version is not a fault (issue #491).
- **Layer:** L2 (real-binary subprocess spawn; the synthetic `ssh` answers `daemon hello` with a `server_version` one below and one above this binary's own).
- **Agent:** none.
- **Asserts:** `ProtocolCompatible` is `PASS` and the run exits 0 in **both** skew directions, and no report quotes a laptop-side protocol version at the user. Pins the removal of the laptop↔remote comparison: `connect` ssh's in and runs the *remote* binary's TUI against the *remote* daemon, so those two constants never share a wire and a difference between them was never evidence of a fault.
- **Does not assert:** anything about a remote that cannot answer `daemon hello` at all — that floor is kept and is unit-covered by `connect::tests::unanswerable_handshake_stays_fatal_without_naming_versions`.
- **Platform coverage:** mac+linux (Unix-only `sh` + `PATH` executable seam).

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

#### codex/wrap

##### codex/wrap/001 — A synthetic Codex JSONL stream runs through the real wrapper, daemon event stream, and PTY-attached dashboard (PRD #20 M7).
- **Layer:** L2 PTY-attached (`TuiDeck`, real binary + daemon, deterministic shell stand-in, no authentication or LLM).
- **Agent:** synthetic stand-in wrapped with `dot-agent-deck wrap --agent codex`.
- **Asserts:** realistic turn-start and turn-completed lines become typed Codex `AgentEvent`s carrying `AGENT_EVENT_SCHEMA_VERSION`; because the wrapper is inside a daemon-managed pane, events declare `Pty/Live`; `WriteAndSubmit` returns Applied and the child records the submitted line; the rendered card visibly shows the Codex identity and transitions Thinking → Idle.
- **Does not assert:** model authentication or Codex CLI behavior (covered by `codex/live/001`).
- **Platform coverage:** mac+linux.

##### codex/wrap/002 — Wrapped children retain TTY identity, resize delivery, input, and Ctrl+C behavior (PRD #20, blocker 1 / finding 16).
- **Layer:** L2 PTY-attached (`TuiDeck` + deterministic shell probe under the real wrapper; no LLM).
- **Agent:** synthetic terminal probe wrapped as Codex.
- **Asserts:** the child observes `isatty(0/1/2) == true`, receives SIGWINCH after resize, records ordinary input, and handles Ctrl+C as SIGINT.
- **Does not assert:** Codex output parsing or model behavior (`codex/live/001`).
- **Platform coverage:** mac+linux.

##### codex/wrap/003 — Wrapped commands preserve each standard descriptor's independent TTY or redirection semantics (PRD #20, final review finding 11).
- **Layer:** L1/fast real-binary subprocess integration with controlled pseudo-terminals and files; no TUI or LLM.
- **Agent:** deterministic shell probes wrapped as Codex.
- **Asserts:** wholly non-interactive stdout/stderr remain separate and binary stdin remains byte-exact through EOF; redirecting only stderr sends child stderr to that file rather than merged PTY stdout; redirecting only stdout leaves child stdin and stderr attached to TTYs.
- **Does not assert:** interactive resize, ordinary input, or Ctrl+C behavior (`codex/wrap/002`).
- **Platform coverage:** mac+linux.

##### codex/wrap/004 — Catchable termination signals tear down and reap wrapped children on PTY and pipe paths (PRD #20, final review finding 12).
- **Layer:** L1/fast real-binary subprocess integration with a controlled pseudo-terminal for the interactive path and null descriptors for the pipe path; no TUI or LLM.
- **Agent:** deterministic lingering shell child wrapped as Codex.
- **Asserts:** after SIGTERM and SIGHUP are delivered to the wrapper, both interactive PTY and non-interactive pipe wrappers exit and their recorded child process is no longer running.
- **Does not assert:** the pre-spawn signal race or termios restoration during a signal arriving inside setup; those timing edges are not deterministic at this subprocess seam.
- **Platform coverage:** mac+linux.

##### codex/wrap/005 — Concurrent standalone wrappers emit unique session IDs (PRD #20 Greptile finding #4).
- **Layer:** L1/fast real-binary subprocess integration with a synthetic hook socket; no TUI or LLM.
- **Agent:** two overlapping deterministic shell probes wrapped with Codex identity and no pane environment ID.
- **Asserts:** the two wrapper lifecycles produce two distinct session IDs instead of reconciling onto one synthetic `wrap-<program>` ID.
- **Does not assert:** managed-pane session IDs, which intentionally remain pane-derived and are covered by `codex/wrap/001`.
- **Platform coverage:** mac+linux.

##### codex/wrap/006 — A wrapped child sitting at its ready interface is announced with a readiness signal of its own, distinct from the fork-time card-surfacing `SessionStart` (issue #243).
- **Layer:** L1/fast real-binary subprocess integration over an interactive pseudo-terminal with a real hook socket collecting every emitted event; no TUI, daemon or LLM.
- **Agent:** deterministic shell stand-in wrapped as Codex that paints a ready prompt, records that its interface exists, and then idles at it forever — so nothing it does can be confused with the wrapper's exit-time `Idle`/`Error`.
- **Asserts:** the precondition that the stand-in genuinely reached its ready interface, then that within three seconds the wrapper emits a `SessionStart` NOT carrying `WRAPPER_FORK_SESSION_START_ORIGIN` — the exact shape `state::session_start_means_ready` already accepts as readiness, so the assertion is agnostic about which origin value the new signal carries, and stayed green when the implementation later split that signal into two origins (`wrapper_interface_ready` for the child clearing `ICANON`/`ECHO`, `wrapper_interface_settled` for output going quiet). GREEN since the wrapper-side signal landed, at 0.78 s; before it, the wrapper's whole stream for a ready, idling child was the fork-time `SessionStart` plus a `Thinking` classified off the banner, so a delegate gate had nothing to release on.
- **Does not assert:** what the delegate gate then does with the signal (`orchestration/delegate/029`); the fork-time event's own card-surfacing role (`orchestration/delegate/007`); real codex-cli boot output (`codex/live/001`).
- **Platform coverage:** mac+linux (unix-only — `openpty` and a POSIX shell stand-in).

#### codex/trust

##### codex/trust/001 — No Codex launch form receives an invocation-global hook-trust bypass (PRD #20 Greptile P1 close-by-deletion).
- **Layer:** L1/fast real-binary subprocess integration with controlled Codex homes and executable stand-ins.
- **Agent:** deterministic bare `codex`, absolute `/path/codex`, launcher script, and `devbox` stand-ins.
- **Asserts:** every launch form inherits the pinned `CODEX_HOME`, and none receives `--dangerously-bypass-hook-trust`; the hazardous global mechanism is absent rather than launcher-identity-gated.
- **Does not assert:** scoped trust records (covered by `codex/trust/002`–`003`) or real Codex hook execution.
- **Platform coverage:** mac+linux.

##### codex/trust/002 — Scoped trust selects only pinned-home, unmanaged, deck-owned hook entries (PRD #20 §4.3.1/§4.3.6).
- **Layer:** L1/fast real-binary subprocess integration with a deterministic `codex app-server` JSON-RPC stand-in, exercised through both bare Codex and a launcher script.
- **Agent:** synthetic Codex hooks/list response containing one eligible deck hook plus a foreign command, a deck command from a different home, a managed entry, and a user command that merely mentions `dot-agent-deck`.
- **Asserts:** `[hooks.state]` contains only the eligible entry whose `sourcePath` is the pinned home's `hooks.json`, command ends in `hook --agent codex`, and `isManaged` is false; the global bypass never appears for either launch method.
- **Does not assert:** byte-preserving config edits or untrust behavior (covered by `codex/trust/003`).
- **Platform coverage:** mac+linux.

##### codex/trust/003 — Scoped trust config edits preserve user bytes, remain idempotent, and untrust only deck keys (PRD #20 §4.3.2).
- **Layer:** L1/fast real-binary subprocess integration with an isolated Codex home and deterministic app-server stand-in.
- **Agent:** synthetic deck hook identity plus a pre-existing user `config.toml` containing a comment, model selection, and foreign trust record.
- **Asserts:** trust appends exactly one hash-pinned deck table while preserving the existing config bytes verbatim; a second write creates no duplicate; Codex hook uninstall removes the deck key and retains the foreign key.
- **Does not assert:** Codex's runtime trust-status interpretation (covered by the real-agent green-confirm scenario).
- **Platform coverage:** mac+linux.

#### codex/spawn

##### codex/spawn/001 — Plain restored Codex panes launch through the Wrapper strategy (PRD #20, blocker 3).
- **Layer:** L2 synthetic PTY-attached restore with PATH recorder stubs.
- **Agent:** synthetic Codex recorder.
- **Asserts:** restoring persisted bare `codex` executes exactly `dot-agent-deck wrap --agent codex -- codex` and never the bare recorder.
- **Does not assert:** mode or orchestration paths (002–003).
- **Platform coverage:** mac+linux.

##### codex/spawn/002 — Mode-pane Codex commands launch through the Wrapper strategy (PRD #20, blocker 3).
- **Layer:** L2 synthetic PTY-attached new-pane mode flow with PATH recorder stubs.
- **Agent:** synthetic Codex recorder.
- **Asserts:** selecting a workload mode while Command is bare `codex` injects the wrapped command into the mode pane, never bare Codex.
- **Does not assert:** restore or orchestration paths.
- **Platform coverage:** mac+linux.

##### codex/spawn/003 — Orchestration role Codex commands launch through the Wrapper strategy (PRD #20, blocker 3).
- **Layer:** L2 synthetic PTY-attached new-pane orchestration flow with PATH recorder stubs.
- **Agent:** synthetic Codex recorder as the start role.
- **Asserts:** selecting an orchestration whose start-role command is bare `codex` launches the role through the wrapper exactly once.
- **Does not assert:** scheduler role spawning (`scheduler/spawn/006`) or respawn.
- **Platform coverage:** mac+linux.

##### codex/spawn/004 — Restored mode-pane Codex commands launch through the Wrapper strategy (PRD #20, blocker 3).
- **Layer:** L2 synthetic PTY-attached saved-session mode restore with PATH recorder stubs.
- **Agent:** synthetic Codex recorder.
- **Asserts:** a saved pane carrying `mode = "wrapped-mode"` and bare command `codex` rebuilds the mode tab and injects the wrapper command, never bare Codex.
- **Does not assert:** fresh mode creation (`codex/spawn/002`) or plain restore (`codex/spawn/001`).
- **Platform coverage:** mac+linux.

##### codex/spawn/005 — Respawning an existing pane as Codex launches through the Wrapper strategy (PRD #20, blocker 3).
- **Layer:** fast PTY registry integration (`AgentPtyRegistry::respawn_agent_for_pane` with PATH recorder stubs).
- **Agent:** synthetic Codex recorder.
- **Asserts:** replacing an existing pane process with bare command `codex` executes exactly `dot-agent-deck wrap --agent codex -- codex`.
- **Does not assert:** delegate routing that chooses respawn; it pins the respawn spawn boundary itself.
- **Platform coverage:** mac+linux.

##### codex/spawn/006 — An explicit Codex identity wraps a non-inferable custom launcher and remains the pane identity (PRD #20, R20-009).
- **Layer:** fast PTY registry integration (`AgentPtyRegistry::spawn_agent` with PATH recorder stubs).
- **Agent:** synthetic custom launcher explicitly declared as Codex.
- **Asserts:** a command whose basename is not `codex` still executes exactly through `dot-agent-deck wrap --agent codex -- ...` when the caller supplies `AgentType::Codex`, and the live registry records that pane as Codex.
- **Does not assert:** command-string inference (covered by the detection matrix) or real Codex behavior.
- **Platform coverage:** mac+linux.

##### codex/spawn/007 — A hook-learned Codex badge does not mutate a non-inferable pane's launch shape on respawn (PRD #225 M1).
- **Layer:** fast PTY registry integration (`AgentPtyRegistry::spawn_agent` + hook-path `set_agent_type` + `respawn_agent_for_pane`, with PATH recorder stubs).
- **Agent:** synthetic `devbox run codex-big` launcher whose basename intentionally does not infer an agent type.
- **Asserts:** the initial and replacement exec records are byte-identical `devbox run codex-big` lines even after the registry badge upgrades from `None` to `Some(Codex)`; no `dot-agent-deck wrap` line appears on respawn.
- **Does not assert:** daemon hook-socket ingestion of the badge (covered by `hooks/delivery/007`); an EDITED role command's effect on the wrap decision (`codex/spawn/008`); a config-declared identity (`codex/spawn/009`–`010`); real Codex behavior.
- **Platform coverage:** mac+linux.

##### codex/spawn/008 — A respawn's wrap decision follows the command it is actually launching, so an explicit Codex identity can never wrap a different agent (PRD #225 review finding 1).
- **Layer:** fast PTY registry integration (`AgentPtyRegistry::spawn_agent` + two `respawn_agent_for_pane` calls, with PATH recorder stubs for `devbox`, `claude`, and `dot-agent-deck`).
- **Agent:** synthetic `devbox run codex-big` launcher spawned with an explicit `AgentType::Codex` identity, then respawned once with that same command and once with the role command edited to `claude --model haiku`.
- **Asserts:** the unchanged respawn relaunches byte-identically as `dot-agent-deck wrap --agent codex -- devbox run codex-big` (the frozen identity is the only thing that knows this launcher is Codex); the edited respawn executes a bare `claude --model haiku` and never `wrap --agent codex -- claude …`; and the pane badge follows the newly launched command (`ClaudeCode`) instead of still advertising the replaced agent. Both halves are load-bearing — replaying the frozen identity verbatim wraps Claude as Codex, and dropping it flips the unchanged pane to bare.
- **Does not assert:** the hook-learned badge path (`codex/spawn/007`); a freshly re-read config declaration that legitimately outranks the current command's derived type (`codex/spawn/010`); a launcher whose command implies no type AND whose underlying agent changed (`devbox run codex-big` → `devbox run claude-big`), which keeps its creation-time identity by documented design.
- **Platform coverage:** mac+linux.

##### codex/spawn/009 — A config-declared Codex orchestration role wraps and badges a non-inferable launcher before the first task.
- **Layer:** L2 synthetic PTY-attached new-pane orchestration flow with PATH recorder stubs.
- **Agent:** synthetic `devbox run codex-big` launcher declared as Codex by the start role.
- **Asserts:** the role executes exactly once as `dot-agent-deck wrap --agent codex -- devbox run codex-big`, and its visible card reads `Codex` at spawn without any delegated task or synthesized hook event.
- **Does not assert:** `clear = true` re-create precedence (`codex/spawn/010`), mode panes (`codex/spawn/011`), or a real Codex process (`codex/spawn/012`).
- **Platform coverage:** mac+linux.

##### codex/spawn/010 — A current config declaration outranks command derivation across spawn and missing-record re-create without admitting learned identity into the exec line.
- **Layer:** fast PTY registry integration (`AgentPtyRegistry::spawn_agent` + `respawn_or_recreate_agent_for_pane` + `set_agent_type`, with PATH recorder stubs).
- **Agent:** synthetic Codex declaration applied to a command whose basename derives Claude Code.
- **Asserts:** declared Codex beats the conflicting Claude derivation at initial spawn and on two same-pane missing-record `clear = true` re-creates; a conflicting learned badge observation cannot replace the declared badge or alter the byte-identical Codex wrapper exec record.
- **Does not assert:** TOML parsing or the visible card (covered by `codex/spawn/009`); ordinary frozen-identity respawn precedence (covered by `codex/spawn/008`); daemon hook-socket ingestion.
- **Platform coverage:** mac+linux.

##### codex/spawn/011 — A config-declared Codex mode pane wraps and badges a shell-injected non-inferable launcher.
- **Layer:** L2 synthetic PTY-attached new-pane mode flow with PATH recorder stubs.
- **Agent:** synthetic `devbox run codex-big` launcher entered for a `[[modes]]` agent pane declared as Codex.
- **Asserts:** the mode's shell-injection seam executes exactly `dot-agent-deck wrap --agent codex -- devbox run codex-big`, and the mode agent's Dashboard card reads `Codex` without a hook event.
- **Does not assert:** restored mode panes, persistent side panes, orchestration roles (`codex/spawn/009`), or real Codex behavior.
- **Platform coverage:** mac+linux.

##### codex/spawn/012 — A real script-launched Codex role badges at spawn before its first prompt.
- **Layer:** L2 PTY-attached real-agent orchestration flow; runtime-skipped unless `check_codex_available` verifies the binary, persisted auth, and a live model request.
- **Agent:** real interactive Codex on the cheap test model, launched by a bespoke `run-codex.sh` role command whose config declares `agent = "codex"`.
- **Asserts:** the bespoke script starts the genuine Codex CLI and, before any prompt-bearing event or user input, the role card visibly reads `Codex` and `Idle`.
- **Does not assert:** prompt delivery, model prose, tool execution, `clear = true` respawn, or post-prompt native hook behavior.
- **Platform coverage:** mac+linux (real-agent tier is local-only).
- **Cost note:** one minimal mini-model availability probe; the launched interactive agent receives no prompt.

#### codex/hooks

##### codex/hooks/001 — A real launcher-script interactive Codex turn reports native prompt/tool detail and becomes Idle without process exit (PRD #20 W1, R20-013/R20-014, §4.3.7). [reel]
- **Layer:** L2 PTY-attached (`TuiDeck`, reel-eligible); runtime-skipped unless `check_codex_available` verifies the binary, persisted auth, and a live model request.
- **Agent:** real interactive Codex on the cheap test model, launched through a recorder script named `codex` ahead of PATH with isolated credentials and a fresh Codex home, workspace-write sandbox, no approvals, network-enabled sandbox configuration, and low reasoning effort; launch passes through the normal Wrapper strategy seam.
- **Asserts:** the launcher handles both the deck's `app-server` trust probe and the interactive agent without receiving `--dangerously-bypass-hook-trust`; the fresh home trusts exactly the deck's ten scoped hook keys; those hooks emit a prompt-bearing Thinking event, shell ToolStart/ToolEnd events with sentinel command detail, and Stop-hook Idle; the dashboard visibly retains prompt/tool detail and shows Idle, the requested sentinel contains exact known content, and the Codex pane is still alive because the test never sends `/exit`.
- **Does not assert:** stdout JSONL classification (covered by `codex/wrap/001`) or exact model prose.
- **Platform coverage:** mac+linux (real-agent tier is local-only).
- **Cost note:** one minimal mini-model availability probe plus one short interactive shell-tool turn.

##### codex/hooks/002 — A script-launched Codex inherits its pinned home and exactly the deck's scoped trust records (PRD #20 §4.3.5).
- **Layer:** L2 synthetic real-binary subprocess under the `e2e` feature; deterministic launcher and `codex app-server` stand-ins, no LLM.
- **Agent:** launcher script executing a Codex stand-in with all ten deck hook identities returned by hooks/list.
- **Asserts:** `hooks.json` is installed, the child inherits the pinned `CODEX_HOME`, child argv has no global bypass, and `[hooks.state]` names exactly the ten deck keys.
- **Does not assert:** rendered dashboard behavior or real Codex hook execution.
- **Platform coverage:** mac+linux.

##### codex/hooks/003 — Command-agnostic startup integration delivers Codex events from a non-Codex-basename launcher (PRD #20 §4.2.1/§4.3.6).
- **Layer:** L2 PTY-attached (`TuiDeck`, real binary + daemon, deterministic launcher and app-server stand-ins, no LLM).
- **Agent:** restored `/bin/sh startup-parity-launcher.sh` pane whose command cannot infer Codex identity; the launcher emits a Codex hook event only after startup scoped trust exists.
- **Asserts:** daemon/TUI startup installs and trusts Codex hooks independently of the pane command basename, then the emitted prompt visibly creates a Codex card showing Thinking and the prompt sentinel.
- **Does not assert:** wrapper classification, explicit role `agent = "codex"`, or real Codex execution.
- **Platform coverage:** mac+linux.

##### codex/hooks/004 — The documented Codex hook-install CLI succeeds (PRD #20 §4.2.1).
- **Layer:** L1/fast real-binary subprocess integration with an isolated home and deterministic Codex app-server stand-in.
- **Agent:** synthetic Codex installation environment.
- **Asserts:** `dot-agent-deck hooks install --agent codex` exits successfully and creates `hooks.json` instead of reporting `No hook installer for agent Codex`.
- **Does not assert:** uninstall scoping (covered by `codex/trust/003`) or dashboard rendering.
- **Platform coverage:** mac+linux.

#### codex/live

##### codex/live/001 — A real interactive cheap-model Codex run launched through the normal new-pane flow works visibly and reports live status (PRD #20, rule 4 / finding 16). [reel]
- **Layer:** L2 PTY-attached (`TuiDeck`, reel-eligible); runtime-skipped unless `check_codex_available` verifies the binary, persisted auth, and a live model request.
- **Agent:** real interactive bare `codex` using `gpt-5.1-codex-mini`, isolated copied credentials, workspace-write sandbox, and low reasoning effort; automatic wrapping occurs at the normal pane spawn seam.
- **Asserts:** the interactive pane becomes ready, accepts a typed prompt, uses the shell to list the fixture and writes a proof file naming `codex_sentinel_a7c91f.txt`; after detach, the visible Codex card has traversed Thinking → Idle.
- **Does not assert:** exact model phrasing or token usage.
- **Platform coverage:** mac+linux (real-agent tier is local-only).
- **Cost note:** one minimal mini-model availability probe plus one short interactive directory-listing/file-write turn.

#### codex/worker

##### codex/worker/001 — A real wrapped Codex orchestration worker receives a delegated task, does the work, and signals work-done (PRD #20 parity gap #12).
- **Layer:** L2 headless in-process daemon plus a real interactive Codex PTY; runtime-skipped unless `check_codex_available` verifies the CLI, persisted auth, and model access.
- **Agent:** real `gpt-5.1-codex-mini` Codex configured as the `coder` role with workspace-write sandboxing, approval disabled, low reasoning effort, isolated copied credentials, and project trust; the common spawn seam automatically launches it through `dot-agent-deck wrap`.
- **Asserts:** Codex auto-submits the daemon-injected single-line `worker-task-coder.md` pointer, reads the delegated task, creates `codex_worker_sentinel_c81f2a.txt` with exact known contents, and runs the task footer's `dot-agent-deck work-done` command so the daemon writes `.dot-agent-deck/work-done-coder.md`.
- **Does not assert:** exact model phrasing, token usage, or dashboard rendering (covered by `codex/live/001`).
- **Platform coverage:** mac+linux (real-agent tier is local-only).
- **Cost note:** one minimal mini-model availability probe plus one short worker turn.

#### devin/live

##### devin/live/001 — A real interactive Devin turn drives the dashboard card live through the deck's own installed hooks. [reel]
- **Layer:** L2 PTY-attached (`TuiDeck`, reel-eligible); runtime-skipped unless `check_devin_available` verifies the binary, persisted credentials, and a logged-in account.
- **Agent:** real interactive `devin` restored into a pane, using the account's default (cheap SWE-family) model with isolated copied credentials, the setup wizard pre-satisfied, workspace trust waived, and `--permission-mode auto`; launch goes through the normal `NativeHooks` seam with no wrapper.
- **Asserts:** the deck-written `"hooks"` block in Devin's own config is actually read and executed by the third-party binary — a typed prompt produces a Devin-stamped Thinking event and a visible Thinking card, an `exec` ToolStart carrying a non-empty tool detail, the pane showing `devin_live_sentinel_4c81de.txt`, and a Stop-driven Idle.
- **Does not assert:** exact model phrasing or token usage; hook payload parsing in isolation (covered by the fast-tier `devin_hook_ingestion` tests) or config-merge safety (covered by the `devin_hooks_manage` unit tests).
- **Platform coverage:** linux+mac (real-agent tier is local-only; `devin_config_dir` is Unix-only by design).
- **Cost note:** one inference-free `devin auth status` probe plus one short interactive directory-listing turn — measured at roughly 2.7s of agent time. No `--model` is pinned because a free-tier account rejects every explicit model.

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
- **Layer:** L2 (in-process daemon whose hook loop routes `delegate`/`work-done`/`agent-event` and re-broadcasts `AgentEvent`s; real agent PTYs via `AgentPtyRegistry::spawn_agent` — the `e2e` tier, hits a real model). Mirrors `e2e_delegate_work_done_chain.rs` with the ORCHESTRATOR role swapped to `pi`: the worker (spawned + ready first) is a black-box `claude` with its hooks/CLI unchanged; the orchestrator is a real `pi` whose HOME carries the bundled extension (materialized via `orchestrator_ext::materialize`). `ANTHROPIC_API_KEY` + `HOME` are explicitly propagated into the pi child's `opts.env` (the key is never printed).
- **Agent:** REAL `pi` 0.80.6 orchestrator (`--provider anthropic --model claude-haiku-4-5 --approve`, the cheapest tier in pi's Anthropic catalog — TEMPORARY: the pi tier is on Anthropic Haiku while the GPT accounts are without credit; the tier is provider-agnostic and the model is a one-line change) + REAL Claude Code (Haiku, `claude-haiku-4-5-20251001`, `--allowedTools Bash Read Write`) worker. Flaky-tolerant lane-2 tier (real LLM) — run once, not looped (rule 4/5). Runtime-skipped (Decision 26) when `pi`/`claude`/credentials/`ANTHROPIC_API_KEY` are absent.
- **Asserts:** the directive-prompted pi calls the native `delegate` tool once (role `coder`), the daemon routes it into the pre-spawned worker pane, and the real worker creates the sentinel `pi_orch_sentinel_7c3f.txt` (contents `PI_ORCH_SENTINEL_OK`) via the delegated task (proves the full pi→daemon→worker route ran); the daemon writes `.dot-agent-deck/work-done-coder.md` (work-done returned to the orchestrator); and a `Pi`-typed `AgentEvent` for the orchestrator pane rode the daemon's broadcast — status tracked through the extension's `agent-event` path with NO hook installed. Generous per-step timeouts (240s sentinel / 120s work-done) sized to confidence, not token cost (Design Decision #7).
- **Does not assert:** exact agent phrasing / the exact task text pi forwards (the sentinel filename + content are the literal tokens that must survive); the extension's per-event state mapping (covered deterministically by the TS unit tests + synthetic `status/agent-event/003`); the daemon's routing/role-guard internals (covered by `orchestration/delegate/*`).
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** one short Haiku turn through pi (orchestrator delegates) + one short Haiku turn through claude (worker creates a file + work-done) — well under Decision 23's <$0.05/run bound.

##### chain-smoke/pi/002 — A REAL `pi` WORKER receives a daemon-injected delegate (the agent-agnostic `worker-task-<role>.md` footer + `write_to_pane_and_submit` path), does the task (creates a uniquely-named sentinel), and signals `work-done` back — proving pi's SECOND role (PRD #201 completeness; the orchestrator role is pinned by `chain-smoke/pi/001` + `pi/live/002`).
- **Layer:** L2 (in-process daemon whose hook loop ingests `work-done` over the socket; a real pi worker PTY via `AgentPtyRegistry::spawn_agent` — the `e2e` tier, hits a real model). HEADLESS (a functional proof, not a reel clip — the orchestrator-role reel clip is `pi/live/002`). Reuses the real-pi machinery of `chain-smoke/pi/001`: the pi worker's HOME carries the bundled extension (materialized via `orchestrator_ext::materialize`), and `ANTHROPIC_API_KEY` + `HOME` (+ pane/socket/PATH) are explicitly propagated into the pi child's `opts.env` (the key is never printed). The ORCHESTRATOR side is the DETERMINISTIC synthetic-delegate path — `AppState::handle_delegate` with a synthetic `DelegateSignal` from an un-spawned orchestrator pane (the pattern of `e2e_delegate_work_done_chain.rs`) — chosen because the WORKER is the thing under test and a real orchestrator would add LLM flakiness without adding to the worker proof (the genuine real-pi-orchestrator ⇄ real-worker mix is already pinned by `chain-smoke/pi/001` + `pi/live/002`). `clear = false` (no `.dot-agent-deck.toml` role config ⇒ `handle_delegate` role lookup returns `None` ⇒ no respawn): the pi worker is spawned ONCE and the delegate injects only after it is polled to genuine input-readiness — deliberately isolating the worker proof from the separately-tracked `clear = true`-respawn + 10s-`SESSION_START_WAIT`-fallback fragility (pi never emits `EventType::SessionStart`).
- **Agent:** REAL `pi` (`--provider anthropic --model claude-haiku-4-5 --approve`, the cheapest tier in pi's Anthropic catalog — TEMPORARY: the pi tier is on Anthropic Haiku while the GPT accounts are without credit; the tier is provider-agnostic and the model is a one-line change) spawned IDLE (no CLI-arg prompt) as the `coder` WORKER pane — seeded ONLY by the daemon-injected worker-task pointer. Flaky-tolerant lane-2 tier (real LLM) — run once, not looped (rule 4/5). Runtime-skipped (Decision 26) when `pi` / `ANTHROPIC_API_KEY` is absent.
- **Asserts:** the full WORKER chain — the pi worker AUTO-SUBMITTED the daemon-injected single-line `worker-task-coder.md` pointer, read its task file, and created the sentinel `pi_worker_sentinel_9d2e.txt` (contents `PI_WORKER_SENTINEL_OK`) — proving it RECEIVED and DID the delegated task; and the daemon wrote `.dot-agent-deck/work-done-coder.md`, proving the pi worker SIGNALLED work-done over the hook socket (via the footer `dot-agent-deck work-done` CLI or the extension's native `work_done` tool — either routes the same `WorkDone` signal, so the file's appearance is a path-agnostic proof). Generous per-step timeouts (240s sentinel / 120s work-done) sized to confidence, not token cost (Design Decision #7).
- **Does not assert:** exact agent phrasing / the exact task text (the sentinel filename + content are the literal tokens that must survive); WHICH work-done path pi took (CLI vs native tool — both produce the same file); the `clear = true`-respawn worker path with pi (isolated out via `clear = false`; that path's 10s-fallback fragility is tracked for the companion PRD); the extension's per-event status mapping (covered by the TS unit tests + synthetic `status/agent-event/003`).
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** one short Haiku worker turn (read a task file, create a file, work-done) — well under Decision 23's <$0.05/run bound.

#### pi/live

##### pi/live/001 — A REAL `pi` agent runs LIVE in a PTY-attached pane and its card renders the experimental-gated Pi identity plus a real, extension-driven status TRANSITION on the vt100 grid, with NO hook (PRD #201, CLAUDE.md rule 4 + PRD #180 reel-eligibility).
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness — records a `full-stream.cast`, so it is demo-reel-eligible per PRD #180, unlike the HEADLESS `chain-smoke/pi/001` + `scheduler/pi/001`). Mirrors `e2e_issue_dispatch_real.rs` / `e2e_chain_smoke_claude.rs` and the reference `scheduler/dispatch/013`. The bundled extension is materialized into the per-test HOME BEFORE launch (`TuiDeckBuilder::with_pi_extension`) so the deck's lazy-spawned daemon — and the pi child it spawns, which inherits that HOME — auto-discovers it at boot; `ANTHROPIC_API_KEY` + the built-binary PATH are threaded into the deck via `with_env` (the key is never printed). Launched with `DOT_AGENT_DECK_EXPERIMENTAL=1`.
- **Agent:** REAL `pi` (`--provider anthropic --model claude-haiku-4-5 --approve`, the cheapest tier in pi's Anthropic catalog — TEMPORARY: the pi tier is on Anthropic Haiku while the GPT accounts are without credit; the tier is provider-agnostic and the model is a one-line change) as a single interactive pane restored from a staged saved session. Flaky-tolerant lane-2 tier (real LLM) — run once, not looped (rule 4/5). Runtime-skipped (Decision 26) when `pi` / `ANTHROPIC_API_KEY` is absent.
- **Asserts (on the rendered vt100 grid):** after detaching to the dashboard, the Pi pane's card shows a REAL, extension-driven status TRANSITION with NO hook installed — `Thinking` (extension `agent_start`→running), an Idle → running transition the daemon never produces on its own for a hook-less Pi pane (a freshly-spawned pane defaults to Idle, and `session_start` now reports Idle for parity with Claude/OpenCode/Codex, so `Thinking` — not the retired `Needs Input` — is the extension-only proof), then a settle back to `Idle` (extension `agent_settled`→finished, polled on the CURRENT grid so it can only be the post-turn settled frame — this is the turn-end→Idle mapping the fix changed, so a regression to "Needs Input" fails here) — and the card title carries the experimental-flag-gated first-class Pi identity (`Pi ·`, the `AgentType` Display, which the lowercase `pi` command never produces). The `Thinking` step is scanned over the rolling byte history so a transient frame still matches; generous 180s ceilings sized to confidence (Design Decision #7).
- **Does not assert:** the orchestrator→worker delegation chain (a single live Pi pane fully satisfies rule 4; the delegate route is pinned headless by `chain-smoke/pi/001` and LIVE + injection-seeded by `pi/live/002`); any specific text pi prints; the directed sentinel file (`pi_live_sentinel_4b1a.txt`) is a best-effort/logged secondary signal, not a gate, since the rendered status transition already proves the pi turn ran.
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** one cheap Haiku directive turn (create one file) — well under Decision 23's <$0.05/run bound.

##### pi/live/002 — A REAL `pi` orchestrator AUTO-SUBMITS a daemon-INJECTED seed (the production restore-path injection, NOT a CLI arg) and drives a full orchestration LIVE on the vt100 grid: pi → native `delegate` → real `claude` Haiku worker creates a uniquely-named sentinel + signals `work-done` (PRD #201 parity GAP #1 + the real-usage orchestration reel clip).
- **Layer:** L2 PTY-attached (the REAL `dot-agent-deck` binary driven through the vt100 `TuiDeck` harness — records a `full-stream.cast`, demo-reel-eligible per PRD #180). Closes the gap left by `chain-smoke/pi/001` (headless, CLI-arg seeded) and `pi/live/001` (single live pane): a two-role orchestration is staged as a `.dot-agent-deck.toml` (`[[orchestrations]] name = "pi-parity"`, an `orchestrator` START role running an IDLE real `pi` + a `coder` role running an IDLE real `claude`, neither carrying a CLI-arg prompt) plus a `session.toml` whose `OrchestrationSnapshot.orchestrator_prompt` is the delegate directive; on the daemon-empty restore the deck spawns both role panes IDLE and REPLAYS the directive into the pi START role via the PRODUCTION `write_and_submit_to_pane` injection primitive (single-line write, SUBMIT_DELAY, then `\r`) — the exact auto-submit path shipped code relies on. The bundled Pi extension is materialized into the per-test HOME BEFORE launch (`with_pi_extension`); imported Claude credentials + `with_claude_project_trust` for the shared orchestration cwd clear the worker's first-run gates; `ANTHROPIC_API_KEY` + the built-binary PATH are threaded in via `with_env` (the key is never printed). Launched with `DOT_AGENT_DECK_EXPERIMENTAL=1`.
- **Agent:** REAL `pi` orchestrator (`--provider anthropic --model claude-haiku-4-5 --approve`, seeded ONLY by injection — no CLI-arg prompt; the cheapest tier in pi's Anthropic catalog, TEMPORARY: the pi tier is on Anthropic Haiku while the GPT accounts are without credit, and the tier is provider-agnostic so the model is a one-line change) + REAL Claude Code (Haiku, `claude-haiku-4-5-20251001`, `--allowedTools Bash Read Write`) worker with the NORMAL toolset (no `--no-builtin-tools`, no stand-ins). Flaky-tolerant lane-2 tier (real LLM) — run once, not looped (rule 4/5). Runtime-skipped (Decision 26) when `pi` / `claude` / credentials / `ANTHROPIC_API_KEY` are absent.
- **Asserts:** AUTO-SUBMIT CHECKPOINT (GAP #1) — the daemon writes `.dot-agent-deck/worker-task-coder.md` ONLY inside `handle_delegate`, so its appearance is the isolated proof that pi AUTO-SUBMITTED the daemon-INJECTED seed and called the native `delegate` tool; the delegate pointer `worker-task-coder` renders LIVE in the worker pane on the orchestration grid (the user-visible "delegation happening + worker" reality); and the full chain landed — the delegated worker created the sentinel `pi_inject_orch_sentinel_5e8c.txt` (contents `PI_INJECT_ORCH_OK`) and the daemon wrote `.dot-agent-deck/work-done-coder.md`. Generous per-step ceilings sized to confidence, not token cost (Design Decision #7).
- **Does not assert:** the experimental-gated `Pi ·` card title (a named orchestration role pane titles its card with the ROLE name, not the agent-type identity — that surface is pinned by `pi/live/001` + `dashboard/pane/007`); the exact task text pi forwards (the sentinel filename + content are the literal tokens that must survive LLM phrasing); the extension's per-event state mapping (covered by the TS unit tests + synthetic `status/agent-event/003`).
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** one short Haiku turn through pi (orchestrator delegates) + one short Haiku turn through claude (worker creates a file + work-done) — well under Decision 23's <$0.05/run bound.

### Mouse Parity (PRD #80)

These entries cover PRD #80 (mouse parity for keyboard actions): every keyboard-only TUI action gains a clickable affordance carrying its shortcut inline, funneled through the single `dispatch_action` action layer.

#### mouse/dispatch

##### mouse/dispatch/001 — Ctrl+N (key) and a click on a New-Pane button rect map to the same `Action::NewPane`.
- **Layer:** pure-data (plain logic, no TUI harness).
- **Agent:** none.
- **Asserts:** `global_ctrl_action(Ctrl+N)` and `hit_test_button` on a synthetic New-Pane button rect both yield `Action::NewPane`; a click that misses every rect yields `None`.
- **Does not assert:** rendering or end-to-end dispatch side effects.
- **Platform coverage:** mac+linux+windows.

#### mouse/hyperlink

##### mouse/hyperlink/001 — The Ctrl+click "Opened:" status shortens an agent-controlled URL on a char boundary (issue #574).
- **Layer:** pure-data (plain logic, no TUI harness).
- **Agent:** none (URL strings as they arrive from the embedded pane's OSC-8 hyperlink map).
- **Asserts:** `opened_link_status` — the exact function the live Ctrl+click arm calls — leaves a short URL whole, and shortens an over-long one to the longest char-boundary prefix within its 57-byte budget plus an `…`, for 2-, 3- and 4-byte characters at every ASCII offset that puts byte 57 mid-character. Before the fix `&url[..57]` panicked the event loop on a URL an agent had written into its own PTY.
- **Does not assert:** the hit-test that finds the URL under the cursor, or the `open::that` call itself (both are outside this seam); the session-card id, which is the same defect on a different string (`dashboard/pane/011`).
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

##### mouse/buttonbar/007 — The dimmed Close button is inert outside command mode.
- **Layer:** L2 (real-binary PTY with production button rendering and SGR mouse hit-testing).
- **Agent:** none (continued `cat` pane).
- **Asserts:** Help mode still visibly renders `[Close Ctrl+W]`; clicking it arms neither the pane-scoped nor tab-scoped close confirmation; Help's own `[Close]` then dismisses the overlay normally; the daemon agent remains alive.
- **Does not assert:** the DIM cell modifier itself (covered through the live buffer path by `keybindings/hints/003`).
- **Platform coverage:** mac+linux.

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
- **Asserts:** the strip renders exactly one `×` per closeable tab and none for the Dashboard; clicking a Mode tab's `[×]` leaves the tab intact behind the tab-scoped `Close this tab and all its panes?` Cancel-default confirmation, and Down+Enter then closes it.
- **Does not assert:** which tab gets focus after close.
- **Platform coverage:** mac+linux (L1 half: +windows).

##### mouse/tabstrip/003 — An inactive tab's `×` binds confirmation to that stable tab while modal navigation is suppressed.
- **Layer:** L2 (real-binary PTY with two distinct synthetic Mode tabs and production SGR mouse/key dispatch).
- **Agent:** none (the `alpha` and `beta` fixture modes run long-lived side panes with unique rendered sentinel text).
- **Asserts:** with `BETA_TAB_SENTINEL` active, clicking the inactive alpha tab's `×` arms alpha with tab-scoped copy; Ctrl+PageUp and Ctrl+PageDown leave beta rendered; confirmation removes alpha while beta and its single remaining `×` survive.
- **Does not assert:** dashboard-session identity replacement (covered by `prompt/close-confirm/005`).
- **Platform coverage:** mac+linux.

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

##### theme/contrast/002 — The `WaitingForInput` status colour is legible on light and dark terminals (WCAG contrast, not colour identity).
- **Layer:** L1 (pure computation over `palette::STATUS_WAITING`; no rendering).
- **Agent:** none.
- **Asserts:** `palette::STATUS_WAITING`, resolved through the reference xterm ANSI palette into its base and bold-as-bright renderings, clears WCAG AA for text (4.5:1) on the two theme-matched pairings — the base slot on a white background, the bright slot on a black one — and WCAG AA for non-text UI components (3:1) on the two mismatched pairings (bright-on-white, base-on-black); and that the role stays a distinct colour from every other palette role. A paired non-spec unit-guard pins the helpers against the 21:1/1:1 endpoints and reproduces issue #579's reported 1.70:1 and 1.07:1 yellow measurements, then proves the floors still reject `Color::Yellow`.
- **Does not assert:** which colour the role holds (deliberately — `theme/palette/001-002` own the identity, and asserting identity here is what let an unreadable colour ship); contrast of the other status/accent roles (green 2.16:1 and cyan 1.98:1 on white have the same weakness and are a separate colour decision); how any specific terminal emulator actually resolves the slot.
- **Platform coverage:** mac+linux+windows.

#### theme/guard

##### theme/guard/001 — No absolute background on any cheaply-seamable surface; command-mode selection is cued by the terminal's own foreground plus a thickened border, not an absolute fill.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none.
- **Asserts:** rendering the five overlay seams plus a session card in both the unselected and selected states **in command mode** (`UiMode::Normal`), (a) no cell carries a `Color::Rgb(..)` background — backgrounds must be `Color::Reset`; and (b) the selected card is distinguished from the unselected one by terminal-relative cues — the `▸ ` title prefix, a border in the terminal's own foreground (`Color::Reset`), and a thickened `┃` glyph where the unselected card draws `│` — rather than an absolute `selected_bg` fill, and that the selected border is never `DIM` (issue #442).
- **Does not assert:** named-ANSI accents/status colors; the `render_frame` canvas/tab-bar fills (not cheaply reachable through a render seam — guarded by `theme/guard/002`); the per-mode emphasis of the selection (covered by `mode/deck/001`); the all-statuses sweep proving a selected border never inherits a low-contrast status colour (covered by `theme/palette/006`).
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
- **Asserts:** rendering a deck card (not selected, not focused) for each agent status resolves its border to the matching centralized status role — working=`Color::Green`, thinking=`Color::Blue`, compacting=`Color::Blue` (shares the thinking role), waiting=`Color::Yellow`, error=`Color::Red`, idle=`Color::DarkGray`; and that no status border reuses the `focused` accent (`Color::Cyan`) or the retired `selected` accent (`Color::Magenta`), so a status never collides with focus.
- **Does not assert:** the per-card status badge text/glyph; the selection glyph and the focus accent (covered by `theme/palette/003-004`, `theme/palette/006`); the palette module's internal API (reads the rendered border color).
- **Platform coverage:** mac+linux+windows.

##### theme/palette/002 — Embedded-pane border uses the SAME status color the deck card uses (deck/pane consistency).
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (six live session fixtures + a `TerminalWidget` per status).
- **Asserts:** for each agent status (including compacting, which shares the thinking/Blue role), the embedded pane's border color (neither selected nor focused) equals the deck card's border color for that status, and both equal the palette status role — so a given state looks identical as a deck card and as an embedded pane (PRD #155 success criterion #2).
- **Does not assert:** pane content/title rendering; the focused/selected pane accents (covered by `theme/palette/004` / `theme/guard/001`).
- **Platform coverage:** mac+linux+windows.

##### theme/palette/003 — Selected deck-card border in command mode is the terminal's own foreground, with a thick glyph + BOLD + marker.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (one selected live session fixture).
- **Asserts:** rendering a selected deck card for a Working agent **in command mode** (`UiMode::Normal`) resolves its border to `palette::SELECTED` (`Color::Reset`, the terminal's own foreground) — explicitly NOT the working-status `Color::Green`, the retired Magenta accent, or the focused-pane Cyan — carried together with a thick `┃` glyph, `Modifier::BOLD` and a `▸ ` title marker, and with no `Modifier::DIM` (issue #442).
- **Does not assert:** the status badge (still shows status independent of selection); the absolute-background guard (covered by `theme/guard/001`); the PaneInput emphasis of the same selection (covered by `mode/deck/001`); the all-statuses/both-modes sweep (covered by `theme/palette/006`).
- **Platform coverage:** mac+linux+windows.

##### theme/palette/004 — Focused-pane border is the dedicated `focused` accent (Cyan), distinct from every status and from `selected`.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (one focused `TerminalWidget`).
- **Asserts:** rendering a focused embedded pane resolves its border to `Color::Cyan`, and that this color is distinct from every status role (green/blue/yellow/red/dark-gray) and from the retired Magenta accent — focus keeps the only accent HUE while deck selection uses the terminal's own foreground plus thickness, so status/selection/focus stay provably distinct (PRD #155 success criterion #3, issue #442). Also asserts the PRECEDENCE invariant: a pane that is focused AND carries a present `Working` status still renders the focused accent (Cyan), never the Working/Green status color — focus OVERRIDES a present status in the unified border precedence (Option A).
- **Does not assert:** unfocused-pane status coloring (covered by `theme/palette/002`); pane content rendering; the command-mode half of the focus precedence (covered by `theme/palette/005`).
- **Platform coverage:** mac+linux+windows.

##### theme/palette/005 — A focused pane in command mode drops the Cyan accent for its status color and thickens its border.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (one `TerminalWidget` rendered twice, live vs. command mode).
- **Asserts:** rendering the SAME focused pane with `input_active=true` (`UiMode::PaneInput`) vs. `input_active=false` (command mode) produces visually distinguishable borders — live resolves to `Color::Cyan` on a thin `│` (`BorderType::Plain`) border, command mode falls through to the agent's status role (`Working`=`Color::Green`) on a thick `┃` (`BorderType::Thick`) border — and that the two colors differ, so colour encodes whether keystrokes reach the pane while thickness still encodes which pane is focused. Also asserts an UNFOCUSED pane keeps the thin border in BOTH modes, so thickness stays exclusive to the focused pane.
- **Does not assert:** that the inner area / PTY size is unaffected by the border weight (`BorderType` never feeds `Block::inner`, and the PRD #84 invariant-3 contract assert covers a regression there); the bottom-bar and hint-string mode cues (covered by the PRD #241 M4 button-bar specs); the status-less focused pane's dim fallback.
- **Platform coverage:** mac+linux+windows.

##### theme/palette/006 — A selected deck card is visible at every status: terminal-foreground border, thickened glyph, never dimmed.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, color-aware capture).
- **Agent:** none (one live session fixture per status, rendered selected and unselected in both modes).
- **Asserts:** for every agent status and in BOTH `UiMode::Normal` and `UiMode::PaneInput`, a SELECTED card's border resolves to `palette::SELECTED` (`Color::Reset`) and never to that status's role colour, thickens its glyph from `│` to `┃`, and never carries `Modifier::DIM`. The CONTROL in the same loop is that the UNSELECTED card is untouched — still its status role, still `│` — so an idle agent keeps receding. Guards issue #442 in both of its reported forms: selection dimmed into the `palette::STATUS_IDLE` band (the original report), and a selected idle card inheriting DarkGray so that thickening its border changed nothing (the follow-up).
- **Does not assert:** the `▸ ` title marker (covered by `theme/palette/003` / `theme/guard/001`); the BOLD-vs-plain mode emphasis (covered by `mode/deck/001`); embedded-pane borders (covered by `theme/palette/002`, `004`, `005`).
- **Platform coverage:** mac+linux+windows.


### Mode indication (PRD #341)

#### mode/cursor

##### mode/cursor/001 — The painted terminal cursor appears only while pane input is active.
- **Layer:** L1 (in-process `TerminalWidget` rendered into a `ratatui::buffer::Buffer`; no PTY, no subprocess).
- **Agent:** none (one focused vt100 fixture rendered twice).
- **Asserts:** with `input_active=true`, the known cursor cell retains today's exact black-on-`LightGreen` bold block styling; with `input_active=false`, the same cell is styled identically to its neighbouring non-cursor cells and carries no cursor modifier, so command mode renders no painted cursor of any kind.
- **Does not assert:** the terminal emulator's own cursor (covered by `mode/cursor/002`); pane-border mode styling (covered by `theme/palette/005`).
- **Platform coverage:** mac+linux+windows.

##### mode/cursor/002 — The terminal emulator cursor is hidden in command mode.
- **Layer:** L1 (ratatui `TestBackend` frame rendering through the production focused-pane path; no PTY subprocess).
- **Agent:** none (one in-memory focused pane fixture).
- **Asserts:** the same focused-pane frame requests a visible terminal cursor in `UiMode::PaneInput` and no terminal cursor in `UiMode::Normal`, proving command mode skips `Frame::set_cursor_position`.
- **Does not assert:** painted cursor-cell styling (covered by `mode/cursor/001`); cursor shape; unfocused panes; modal input cursors outside the terminal-pane path.
- **Platform coverage:** mac+linux+windows.

#### mode/chip

##### mode/chip/001 — The bottom bar persistently names the current mode.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, rendered through `render_button_bar_for_mode_to_buffer` and the live `render_bottom_bar` path).
- **Agent:** none.
- **Asserts:** command mode begins with ` COMMAND ` and PaneInput begins with ` TYPING `; both chips use `Modifier::REVERSED | Modifier::BOLD`, carry no `Color::Rgb`, and the snapshot pins the complete production bar in both modes.
- **Does not assert:** behavior after clicking the adjacent destination button; narrow-width wrapping; banner or pane-dimming behavior.
- **Platform coverage:** mac+linux+windows.

##### mode/chip/002 — The current-mode chip is universal and coexists with the destination button.
- **Layer:** L1 (ratatui `TestBackend` through the production global-only and context-rich bottom-bar paths).
- **Agent:** none.
- **Asserts:** Dashboard, Mode, and Orchestration contexts place the chip at the same left-edge position; command mode shows ` COMMAND ` with `[Back to Pane Ctrl+D]`, while PaneInput shows ` TYPING ` with `[Command Mode Ctrl+D]`, so the current-state label never replaces the destination affordance.
- **Does not assert:** click dispatch for the destination button; exact spacing after the chip; context-specific buttons after the universal prefix.
- **Platform coverage:** mac+linux+windows.

##### mode/chip/003 — Narrow mode chips disappear symmetrically without changing the bar's row budget.
- **Layer:** L1 (ratatui `TestBackend` through the production bottom-bar renderer; no PTY or subprocess).
- **Agent:** none.
- **Asserts:** across every width 0–24, ` COMMAND ` is present if and only if ` TYPING ` is present, both are absent below the shared 10-column threshold and present at or above it, and Normal/PaneInput/Filter/Rename rendering never panics; the command bar's reserved and rendered rows remain 11/5/3/2/1 at widths 19/40/80/120/200.
- **Does not assert:** click dispatch, exact button placement within each wrapped row, or full-frame card geometry (covered by `render/layout/004`).
- **Platform coverage:** mac+linux+windows.

#### mode/banner

##### mode/banner/001 — A fresh command-mode entry dims only the focused pane and centres the full block banner without erasing agent output.
- **Layer:** L1 (in-process production focused-pane renderer through a `TestBackend`-backed buffer seam plus `insta` style-aware capture).
- **Agent:** none (one synthetic vt100 pane rendered focused in command mode and PaneInput, then unfocused in command mode).
- **Asserts:** a roomy focused command pane selects the full block-letter tier, centres its REVERSED block region and `Ctrl+D to type` subtitle, retains readable underlying agent output, and applies DIM throughout the inner area except where the banner overlays it; the same focused PaneInput pane and an unfocused command pane have neither banner nor DIM, and no rendered cell uses `Color::Rgb`.
- **Does not assert:** timed decay or input-driven collapse (covered by `mode/banner/003`); narrow fallback geometry (covered by `mode/banner/002` and `/004`); terminal-specific visual support for DIM or the live binary path (M6 L2 scope).
- **Platform coverage:** mac+linux+windows.

##### mode/banner/002 — The narrow-pane fallback ladder is pure, monotonic, safe, and always fits.
- **Layer:** unit/L1 (pure `command_banner_tier(width, height)` width/height sweep; no renderer, PTY, or subprocess).
- **Agent:** none.
- **Asserts:** all five tiers own a reachable size band in the documented order; 0×0, 1×1, very-wide/one-row, and very-tall/three-column areas safely omit; every selected tier reports rendered dimensions within the available inner area; increasing either dimension never selects a lower tier.
- **Does not assert:** glyph shapes, centring, modifiers, or clipping in the actual buffer (covered by `mode/banner/001` and `/004`).
- **Platform coverage:** mac+linux+windows.

##### mode/banner/003 — Banner decay is deterministic, asymmetric for bound versus unbound keys, and re-arms on every entry.
- **Layer:** L1 state-machine unit using an injected `Instant`; no sleep, PTY, or subprocess.
- **Agent:** none.
- **Asserts:** the named TTL is 2.5 seconds; fresh entry is expanded until the TTL and collapsed at expiry; a command-mode Action and a bottom-bar click collapse early; an unbound printable holds the banner before decay and re-asserts it with a fresh clock after collapse; leaving hides/clears it and re-entry expands it again.
- **Does not assert:** command keybinding resolution itself; rendering or persistent DIM (covered by `mode/banner/001` and `/004`); wall-clock scheduling in the 16ms live loop.
- **Platform coverage:** mac+linux+windows.

##### mode/banner/004 — Every degraded and collapsed banner state stays inside the focused pane.
- **Layer:** L1 (production pane render seam under `catch_unwind` at tier-boundary sizes plus `insta` text/style capture).
- **Agent:** none (synthetic vt100 content in small-but-valid focused panes).
- **Asserts:** nonempty 0×0, 1×1, 2×2, 1×40, and 40×1 pane renders do not panic and return the exact requested buffer size; all three release-exposed controller seam paths resolve a single axis just above `PTY_RESIZE_DIM_MAX` to the safe 24×80 parser fallback; tiers 2–4 render their exact block-COMMAND, full reversed line, and reversed word fallbacks entirely inside the inner area; tier 5 omits safely; all valid bordered sizes retain DIM and avoid `Color::Rgb`; after decay the pane stays dim/readable with no banner while the bottom bar still carries the persistent ` COMMAND ` chip.
- **Does not assert:** the full tier-1 banner (covered by `mode/banner/001`); the transition rules that produce Collapsed (covered by `mode/banner/003`); M6 PTY/real-agent behavior.
- **Platform coverage:** mac+linux+windows.

##### mode/banner/005 — Same-drain mode edges preserve the command banner's real key semantics.
- **Layer:** L1 (in-process production `handle_key_event` burst observer with no render between keys; no PTY or subprocess).
- **Agent:** none (one inert focused pane).
- **Asserts:** a queued double-`Ctrl+D` burst traverses Normal → PaneInput → Normal and re-expands the banner; `Ctrl+D` then bound `Ctrl+T` from PaneInput lands Normal → Normal and stays Collapsed; single bound `Ctrl+T`, bound-then-unbound-printable, and single PaneInput exit control rows retain their distinct Collapsed/Expanded outcomes, with the before-burst visibility pinned for every case.
- **Does not assert:** mouse bursts, wall-clock TTL expiry (covered by `mode/banner/003`), or rendered banner geometry.
- **Platform coverage:** mac+linux+windows.

#### mode/deck

##### mode/deck/001 — The selected deck-card emphasis is full-strength only in command mode, and is never dimmed.
- **Layer:** L1 (ratatui `TestBackend` + `insta`, colour-and-modifier-aware card capture through the production renderer).
- **Agent:** none (one synthetic selected Working session rendered in both modes).
- **Asserts:** command mode remains byte-identical to the legacy selected-card seam and carries `palette::SELECTED` (`Color::Reset`) on a thick `┃` border with BOLD and `▸ `; PaneInput keeps the same colour, the same thick glyph and `▸ ` but drops BOLD; NEITHER mode carries `Modifier::DIM`, since dimming the selection is what made it read as an idle card (issue #442); neither rendering contains `Color::Rgb`; the snapshot pins both styled cards.
- **Does not assert:** unselected-card styling (covered by `theme/palette/006`); focused terminal-pane styling; statuses other than `Working`.
- **Platform coverage:** mac+linux+windows.

#### mode/scroll

##### mode/scroll/001 — Focused agent-pane wheel routing explains mature zero-depth panes without false positives.
- **Layer:** L1 (in-process production wheel and pane-render seams over real vt100 parsers, with a recording child-input channel and synthetic DECSTBM/cursor-repaint, trivial-output, and plain-stream fixtures; no PTY subprocess).
- **Agent:** none (agent-agnostic synthetic byte streams; the trigger names no agent).
- **Asserts:** every mode × child-mouse cell that routes to deck scrollback arms the notice when the live vt100 retained-line depth is zero despite at least 8 `rows * cols` screenfuls fed since spawn; PaneInput+child-mouse forwards exactly one report and never arms it; a substantial plain stream has nonzero live depth, scrolls normally, and stays notice-free; trivial fresh output stays notice-free; two focus/reconcile frames with substantial zero-depth output but no scroll attempt prove the notice is reactive rather than proactive. The depth assertion explicitly measures vt100's live clamp rather than the 10,000-line configured capacity.
- **Does not assert:** wheel-down direction (the same production route receives a direction parameter), real terminal mouse-report decoding (covered by the next scanner delegation), side-pane pointer hit-testing, real-agent behavior, or that any particular agent produces the synthetic repaint pattern.
- **Platform coverage:** mac+linux+windows.

##### mode/scroll/002 — Default and remapped keyboard scrolls share the cannot-scroll explanation.
- **Layer:** L1 (in-process production keybinding resolution, focused-pane scroll, vt100 depth, pane render, and child-input recording seams).
- **Agent:** none (agent-agnostic synthetic plain-stream and DECSTBM/cursor-repaint output).
- **Asserts:** PageUp/PageDown still move ordinary focused-pane history away from/toward live output without writing to the child; `[dashboard]` scroll remaps parse without warnings, retire their defaults, and move history on replacement chords; default PageUp, default PageDown, and remapped `scroll_pane_up` all arm the same notice when a mature zero-depth pane cannot move, still without child bytes; a retired default attempts no scroll and arms nothing.
- **Does not assert:** real terminal mouse-report decoding (covered by the next scanner delegation), help-overlay or bottom-bar discoverability, filesystem loading of `keybindings.toml`, real-agent behavior, or that any particular agent produces the synthetic repaint pattern.
- **Platform coverage:** mac+linux+windows.

##### mode/scroll/003 — PaneInput snaps newly targeted panes back to live output without disabling deliberate scrolling.
- **Layer:** L1 (in-process two-frame reconcile through two real synthetic vt100 panes; no PTY subprocess).
- **Agent:** none (two in-memory panes with synthetic history and production focus changes).
- **Asserts:** command-mode scrollback is nonzero before entering PaneInput and zero afterward; an unchanged PaneInput target deliberately retains its offset; moving PaneInput focus snaps only the newly targeted second pane while leaving the first at live output; an unchanged command-mode target deliberately retains its offset. Every case pins both pre- and post-reconcile offsets for both panes.
- **Does not assert:** hardware-cursor rendering after the reset (covered by `mode/live/002`); key dispatch for entering PaneInput; real-agent output.
- **Platform coverage:** mac+linux+windows.

##### mode/scroll/004 — PaneInput without a focused pane settles in command mode exactly once.
- **Layer:** L1 (in-process two-frame production scrollback reconcile plus command-banner edge observer with injected `Instant`s; no PTY or subprocess).
- **Agent:** none (a controller with no panes).
- **Asserts:** a no-focus PaneInput frame lands in Normal with an Expanded banner, remains Normal on the next frame, and reports Collapsed exactly at the TTL so the entry instant was not re-stamped; equal frame instants remain Expanded, and an already-Normal initial mode produces the identical idempotent result.
- **Does not assert:** how focus vanished, focus replacement policy when another pane exists, rendered banner geometry, or real-agent behavior.
- **Platform coverage:** mac+linux+windows.

##### mode/scroll/005 — The cannot-scroll notice selects a safe render tier and stays transient, pane-local, and non-consuming.
- **Layer:** L1 (in-process production wheel, boundary-sized one- and two-pane renders, single-line and rich block-tier command-banner overlays, production focus change, and full `handle_key_event` seams with injected `Instant`s and a recording child-input channel; no PTY subprocess).
- **Agent:** none (agent-agnostic mature DECSTBM/cursor-repainting panes, including both panes in the focus-affinity frame).
- **Asserts:** an inner pane exactly as wide as the production sentence renders the complete long tier, one column below it falls back to the centred short sentence, an inner pane exactly as wide as the short sentence still renders it without offering PageUp, and one column below that omits both tiers without modifying either border or any guard column beyond the pane — every boundary derived from the production constants' own lengths, never transcribed, so a reword cannot silently un-pin them; when the notice overlaps either the compact two-row single-line command banner or the roomy seven-row `BlockCommandModeWithSubtitle` tier, the intact notice renders exactly once as the frame's only reversed run, with no block-letter rows or `Ctrl+D to type` subtitle surviving anywhere, while the focused pane remains dimmed in command mode; after production focus moves from pane A to pane B, exactly one notice remains on A and none appears on B; the normal wide notice remains a single centred REVERSED line reporting only what was observed — this pane has no scrollback and there is nothing to scroll — without suggesting PageUp and without asserting the agent's rendering model; it clears exactly at the shared 2.5-second `COMMAND_BANNER_TTL`, refreshes without duplication through the original expiry, and clears at the refreshed expiry; the next unbound printable and bound Ctrl+D each dismiss it without losing normal child forwarding or mode transition.
- **Does not assert:** the intermediate five-row `BlockCommand` geometry or exact block-glyph shapes (the richest seven-row tier pins whole-banner suppression); real terminal mouse-report decoding (covered by the next scanner delegation); wall-clock scheduling in the live 16ms loop; horizontal multi-column pane geometry; real-agent behavior; or that any particular agent produces the synthetic repaint pattern.
- **Platform coverage:** mac+linux+windows.

#### mode/live

##### mode/live/001 — A real PTY-attached deck keeps the persistent mode chip after the command banner collapses.
- **Layer:** L2 PTY (the real `dot-agent-deck` binary in the isolated `TuiDeck` harness, asserted on the rendered vt100 grid and terminal attributes).
- **Agent:** none (synthetic `printf; sleep` stand-in pane).
- **Asserts:** Ctrl+D enters command mode with readable DIM pane content, the expanded banner, and the left-anchored ` COMMAND ` chip; the bound `j` action collapses the banner without removing the chip or content; Ctrl+D returns to a banner-free ` TYPING ` chip.
- **Does not assert:** a genuine agent boot or agent response; real-agent cursor and scroll behavior (covered by `mode/live/002`); exact block-glyph shapes or subtitle position.
- **Platform coverage:** mac+linux.

##### mode/live/002 — A real interactive Haiku agent visibly traverses typing, command-mode reading and scrollback, then typing again. [reel]
- **Layer:** L2 PTY (the real `dot-agent-deck` binary in the isolated `TuiDeck` harness, asserted on the rendered vt100 grid and terminal attributes; flaky-tolerant lane-2 real-agent tier).
- **Agent:** REAL interactive Claude Code on Haiku (`claude-haiku-4-5-20251001`, `--ax-screen-reader`, `--allowedTools Bash Read`, no `-p`), with isolated imported credentials plus onboarding/project trust seeded in the per-test HOME; the supported accessibility renderer keeps genuine interactive output in terminal scrollback instead of repainting it out of the vt100 history.
- **Asserts:** the live prompt accepts typed keystrokes and exposes both cursor channels with ` TYPING `; the submitted prefix-glob directive makes Haiku inspect and visibly list a uniquely named fixture sentinel; Ctrl+D hides the hardware cursor and removes the painted block while retaining readable DIM output, the expanded banner, and ` COMMAND `; wheel-up reveals older real-agent filename output through deck scrollback rather than the child mouse path; Ctrl+D restores the cursor treatment and ` TYPING `.
- **Does not assert:** exact model prose, tool-call wording, response timing, pixel-level DIM appearance, light-versus-dark terminal rendering, or command-mode indication on all three tab types (covered at L1 by the mode suites and manually validated across tabs).
- **Platform coverage:** mac+linux.

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

##### scheduler/spawn/006 — Scheduled single-agent and orchestration-role Codex commands both launch through the Wrapper strategy (PRD #20, blocker 3).
- **Layer:** L2 headless daemon `RunNow` with PATH recorder stubs.
- **Agent:** synthetic Codex recorders for one plain scheduled task and one scheduled orchestration start role.
- **Asserts:** both bare `codex` commands execute exactly as `dot-agent-deck wrap --agent codex -- codex`; neither path launches the bare recorder.
- **Does not assert:** issue-dispatch worktree creation or prompt delivery content.
- **Platform coverage:** mac+linux.

##### scheduler/spawn/007 — A scheduler-created single-agent pane accepts a real non-`SessionStart` lifecycle report and joins it to its daemon registry record.
- **Layer:** L2 headless real daemon (the production `RunNow → spawn_or_reuse → spawn` caller chain, the REAL `dot-agent-deck agent-event --type running` CLI, and `ListAgents`; no attached TUI or LLM).
- **Agent:** none (synthetic — a long-lived `cat` schedule gives the real CLI a stable generated pane id and daemon registry agent id).
- **Asserts:** `RunNow` creates a registry record through the scheduler's production spawn callback; the real non-`SessionStart` lifecycle CLI exits successfully using that record's pane and agent ids; and a later `ListAgents` returns the same record with a joined `Thinking` live snapshot rather than `live = None`.
- **Does not assert:** the TUI-rendered reconnect symptom (`session/live/012`); issue-dispatch worktree creation; prompt-delivery contents.
- **Platform coverage:** mac+linux.

##### scheduler/spawn/008 — A scheduled fire opens the orchestration a repo DECLARES as its default, past a roleless block in slot 0 and past the one that merely comes first (issue #704).
- **Layer:** L2 (headless `dot-agent-deck daemon serve` driven via `RunNow`, same shape as `scheduler/spawn/002`).
- **Agent:** none (every spawnable role runs `cat`, which echoes the delivered prompt).
- **Asserts:** a fire into a dir whose `.dot-agent-deck.toml` holds a ROLELESS block, then `first-real`, then `chosen` carrying `default = true`, registers an agent whose `TabMembership::Orchestration` names **`chosen`** — not a single-agent card, and not `first-real` — and the scheduled prompt is echoed by that orchestrator's PTY.
- **Why it exists:** the two paths that answer "which orchestration when none was named" disagreed. The bare `dispatch --orchestration=` form took the first ROLE-BEARING block; this one took `orchestrations.first()` and degraded to a single-agent card when that entry was roleless — so a scheduled `issue_dispatch` and a bare dispatch rooted at the same repo opened different things, and in the roleless case the scheduler opened nothing while `--list-targets` was still offering a target. Both halves are verified load-bearing: reverting `decide_target` to `orchestrations.first()` fails at the first assertion (a single-agent card, `tab_membership: None`), and reverting it to "first role-bearing" fails at the second (`first-real` instead of `chosen`).
- **Does not assert:** the diagnostic text emitted when the choice IS implicit (the `default_orchestration` unit tests own the wording); the dispatch side of the same rule (`orchestration/dispatch/004`); role layout beyond the orchestrator slot (`scheduler/spawn/002`).
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

##### scheduler/dispatch/004 — Issue-dispatch orchestration-role and single-agent Codex spawns both use the Wrapper strategy (PRD #120 M2.3, PRD #20 blocker 3).
- **Layer:** L2 (as `scheduler/dispatch/001`; two `issue_dispatch` tasks, one fixture remote with a committed Codex orchestration and one without; `default_command = codex`; PATH recorders execute `cat` after recording argv so prompt delivery remains observable).
- **Agent:** synthetic Codex recorders (run-now; observes `ListAgents` tab_membership + spawn cwd + PTY prompt echo + launch argv).
- **Asserts:** the orchestration clone spawns an `orchestrator`-role agent in its worktree and receives `ORCHDISP-11`; the plain clone spawns a non-orchestration card in its worktree and receives `PLAINDISP-22`; both launch records are exactly `dot-agent-deck wrap --agent codex -- codex`, never bare Codex.
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
- **Fixture:** the permanent public repo `vfarcic/dot-agent-deck-tests` — a committed `DISPATCH_E2E_SENTINEL.md`, a `.dot-agent-deck.toml` with `[[orchestrations]] name = "issue-work"` (roles `orchestrator` (start) + `worker`, both Haiku `claude`; the orchestrator's `prompt_template` delegates the task to the worker), and a PERMANENT open issue #1 labelled `agent-dispatch-test`. The schedule filters on that label with `max_per_run = 1`, so ONLY issue #1 is enumerated (deterministic). Both role panes share the per-issue worktree cwd (pre-trusted in the per-test HOME so claude's first-run gates clear with no keystroke). Clone + worktree live under a `common::harness_tempdir()` removed on drop.
- **Agent:** REAL Claude Code (Haiku) ×2 role panes, cheap interactive turns (<$0.05/run). Flaky-tolerant lane-2 tier (real LLM + real network) — run once, not looped (rule 4). Runtime-skipped (Decision 26) when the `claude` CLI/credentials or `GITHUB_TOKEN` are absent.
- **Asserts:** after the fire the daemon registers BOTH of the dispatched orchestration's role agents, each under its own ROLE NAME — `orchestrator` and `worker` (precondition — proves the live clone + worktree + spawn happened). Until `orchestration/dispatch/002` this looked for the shared schedule name `github-issues` on a role pane, which is what a dispatched role's `display_name` used to be; role panes now carry their role name (matching the interactive `Ctrl+n` path), and requiring both names is strictly stronger — one shared name could be satisfied by a single spawned pane. The dispatched ORCHESTRATION then surfaces LIVE as an orchestration TAB labelled `issue-work` (the fixture's `[[orchestrations]] name`) in the attached TUI's tab strip, with no reconnect/relaunch — RED today, because `spawn::spawn`'s orchestration branch does not call `surface_spawned_pane` and orchestration tabs are rebuilt only at hydration, so the role panes appear only as flat dashboard cards and no `issue-work` tab paints live. Best-effort (once GREEN, logged not gated): switching to the orchestration tab, the worker (delegated to by the orchestrator) lists the cloned repo's files including the committed sentinel `DISPATCH_E2E_SENTINEL.md`; and the fixture repo has no pushed `agent/issue-1` branch afterward (NO REMOTE WRITES).
- **Does not assert:** the delegation chain / sentinel as a hard gate (logged best-effort — too LLM/timing-dependent); exact agent phrasing; the clone/worktree/branch derivation or skip/dedup/cap/cleanup logic (covered by the headless `scheduler/dispatch/001-009` and the deterministic-stub `scheduler/dispatch/011-012`); the single-agent live-surfacing path (covered by `scheduler/dispatch/011`).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/014 — Concurrent single-agent dispatch seeds survive a deterministic boot-window swallow and are confirmed after retry, whether the producer identifies itself before or after the write.
- **Layer:** L2 synthetic PTY-attached (real deck and daemon, five real dispatch worktrees, scripted Claude-shaped stand-ins that post hooks through the real CLI; no LLM). The deck-selected executable is named `claude` so `AgentType::from_command` resolves its frozen spawn record as ClaudeCode — the ordinary `default_command = "claude …"` production shape. `DOT_AGENT_DECK_SESSION_START_WAIT_MS` pins the readiness gate to 3 s so the fallback write path is reached in seconds rather than the production 30 s.
- **Agent:** four one-write-swallowing hook stand-ins retain the existing controls, including `seed-late-claim`, which withholds its genuine start for 6 s to stage issue #570. The fifth pane (`seed-two-write-flush`) is a deterministic two-stage launcher: a `wrapper_fork` start declares standing, the launcher consumes attempts 1 and 2 while emitting only non-generational reporting evidence between them, then stage two posts a genuine Claude start and emits `UserPromptSubmit` for later non-empty input.
- **Asserts:** all five concurrent `dispatch --single` panes durably expose their exact submitted prompt and written/unconfirmed/confirmed lifecycle under distinct delivery IDs. The original four each retain their swallowed-first-write recovery contract; the fifth records exactly two `swallowed|<prompt>` lines followed by exactly one `confirmed|<prompt>`, requires a `prompt written to pane` line carrying `attempt=3` for its pane (rather than the state-set helper that erases attempt counts), and forbids any deadline `abandoning` line. RED before issue #666's implementation: attempts 3–8 are empty probes and the fifth pane abandons with `attempts=8`.
- **Does not assert:** the retry's internal state representation or real-agent boot behavior (covered by `scheduler/dispatch/015`); the sub-150 ms production window in which #570 was actually observed (the late claim is staged as strictly post-write instead); the refusal side for a pane the deck cannot vouch for (covered by `scheduler/dispatch/016`).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/015 — Three concurrent real interactive Claude dispatches each genuinely submit their seed prompt.
- **Layer:** L2 REAL PTY-attached (real deck and daemon, three sibling dispatch worktrees, imported isolated credentials, and project trust pre-seeded for every predicted worktree). A bootstrap launcher mirrors the field report's nested `devbox` startup seam: it announces an explicitly launcher-origin (`wrapper_fork`) `SessionStart`, consumes attempt 1, posts identified non-generational reporting evidence so the standing launcher remains eligible for the bounded replacement, consumes attempt 2 while the real agent is not yet running, then `exec`s Claude. `DOT_AGENT_DECK_SESSION_START_WAIT_MS` pins the readiness gate to 3 s, as `scheduler/dispatch/014` does, because this scenario cannot satisfy that gate before its first write.
- **Scheduling:** exclusive (issue #664). `.config/nextest.toml` gives it `threads-required = "num-test-threads"`, a high `priority` and one retry. Three real cold boots against a 60 s production deadline are unusually sensitive to what the rest of the machine is doing — measured green alone at 32.2 s and red in a full `cargo test-e2e` at 174.1 s, two panes `Error` with their deliveries abandoned. Reserving the pool reproduces the `-j 1` condition that was measured green; the priority makes that reservation free by taking it at run start rather than draining the tier's 250-320 s tail; the single retry covers load that is not the tier's at all (a full-pool run still failed with the 15-minute load average at 58 on 16 cores, from sibling agent worktrees building), and leaves a genuine regression failing both attempts and a recovered run reported as FLAKY.
- **Agent:** REAL interactive Claude Code ×3 pinned to `claude-haiku-4-5-20251001` with `--allowedTools Bash` and no `-p`, reached through the deterministic two-write-swallowing bootstrap launcher; runtime-skipped when the CLI or credentials are absent and flaky-tolerant in the lane-2 tier.
- **Asserts:** all three bootstrap launchers record exactly two swallowed copies of their distinct seed; after Claude's native start, each delivery must log a payload write at some attempt greater than 2, avoid deadline abandonment, and durably expose the exact sentinel-bearing seed through Claude's native `UserPromptSubmit`. RED before issue #666's implementation: all three emit only probes after attempt 2 and abandon at `attempts=8`. The isolated RED measurement left 56.20 s of the production deadline after attempt 2 in every pane, so the deterministic staging retains ample real-agent boot margin within the existing exclusive test.
- **Failure diagnostics:** every failing path reports, per pane, the full expected prompt, the exact durable confirmed value, whether the first submission was swallowed, and the complete bootstrap attempt log, plus the daemon's delivery-lifecycle log lines (written / re-submitting / confirmed / **abandoning** / stopped, each with its `delivery_id` and attempt count) and the final rendered grid. The abandonment line is what separates "the retry path regressed" from "a starved machine missed the 60 s window", which `confirmed_exact=None` alone could not (issue #664).
- **Does not assert:** exact model response phrasing, ordering between the three agents, a fixed boot duration, or which attempt carries the recovered payload because that depends on real-agent boot latency; the bootstrap's `PreToolUse` is synthetic scheduling evidence and not treated as readiness or as the genuine start that authorizes the recovered payload.
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/016 — Detached prompt retries stop on terminal targets/evidence and arm from a post-write producer claim only where the deck vouched for the pane first.
- **Layer:** L1 (in-process detached spawn confirmation task with real registry-owned platform-native shell/byte-observation PTYs and synthetic hook events).
- **Agent:** none (the real platform-native PTYs — `/bin/sh` and `/bin/cat` on Unix, `cmd.exe` and `more.com` on Windows — are observation targets, not agent stand-ins).
- **Asserts:** replacement, a bound `SessionEnd`, broadcast lag, and broadcast closure each terminally stop the watch without stale retry bytes; pane close and daemon shutdown cancel registered watches; a newer same-pane delivery aborts the older single flight before it retries; and issue #570's paired post-write capability claim is accepted only where the deck supplied pre-write standing. For issue #666, attempt 3 carries the prompt after a genuine Claude start when standing comes either from `SpawnOptions::agent_type = ClaudeCode` or solely from a pre-write `wrapper_fork` declaration on an unresolvable `/bin/cat`/`more.com` spawn; the otherwise-identical no-standing case and a Codex-declared start receive only the bare submit. Reciprocally, a pane whose trusted spawn record reads Codex or OpenCode receives only the bare submit when its post-write start declares ClaudeCode, proving an event declaration may withhold but cannot grant eligibility; attempt 4 is bare again, and a second genuine session id terminates the delivery without later bytes.
- **Does not assert:** TUI-owned automatic seed/orchestrator delivery or finer same-agent generation tracking without `SessionEnd` beyond the explicitly terminal replay case; the user-visible end of an armed payload — genuine submission and confirmation by the agent (covered by `scheduler/dispatch/014`). Nor that a Wrapper-strategy agent's real launch shape produces that trusted record: the Codex pane's record is stamped onto the registry after an ordinary `/bin/cat` spawn, because declaring the type at the spawn would put `dot-agent-deck wrap --agent codex --` between the PTY and the byte sink and this case measures raw bytes; the spawn-site plumbing that computes the record stays covered by the ClaudeCode and OpenCode cases, which declare it for real.
- **Platform coverage:** mac+linux+windows.

##### scheduler/dispatch/017 — Cap-exhaustion notices reach a hookless card that exists only in the attached TUI's broadcast state.
- **Layer:** L1 (in-process production delivery-notice sink, daemon/AppState split, attached-client broadcast consumer, and real registry-owned `/bin/cat` PTY).
- **Agent:** none.
- **Asserts:** a `surface_spawned_pane`-shaped `SessionStart` makes the card visible only in the attached client's state while daemon state stays empty; publishing the exact 257th-delivery cap notice through the production sink broadcasts an `Error` that visibly marks that existing client card.
- **Does not assert:** the cap counter's publication branch itself (the existing `abandonment_reports_state_and_never_writes_into_the_pane` unit test fills all 256 slots and proves that the 257th publishes this notice).
- **Platform coverage:** mac+linux.

##### scheduler/dispatch/018 — User typing after a detached automatic payload disarms the next retry.
- **Layer:** L1 (in-process detached spawn confirmation task with a real registry-owned byte-observation PTY: `/bin/cat` on Unix, `more.com` on Windows).
- **Agent:** none.
- **Asserts:** attempt 1 is applied and physically reaches the pane before any automatic-write timestamp exists; an unsent user draft after attempt 1 prevents attempt 2 from appending its replacement payload or submitting the draft, and independently a draft after attempt 2 prevents attempt 3's submit-only probe, each proven by an unchanged PTY byte snapshot.
- **Does not assert:** TUI-owned seed delivery (covered by `prompt/pane-input/032`) or the internal location of the clock comparison.
- **Platform coverage:** mac+linux+windows.

##### scheduler/dispatch/019 — Releasing an attached user's pane writer cannot expose unstamped input to an automatic retry.
- **Layer:** L1 (in-process production registry guard with a real byte-observation PTY — `/bin/cat` on Unix, `more.com` on Windows — and a deterministic writer-lock handoff).
- **Agent:** none.
- **Asserts:** while an attached input writer holds the pane lock, an automatic replacement is queued behind it; the user's unsent draft is physically present before the writer is released; the queued replacement then owns the exact write-to-clock handoff window and must be refused with no snapshot change before the test allows the user-input clock stamp to run.
- **Does not assert:** socket frame parsing or scheduler timing; the test directly forces the ordering produced inside the attach STREAM_IN handler after a successful write and flush.
- **Platform coverage:** mac+linux+windows.

##### scheduler/dispatch/020 — Automatic payload guards distinguish submitted turns from unsent drafts across delivery overlap, guarded writes, paste, and newline controls.
- **Layer:** L1 (in-process production guarded submits with real byte-observation PTYs: `/bin/cat` on Unix, `more.com` on Windows).
- **Agent:** none.
- **Asserts:** after delivery A and a completed user turn, a later delivery B's first attempt carrying the same fixed pointer text is applied and physically writes; user input invalidates delivery A even when a different guarded submit B intervenes; production-shaped bracketed paste, Ctrl+J, and Claude Alt+Enter frames leave drafts unsent and therefore do not let replacements append or submit bytes; a genuine plain Enter drains the completed turn and admits a later automatic payload; and when two active deliveries write the same payload, superseding A after B's write does not let B's retry append to or submit a later user draft.
- **Does not assert:** the internal representation of delivery identity, payload hashes, record lists, paste parsing strategy, or which guard rejects the unsafe writes; every safety assertion compares PTY bytes before and after the attempted retry.
- **Platform coverage:** mac+linux+windows.

##### scheduler/dispatch/021 — A detached writer-held user-input refusal is visibly reported.
- **Layer:** L1 (in-process detached confirmation loop with paused time, a held production pane writer, a real byte-observation PTY — `/bin/cat` on Unix, `more.com` on Windows — and the delivery-notice sink).
- **Agent:** none.
- **Asserts:** paused time deterministically completes the confirmation window while the writer is held, user input is stamped only after the caller's precheck has run and before the writer-held backstop proceeds, and that backstop refusal publishes one durable `DeliveryNotice` instead of becoming a log-only `target went stale` stop.
- **Does not assert:** the notice sink's daemon-to-TUI rendering (covered by `scheduler/dispatch/017`) or exact log wording.
- **Platform coverage:** mac+linux+windows.

#### scheduler/pi

##### scheduler/pi/001 — A SCHEDULED, UNATTENDED real `pi` job (no TUI client attached) boots and its bundled extension reports the Pi pane's status via `agent-event`, re-broadcast on the daemon's event stream (PRD #201 M4.2).
- **Layer:** L2 (real `daemon serve` via the `DaemonProc` harness — no PTY, no attached TUI). The schedule's `command` is a REAL `pi` (`--provider anthropic --model claude-haiku-4-5 --approve -p ready`, a cheap non-interactive turn); the bundled extension is materialized into the daemon's HOME (via `orchestrator_ext::materialize`) so the scheduler-spawned pi (which inherits that HOME) auto-discovers it. `ANTHROPIC_API_KEY` (never printed) + the freshly-built binary dir on PATH are propagated into the daemon via `spawn_daemon_serve_with_env` and inherited by the spawned pi. The fire is driven by `RunNow`; status is observed via an unattended `SubscribeEvents` consumer.
- **Agent:** REAL `pi` 0.80.6 (cheap Haiku `-p` turn, the cheapest tier in pi's Anthropic catalog — TEMPORARY: the pi tier is on Anthropic Haiku while the GPT accounts are without credit; the tier is provider-agnostic and the model is a one-line change). Flaky-tolerant lane-2 tier — run once, not looped. Runtime-skipped (Decision 26) when `pi`/`ANTHROPIC_API_KEY` are absent.
- **Asserts:** after `RunNow`, the scheduled pi boots and its real extension shells `dot-agent-deck agent-event`, which the daemon ingests and re-broadcasts as a `Pi`-typed `AgentEvent` in one of the extension's mapped states (`WaitingForInput`/`Thinking`/`Idle`) carrying the scheduler-injected pane id — proving a scheduled, unattended (no-client) real pi is status-tracked through the same `AgentEvent` contract every client consumes. The match EXCLUDES `SessionStart`: the scheduler's `surface_spawned_pane` broadcasts a synthetic `SessionStart` with the `from_command`-guessed `Pi` type the instant the pane spawns (before pi's runtime boots), so requiring a non-`SessionStart` state is what makes the pass attributable to the REAL extension rather than the daemon's spawn-time guess.
- **Does not assert:** the delegate/work-done chain (covered by `chain-smoke/pi/001`); the exact lifecycle→state mapping across running/waiting/finished (covered synthetically by `status/agent-event/003` and the TS unit tests); a dashboard-attached Pi pane (the synthetic dashboard render is `dashboard/pane/007`; the real-agent unattended path is the M4.2 value here).
- **Platform coverage:** mac+linux (real-agent tier is local-only per Decision 8).
- **Cost note:** one cheap Haiku `-p` turn (and the status assertion resolves on boot, before the turn completes) — well under Decision 23's <$0.05/run bound.

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

##### scheduler/manager/016 — Wheel input over the Scheduled Tasks dialog does not scroll a mode-tab side pane behind the modal (issue #142).
- **Layer:** L2 (real TUI in a PTY; opens a synthetic `scroll` mode tab whose persistent right-hand side pane is filled with deterministic scrollback, then sends precise SGR wheel reports over the overlapping manager dialog).
- **Agent:** none (the mode side pane runs a synthetic shell command; no LLM is invoked).
- **Asserts:** after the side pane is scrolled into history and the manager is opened over it, wheel-down must first move the manager selection from `alpha` to `bravo`, then wheel-up must move it back to `alpha`, while the exposed side-pane marker sequence remains unchanged; the modal consumes the wheel events instead of leaking them to the pane behind it.
- **Does not assert:** focused dashboard-pane wheel behavior; child-app mouse forwarding; the manager list viewport behavior (covered by `scheduler/manager/017`).
- **Platform coverage:** mac+linux.

##### scheduler/manager/017 — Wheel input over a windowed Scheduled Tasks list moves its selection and derived viewport (issue #142).
- **Layer:** L2 (real TUI in a constrained-height PTY; a fixture global `schedules.toml` contains 30 distinct tasks, more than the manager can render at once, and the first visible task row supplies the coordinate for precise SGR wheel reports over the list viewport).
- **Agent:** none (fixture global `schedules.toml` via `DOT_AGENT_DECK_SCHEDULES`).
- **Asserts:** the first row starts selected and `wheel-task-13` starts below the viewport; twelve wheel-down reports over the list move the `▶` marker to `wheel-task-13`, which drags the selection-derived viewport until that initially hidden row is visible.
- **Does not assert:** an independent list scroll offset (none exists); wheel-up wrapping at the first row; background side-pane isolation (covered by `scheduler/manager/016`).
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

#### scheduler/idle-worker

##### scheduler/idle-worker/001 — A delegated worker that never sends work-done produces a self-describing idle prompt in the orchestrator pane.
- **Layer:** fast integration (in-process daemon state + real PTY registry; `cat` stand-ins).
- **Agent:** none (synthetic `cat` panes; the orchestrator is raw/no-echo so one daemon submission appears once in the snapshot).
- **Asserts:** after the test-only millisecond timeout, the orchestrator PTY contains one line carrying both the daemon-provenance clause (`has not responded with work-done (dot-agent-deck daemon report, not a message from a person or an agent)`) and the target role wrapped in `[UNTRUSTED-ROLE-LABEL: … :END-UNTRUSTED-ROLE-LABEL]`.
- **Does not assert:** emoji, elapsed-time wording, or notification-channel behavior.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/002 — Work-done arriving before the timeout cancels that delegation's idle prompt.
- **Layer:** fast integration (real `handle_delegate` + `handle_work_done`).
- **Agent:** none (`cat` stand-ins).
- **Asserts:** a parallel silent control delegation proves the detector fires, while the responsive worker's role never appears on an idle-prompt line after its work-done and timeout window.
- **Does not assert:** work-done summary-file contents or the completion-feedback wording.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/003 — A zero worker-response timeout DISABLES the detector — from the config key and from the millisecond seam alike — rather than firing immediately (PRD #126 M1 audit finding 4).
- **Layer:** fast integration (three delegations against one harness whose project config sets `worker_response_timeout_minutes = 0`, re-pointing the millisecond seam between them).
- **Agent:** none (`cat` stand-ins).
- **Asserts:** with the seam at a positive value the detector fires (positive control, and proof the seam overrides a config that would have disabled it); re-pointing the same harness's seam to `0` produces no prompt; unsetting the seam so the config's own `0` is consulted produces no prompt either; exactly one prompt exists at the end.
- **Does not assert:** that a *file* `0` is decisive against a file positive value — no config value below one minute exists, so that comparison is unobservable behaviorally and is covered at resolution level by `scheduler/idle-worker/007`.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/004 — An outstanding delegation produces only one idle prompt and never re-nags.
- **Layer:** fast integration (in-process daemon state + raw/no-echo orchestrator PTY).
- **Agent:** none (`cat` stand-ins).
- **Asserts:** the first idle prompt appears, then the ASCII idle needle still occurs exactly once after another timeout window.
- **Does not assert:** behavior after a later re-delegation (covered by `scheduler/idle-worker/005`).
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/005 — Re-delegating to the same worker pane replaces the first timer without a premature or duplicate prompt.
- **Layer:** fast integration (real repeated `handle_delegate` calls against one worker pane).
- **Agent:** none (`cat` stand-ins).
- **Asserts:** no prompt appears after delegation one's old deadline but before delegation two's deadline; delegation two then produces exactly one role-bearing idle prompt.
- **Does not assert:** concurrent delegation to different worker panes.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/006 — Closing a delegated worker through StopAgent cancels its outstanding idle timer.
- **Layer:** fast integration with an in-process attach server and the real StopAgent request.
- **Agent:** none (`cat` stand-ins).
- **Asserts:** a silent control worker proves the detector fires, while the stopped worker never appears on an idle-prompt line after the timeout.
- **Does not assert:** worktree cleanup or TUI close-key behavior.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/007 — The worker-response timeout resolves env-over-file-over-default, prefers the orchestration cwd, defaults to 120 minutes, and REJECTS an out-of-range value in favour of the default instead of clamping it.
- **Layer:** fast unit-level (calls the real `worker_response_timeout` resolver directly against purpose-built config directories; no PTY).
- **Agent:** none.
- **Asserts:** an absent key (and a cwd with no config file at all) resolves to 120 minutes; the orchestration cwd's value wins over the worker cwd's and the worker cwd is the fallback when the orchestration cwd has no config; a `20000`-minute file value resolves to the 120-minute DEFAULT, not to the 10080-minute ceiling; an in-range millisecond seam overrides the file; a below-floor (`50`) and an above-ceiling (`604800001`) seam value are both ignored so resolution continues to the file/default rather than clamping; `0` from either source resolves to `None` (detector disabled); the `1`-minute and `10080`-minute bounds themselves are honored.
- **Does not assert:** the delegate-time behavior of a disabled detector (covered by `scheduler/idle-worker/003`); non-integer or negative TOML values (rejected earlier, at parse time).
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/008 — After the ORCHESTRATOR pane closes, an unrelated agent that inherits its pane id receives nothing — the dead orchestration's idle prompt is never auto-submitted into a stranger's session (PRD #126 M1 review finding 1 / audit finding 2).
- **Layer:** fast integration with an in-process attach server, the real StopAgent request, and a second raw/no-echo `cat` spawned onto the freed `pane_id_env`.
- **Agent:** none (`cat` stand-ins; the successor is raw/no-echo so any submitted byte is directly observable in its scrollback).
- **Asserts:** the successor's own readiness marker is present (so absence of anything else is meaningful) while its PTY carries zero occurrences of the daemon clause and no fragment of the dead orchestration's role name, after two full timeout windows during which the successor owned the pane.
- **Does not assert:** which of the two layered guards refused — the record sweep over orchestrator-side records at `begin_pane_close`, or the `write_and_submit_guarded` agent-id gate. Both must be removed before a stray submit appears on THIS (StopAgent) path, because the sweep drops the record before any timer can wake; the identity gate on its own is isolated by `scheduler/idle-worker/014`, which reaches the same pane-reuse state through an orchestrator exit that runs no sweep at all.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/009 — A timer whose deadline falls inside a pane's SIGTERM grace window does not fire the nudge that the deliberate close exists to suppress (PRD #126 M1 review finding 1).
- **Layer:** fast integration with an in-process attach server and the real StopAgent request against a worker that IGNORES SIGTERM, so `close_agent` spends its full three-second grace with the pane marked closing.
- **Agent:** none (`cat` for the control; `trap '' TERM; exec cat` under a pinned `/bin/sh` for the TERM-resistant worker).
- **Asserts:** first, as a precondition, that the close window genuinely bracketed the detector deadline (close started before it and finished after it), so the test cannot pass for the wrong reason; then that a parallel silent control produced a prompt while the closing worker produced none.
- **Does not assert:** SIGKILL escalation timing, or the close outcome for a worker that exits promptly on SIGTERM (covered by `scheduler/idle-worker/006`).
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/010 — A delegate that lands while a pane is mid-close is refused arming, so the close cannot be raced into leaving a record behind it (PRD #126 M1 review finding 1).
- **Layer:** fast integration; a SIGTERM-ignoring worker holds the close transition open for three seconds and the test barriers on `is_pane_closing` before delegating, then re-asserts the mark is still set after the delegate returns.
- **Agent:** none (`cat` for the control; `trap '' TERM; exec cat` for the closing worker).
- **Asserts:** the delegate provably landed inside the close transition, and after the timeout the control has a prompt while the closing worker has none.
- **Does not assert:** the registry-level `arm_outstanding_delegation` → `None` contract in isolation (covered by the in-`src` unit test `begin_pane_close_cancels_records_targeting_the_closing_orchestrator`).
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/011 — A silent delegated worker's idle prompt is visible in a PTY-attached orchestration pane.
- **Layer:** L2 PTY (real `dot-agent-deck` binary and lazy daemon, rendered through the vt100 `TuiDeck` harness).
- **Agent:** none (the `orch-deck` fixture uses live `cat` stand-ins; synthetic Delegate injected over the real hook socket, so this entry is intentionally not reel-marked).
- **Asserts:** after opening the two-role orchestration with a tiny daemon timeout, the rendered surface visibly carries the daemon-provenance clause AND the worker role wrapped in `[UNTRUSTED-ROLE-LABEL: … :END-UNTRUSTED-ROLE-LABEL]`, matched wrap-tolerantly (whitespace squeezed from grid and needle alike) because the prompt is one long line broken across rows at the pane's wrap column.
- **Does not assert:** real-LLM reaction, notification delivery, emoji, or exact elapsed-time wording.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/012 — A real interactive Haiku orchestrator delegates to a silent worker and visibly receives the daemon's idle nudge. [reel]
- **Layer:** L2 PTY (real `dot-agent-deck` binary and lazy daemon, with the restored orchestration rendered through the vt100 `TuiDeck` harness). Flaky-tolerant lane-2 tier; run once, not looped.
- **Agent:** REAL interactive Claude Code orchestrator pinned to Haiku (`claude-haiku-4-5-20251001`, `--allowedTools Bash`, no `-p`) plus a long-lived `cat` worker that intentionally never sends work-done. Runtime-skipped when the Claude CLI or credentials are unavailable — set `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` to turn that skip into a hard failure on a run that must genuinely exercise the agent.
- **Asserts:** the real orchestrator follows a directive to run the genuine `dot-agent-deck delegate` CLI at least once (proved by the daemon-created `worker-task-worker.md`), then the daemon-authored nudge appears visibly on the attached orchestration grid after the test-only timeout, carrying BOTH the self-identifying report clause (`… (dot-agent-deck daemon report, not a message from a person or an agent)`) and the worker role wrapped in `[UNTRUSTED-ROLE-LABEL: … :END-UNTRUSTED-ROLE-LABEL]` — two anchors a narrating model has no reason to emit verbatim, unlike the bare `has not responded` this used to match.
- **Does not assert:** that the orchestrator delegated EXACTLY once. The daemon overwrites `worker-task-worker.md` on every delegate and nothing counts invocations, so the file's existence proves "at least one delegate reached the daemon" and no more. Also not asserted: the model's exact acknowledgement, notification-channel delivery, emoji, or exact elapsed-time wording.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/013 — A late work-done from a superseded delegation retires THAT delegation, leaving the re-delegated worker's own watch armed and still able to fire — while a second completion does retire what the first left armed (PRD #126 M1 review finding 6).
- **Layer:** fast integration (two `handle_delegate` calls against each of two worker panes on one clock, then real `handle_work_done` calls — one for the reported worker, two for the control).
- **Agent:** none (`cat` stand-ins).
- **Asserts:** after the late completion, delegation two's idle prompt still appears; it appears on delegation TWO's clock (no earlier than its own deadline, not the older delegation's); the second worker — twice delegated and twice completed — produces NO prompt, which is what distinguishes a real oldest-first retirement from a `work-done` that retired nothing at all (the surviving watch alone cannot tell them apart); and exactly one prompt exists across all four delegations.
- **Does not assert:** the two accepted residuals recorded in the PRD — an out-of-order completion crediting the wrong delegation, and a consumed-then-re-delegated record being retired by a late completion. Both are documented limitations, not fixed behavior. Also not asserted: the `DelegationRetirement` variant returned to `handle_work_done` (observed only through the resulting prompt/no-prompt behavior).
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/014 — After the orchestrator's process ends ON ITS OWN — no StopAgent, so no close transition and no record sweep — an unrelated agent inheriting its pane id still receives nothing; the `write_and_submit_guarded` agent-id gate is the only guard in play (PRD #126 M1 audit finding 2).
- **Layer:** fast integration; the orchestrator stub is a polling shell that exits when the test drops a flag file in its cwd (a genuine process exit, not a signalled close), after which a raw/no-echo `cat` takes the freed `pane_id_env`. No attach server, so no `StopAgent` exists in this test at all.
- **Agent:** none (`cat` worker stand-in; the successor is raw/no-echo so any submitted byte is directly observable in its scrollback).
- **Asserts:** two preconditions that stop it passing for the wrong reason — the orchestrator pane is NOT in a close transition after the exit (so the close-time sweep is provably not what suppresses the prompt), and the successor owned the pane before the delegation's deadline (so a stray timer had a live target to mis-deliver to) — then that after two further timeout windows the successor's PTY carries its own readiness marker, zero occurrences of the daemon clause, and no fragment of the dead orchestration's role name.
- **Does not assert:** the pane-reuse-after-`StopAgent` path (covered by `scheduler/idle-worker/008`); the orchestration-membership half of the delivery revalidation (the successor is spawned without `tab_membership`, so that check legitimately abstains and the agent-id gate is what refuses).
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/015 — A deferred daemon notice cannot launder user input into a later blind submit probe.
- **Layer:** L1 (in-process production notice composition and guarded notice/submit paths with a real `/bin/cat` byte-observation PTY).
- **Agent:** none.
- **Asserts:** an automatic payload lands, the user types an unsent draft, and the production `compose_worker_exited_notice` text is then delivered through the same `write_notice_guarded` call the daemon's worker-exit sweep makes; a following submit-only probe is refused and leaves the draft-plus-notice snapshot unchanged rather than submitting it. Keyed on the DELIVERY MECHANISM rather than on which notice it is: this pins the whole `write_notice_guarded` family, whose two remaining members are the worker-exited and respawn-no-live-worker notices. Issue #702 moved the delegate silence notice out of that family onto the submitted path, where the question does not arise.
- **Does not assert:** the broader idle-worker detection policy or the exact diagnostic prose, only that a production deferred-notice caller cannot reauthorize a blind probe; the trigger that decides to send the notice (`pump_reader`'s EOF sweep), which is stubbed out here; and the SUBMITTED family's own separate limitation, where a pane holding an unsent human draft has that draft submitted along with the daemon text (issue #544).
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/016 — A delegated worker whose PROCESS exits on its own — no `work-done`, no SIGTERM, no `StopAgent` — has its armed `OutstandingDelegation`/`SilenceWatchRecord` retired immediately by `pump_reader`'s EOF branch, and the orchestrator sees the new "exited without work-done" notice promptly instead of either older timeout-based notice.
- **Layer:** fast integration; the worker stub is a raw-mode shell that prints a readiness marker and exits on its own via a fixed short sleep — a genuine process exit, not a signalled close. Both timeout watches are configured to windows (60s / 30s) far longer than the test's own runtime, so a notice landing within the test's few-second bound can only have come from the EOF-triggered sweep.
- **Agent:** none (worker stand-in; the orchestrator is a raw/no-echo `cat` so the notice is directly observable in its scrollback).
- **Asserts:** after the delegate lands and the worker's process ends on its own, the worker pane is confirmed NOT in a close transition (so the close-time sweep is provably not what retired the records), the new EOF-triggered notice appears in the orchestrator's pane within 5s and names the exited worker's pane id, and neither the older idle-timeout prompt nor the older delegate-possibly-not-delivered silence notice appears at all.
- **Does not assert:** the exact daemon log wording; the identity-bound worker-side match's own race-closing behavior (a delegation whose worker identity has not yet resolved falling through to its own timer instead of being mistaken for a stranger's exit) — that is a `src/agent_pty.rs` unit-test concern, not this integration harness's.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/017 — A one-shot silence report releases its payload record, so a byte-identical SECOND report into the same orchestrator is still submitted rather than refused as a repeat of the user's draft.
- **Layer:** L1 (in-process production notice composition and the guarded submit path against two real `/bin/cat` PTY panes).
- **Agent:** none.
- **Asserts:** the production `compose_delegate_silence_notice` text is submitted into an orchestrator pane, the daemon's `note_payload_settled` release is made exactly where `arm_delegate_silence_watch` makes it, the user then types into that pane, and a byte-identical second report is still `Applied`. A second pane runs the identical sequence WITHOUT the release as a control and is refused (`Stale`), which is what makes the release load-bearing rather than decorative — the repeat is ordinary rather than exotic, since two silent workers on one orchestration whose panes rendered nothing compose the same bytes.
- **Does not assert:** the trigger that decides to send the report (`arm_delegate_silence_watch`'s window, covered by `orchestration/delegate/013` and `/025`); the DEFERRED family's own laundering question, which is `scheduler/idle-worker/015`'s and lives on `write_notice_guarded`; and the identity/liveness gates on the send, which `scheduler/idle-worker/008` and `/014` pin.
- **Platform coverage:** mac+linux.

##### scheduler/idle-worker/018 — An AMBIGUOUS silence report KEEPS its payload record, so a byte-identical second report into the same orchestrator is refused rather than submitted on top of the leftover bytes.
- **Layer:** L1 (in-process production notice composition, the guarded submit path against two real `/bin/cat` PTY panes, and the production settle decision `settle_silence_report_payload_record` driven with each outcome that leaves a record).
- **Agent:** none.
- **Asserts:** after the production `compose_delegate_silence_notice` text is submitted into an orchestrator pane and the settle decision is run with `Ambiguous`, the payload record survives, so once the user types a byte-identical second report is refused (`Stale`) instead of appending the leftover report bytes to the user's unsent draft and submitting both as one turn. A second pane runs the identical sequence with `Applied` as a control and IS admitted, which is what makes the refusal a property of the outcome rather than of the harness. The complement of `scheduler/idle-worker/017`, which pins the `Applied` release itself.
- **Does not assert:** that a real `Ambiguous` arises from this seam — a `/bin/cat` PTY writer cannot be faulted into a partial write, and the classification itself is unit-tested against a fault-injecting writer in `agent_pty` (`deliver_payload_classifies_partial_write_as_ambiguous`); the registry state is identical either way, since both classification arms call `note_automatic_write` with the same payload, so only the outcome the daemon acts on varies here. Also not asserted: the trigger that decides to send the report (`orchestration/delegate/013` and `/025`); the sibling one-shot callers (`arm_idle_worker_watch`'s idle prompt and the delegate task pointer), which still release on `Ambiguous`.
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

##### features/gating/004 — The `[features]` table is found by walking up when the deck is launched from a subdirectory of its project (issue #577).
- **Layer:** L2 (real TUI driven via PTY; observed on the rendered vt100 grid). No env var is used — the flag comes from the fixture's project-root `.dot-agent-deck.toml`, which is the only path issue #577 concerns. The deck's cwd is moved below the fixture root with `with_launch_subdir("nested/deep")`.
- **Agent:** none (`features-experimental-on` fixture; empty dashboard).
- **Asserts:** launched two levels below the project root, the deck resolves the project's `[features] experimental = true` and the rendered grid shows the `experimental: on` footer once the dashboard is up; a control launch at the project root shows the same footer, so the first result is attributable to the launch directory rather than to the fixture or the footer.
- **Does not assert:** the env-override path (`features/gating/003`); the ownership trust check on a candidate config, or the nearest-ancestor-wins ordering (both unit-covered in `tests/features.rs`); behaviour when the deck is launched entirely OUTSIDE any project — that remains the launch directory's own file, and is the residual of issue #577 the walk does not address.
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
| `dot-agent-deck connect <remote>` end-to-end SSH flow ([docs/remote-environments.md](../docs/remote-environments.md), [docs/remote-recipes.md](../docs/remote-recipes.md)) | Requires a remote-harness shape that does not exist yet. Catalogued at M4+ when remote testing lands. Local quit-dialog coverage (`prompt/quit/001`–`005`) already pins the Detach / Stop / Cancel behavior; remote attach adds only the daemon-side log distinction. |
| `dot-agent-deck remote add / list / upgrade / remove` ([docs/remote-environments.md](../docs/remote-environments.md)) | Same — remote-harness territory; the lib already covers the pure-data slices (URL parsing, command construction, error classification) in the kept tests. **Security properties deferred to M4+ end-to-end coverage:** shell-metacharacter quoting on remote-CLI argv assembly (unit-covered by `system_ssh_executor_quotes_arguments_safely`), `remotes.toml` written at mode 0o600 (covered by the now-moved `remotes_toml_written_at_0o600` test — restore at M4+), `DOT_AGENT_DECK_VIA_DAEMON=1` propagation on the remote shell (unit-covered by `build_connect_command_has_t_flag_and_via_daemon_env`). `remote doctor` is the exception: `remote/doctor/001`–`011` cover it through the deterministic PATH-stub `ssh` seam, without a real remote harness. |
| Container-based `remote doctor` validation via [`scripts/reverse-tunnel-validation.sh`](../scripts/reverse-tunnel-validation.sh) (PRD #345 M5) | Deliberately remains manual: it needs a container runtime plus privileged sshd configuration mutation, a harness shape with no e2e-tier precedent. The deterministic PATH-stub coverage in `remote/doctor/001`–`011` exercises the command's observation and classification outcomes; the script remains the documented real-sshd manual validation path. |
| `dot-agent-deck validate` CLI subcommand ([docs/workspace-modes.md#config-validation](../docs/workspace-modes.md)) | Non-TUI; the underlying validator is exhaustively covered by the pure-data `config_validation` tests. |
| `dot-agent-deck watch` CLI subcommand ([docs/workspace-modes.md#dot-agent-deck-watch](../docs/workspace-modes.md)) | Non-TUI subcommand; an L2 test would only exercise its output formatting against a real shell — low value compared to the deck-rendering surface. |
| `dot-agent-deck config get` / `config set` ([docs/configuration.md](../docs/configuration.md)) | Non-TUI; the underlying config field reflection is covered by pure-data tests (`*_get_set_field`, `*_get_set_fields`). |
| `dot-agent-deck hooks install` / `uninstall` CLI commands ([docs/troubleshooting.md#hooks](../docs/troubleshooting.md)) | Auto-install path is catalogued as `hooks/install/001`–`003`; the explicit subcommand variants share the same install/uninstall code. A targeted L2 test will be added only if a divergence appears. |
| Ghostty-specific Shift+Enter terminal config ([docs/troubleshooting.md#shiftenter-submits-instead-of-inserting-a-newline](../docs/troubleshooting.md)) | **No longer a skip** — PRD #227 showed the break was deck-side (`keyevent_to_bytes` collapsed `Enter + SHIFT` to a bare CR), so there IS a deck-side surface: it is now covered by `embed/key-forwarding/001`. Only the outer-terminal *configuration* itself (what a user types into `ghostty/config`) remains untestable here. |
| Mode-tab card jump via `Enter` (broken per docs note → [#68](https://github.com/vfarcic/dot-agent-deck/issues/68)) | Documented as broken. The catalog will gain an entry once the bug is closed; until then leaving it uncovered avoids pinning the broken behavior. |
| `--continue` "dashboard-first landing" detail ([docs/session-management.md#resuming-sessions](../docs/session-management.md)) | Implicit consequence of `session/restore/001`; not separately worth a catalog ID. Reconsider if the landing-tab logic ever has its own surface. |
