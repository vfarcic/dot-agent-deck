# PRD #385: Copy text out of any pane, and have it actually land on the clipboard

**Status**: Not started
**Priority**: High
**Created**: 2026-08-05

> **Update (#1199):** workspace modes were removed in [#1199](https://github.com/vfarcic/dot-agent-deck/issues/1199), so the mode tabs this PRD mentions no longer exist. Where it says "dashboard and mode tabs", read the dashboard; where it says a non-focused pane needs "a mode tab or a tiled dashboard" to reproduce, a tiled dashboard is now the case.

## Problem Statement

Three separate users have reported "copy is broken" — [#315](https://github.com/vfarcic/dot-agent-deck/issues/315) (macOS/iTerm2), [#96](https://github.com/vfarcic/dot-agent-deck/issues/96) (GNOME Terminal, closed with a tmux workaround), [#98](https://github.com/vfarcic/dot-agent-deck/issues/98) (the workaround itself), plus a Konsole report inside #96. They are not three bugs. They are one feature with four independent defects, none of which has ever been documented or tested.

**The copy is delivered only by OSC 52.** `copy_to_clipboard_osc52` (`src/ui.rs:4060`) writes `\x1b]52;c;…` to the tty and nothing else; there is no native clipboard path anywhere in the tree. That sequence is silently discarded by Terminal.app, by GNOME Terminal and every other VTE terminal (never implemented — the [VTE feature request](https://gitlab.gnome.org/GNOME/vte/-/issues/2495) is still open), by Konsole, and by iTerm2 unless the user has enabled clipboard access for terminal applications. Meanwhile `src/ui.rs:11178-11182` reports `"Copied to clipboard"` the instant the bytes are written, with no knowledge of whether anything received them. The message is confidently wrong, which is exactly why #315's reporter concluded the text was "copied to memory" but unpastable, and why #96 stayed open long enough to accumulate a second reporter.

**Selection is routed by focus, never by pointer.** The `Down` handler hit-tests only `ui.focused_pane_rect` (`src/ui.rs:11023-11031`), so a drag anywhere else starts no selection at all. On the dashboard tab there is no pane hit-testing whatsoever: `ui.side_pane_rects` is populated only by `render_mode_tab` (`src/ui.rs:13127`), and click-to-focus is gated on both `Tab::Mode` (`src/ui.rs:10927`, `10945`) and `UiMode::Normal` (`src/ui.rs:10912`). Even in a mode tab, the click that focuses a side pane cannot also begin the drag, because `ui.focused_pane_rect` still holds the *previous* frame's value — **the first gesture is always lost**. This is the literal complaint in #315's title, and it is the same root cause as [#362](https://github.com/vfarcic/dot-agent-deck/issues/362), where the wheel is routed by focus and forwards fabricated coordinates to the child.

**A plain drag erases its own evidence.** `src/ui.rs:11186-11188` clears the selection on release for anything that is not a multi-click, so nothing on screen shows what was copied — indistinguishable from a selection that never started.

**Nobody could have found the gesture.** There is no copy/paste section anywhere in `docs/`. The `?` overlay lists `Mouse click → Focus pane` and `Ctrl+click → Open hyperlink` (`src/ui.rs:14542-14543`) and stops. `tests/CATALOG.md` has no entry touching in-pane selection or copy, and `TuiDeck` has `click` and `scroll` but no `drag` (`tests/common/mod.rs:1363-1386`). A feature this undiscoverable generates bug reports instead of usage.

**What is explicitly not the answer:** [#197](https://github.com/vfarcic/dot-agent-deck/issues/197) proposed a `mouse.enabled` toggle so the terminal handles selection natively. #315's reporter already has that behaviour via iTerm2's ⌥-drag, and their complaint is that it "crosses over both the left and right pane" — native terminal selection is row-based across the full terminal width and *structurally cannot* respect a pane column. #197 was closed by its own reporter, and shipping it as the fix for this would make the multi-pane case worse.

## Solution Overview

Make copy work the way a user already assumes it works: select in whatever pane the pointer is over, see what you selected, and find it on your clipboard.

Four independent changes, in descending order of how much user pain they remove: deliver to the **native system clipboard** with OSC 52 kept as the remote/ssh fallback; **route pane mouse events by pointer position** rather than by focus; **keep the selection visible** after the gesture that made it; and **document and test** a feature that currently has neither. On top of that, a **keyboard copy mode**, because on VTE terminals and in ssh sessions no mouse path can ever be made to work, and because "select text without a mouse" is table stakes for an app shaped like a terminal multiplexer.

Every milestone below is independently shippable and independently valuable. M1 alone resolves the reported symptom for the largest group of affected users; M2 alone resolves the issue's literal title.

## Scope

### In Scope

- Native clipboard delivery, with OSC 52 retained as the fallback for remote sessions.
- A truthful status message about what was actually delivered.
- Pointer-based routing for pane mouse events — selection and wheel — on both dashboard and mode tabs, which closes #362.
- Pane-scoped selection state, so the selection, the extraction and the highlight all agree on which pane they belong to.
- A selection that stays visible after the gesture that created it.
- A keyboard copy mode, shipping visible by default (CLAUDE.md rule 9 question asked and answered: **no** experimental flag).
- Removing the unused any-motion mouse tracking mode.
- Docs, `?` overlay coverage, and the L1/L2 tests this feature has never had.

### Out of Scope

- **`mouse.enabled` / opting out of mouse capture (#197).** Deliberately excluded, see Problem Statement. It remains a defensible feature for people who want their terminal's native behaviour for reasons unrelated to this PRD, and it stays closed until someone asks again.
- **Reading the clipboard.** Paste into a pane already works via bracketed paste (`src/ui.rs:11200-11218`). The deck has no clipboard-read and this PRD adds none — see `docs/remote-environments.md` on why that is the right answer for remote sessions.
- **Selecting non-pane chrome** (cards, borders, the button bar). Only native terminal selection could ever grab those, which is #197's territory.
- **Image or file paste.** Covered by the closed #200 and unchanged here.
- **Any daemon, protocol, orchestration or hook change.** This is entirely TUI-local, so CLAUDE.md rule 12's cross-version check does not apply: no `PROTOCOL_VERSION` bump, no `.breaking.md` fragment, patch bump.

## Technical Approach

### Clipboard delivery (M1)

Attempt **both** paths on every copy, unconditionally, and let the environment decide which one lands.

The reasoning is that the two paths are never both meaningful at once, and never conflict when they are. Running locally, both target the same clipboard with the same text — harmless. Running over ssh, the native write either fails outright or sets the *remote* machine's clipboard where nobody will look, while OSC 52 travels back to the terminal that is actually in front of the user. This deliberately avoids needing to detect whether we are remote: `SSH_TTY` / `SSH_CONNECTION` are unreliable (a `tmux` session outlives the ssh connection that started it) and are currently read nowhere in the tree. "Do both" needs no detection to be correct.

Preferred implementation is `arboard` with `default-features = false` — the default features pull in `image` for image-data support, which is dead weight here. Three things to verify **before** committing to the dependency, because they touch the release path:

- That it builds for every release target, including the Windows build (#164). Its X11 backend is `x11rb` (pure Rust) and Wayland support is an optional `wayland-data-control` feature, so it should add no C build dependency — confirm rather than assume.
- That the `Clipboard` instance is held for the **process lifetime** (a `OnceLock`/`LazyLock`), not created per copy. On X11 and Wayland the clipboard content is owned by a live process; dropping the handle after `set_text` discards what was just copied. Content may still be lost when the deck exits unless a clipboard manager is running — acceptable, and strictly better than the current "lost immediately, everywhere".
- That `Clipboard::new()` failing in a headless environment degrades to OSC-52-only without panicking, so CI and the e2e suite are unaffected.

The considered alternative is shelling out to `pbcopy` / `wl-copy` / `xclip` / `clip.exe`: zero new dependencies, and it sidesteps the ownership problem entirely because `wl-copy` and `xclip` daemonise themselves. It loses on Linux, where none of those binaries is guaranteed present — which is precisely the platform #96 came from. Keep it as the escape hatch if the dependency proves awkward for the release builds.

The status message then reports what actually happened rather than what was attempted. Wording needs care: the current message is confident and wrong, and a message that is honest but alarming ("may not have copied") is its own kind of regression. Distinguishing a native write that succeeded from an OSC 52 write that was merely emitted is the minimum.

### Pointer-routed pane mouse events (M2)

`TextSelection` (`src/ui.rs:1805-1816`) carries a `pane_rect` but no pane id, which is the root of the whole family of bugs: `Down` resolves geometry from `ui.focused_pane_rect`, `Up` extracts text from `embedded.focused_pane_id()`, and the highlight is painted over `focused_pane_rect` regardless of where the selection was made (`src/ui.rs:12994`). Three sites, three different implicit answers to "which pane is this selection in", all of which happen to agree only while focus never changes.

Adding `pane_id` to `TextSelection` makes the selection pane-scoped end to end and lets the compiler find the sites. The geometry the hit test needs already exists and is simply discarded: `cards_pane_rects` (`src/ui.rs:11675`) computes id→rect for the dashboard's pane column during the `FrameLayout` pass, exactly as `side_pane_rects` does for mode tabs — it just is not kept on `UiState`. Storing it makes the dashboard hit test mechanical.

A click in a non-focused pane should then focus that pane **and** start the selection in the same gesture, which is what removes the always-lost first drag.

Note that `PaneLayout::Stacked` draws only the focused pane (`src/ui.rs:12924`, PRD #311), so pointer routing changes observable behaviour only where more than one pane is visible: tiled dashboards and mode tabs.

For the wheel half (#362), that issue asks for two decisions to be made deliberately rather than by accident, and they are separable. The forwarding fix is not really optional — `pane_relative_coords` saturating-subtracts, so a wheel event with the pointer outside the pane hands the child a plausible-looking position that is not where the pointer is, and sending fabricated input to an agent is hard to defend under any targeting model. The targeting question proper is left as an Open Question below.

Because this alters `PaneInput` behaviour that predates PRD #341, it wants its own seam coverage; `mode/scroll/001` already pins the mode × mouse-reporting matrix and is the natural place to extend.

### Selection that survives its own gesture (M3)

Multi-click selections already persist; drag selections do not (`src/ui.rs:11186-11188`). Making drag consistent is small, and it is what turns "did that work?" into "yes, that". What needs deciding is what *clears* it — see Open Questions. PRD #113 (clear the deck selection on tab switch) is the precedent for treating this as a real question rather than a default.

### Keyboard copy mode (M5)

A tmux-style mode: a key enters it, movement keys move a cursor, a key starts the selection, a key yanks and exits. No mouse involvement at any point, which makes it the only mechanism that can work on a VTE terminal, and the answer for anyone who does not want to reach for the mouse.

It is sequenced after M1 on purpose: a copy mode built on a delivery path that does not deliver inherits the entire bug.

Scrollback-aware extraction already exists — `extract_selection_text` takes a row offset (`src/ui.rs:4004`) and the mouse path passes `screen_row_offset` (`src/ui.rs:11175`) — so copy mode should be able to reach scrolled-back content, not only the visible screen. Command mode already scrolls the focused pane's scrollback (PRD #341 M5), so the two compose naturally.

Ships visible by default, so per rule 9 there is no `features::show_*` wrapper and no `graduate-*` follow-up.

### Dropping any-motion mouse tracking (M4)

`EnableMouseCapture` turns on `?1003h` — any-event tracking, which reports pointer motion with no button held (crossterm 0.29 `src/event.rs:325-333`). The deck handles **no** `MouseEventKind::Moved` anywhere; those events are dropped by a `_ => {}` arm after being decoded, and because the event loop re-renders once it has drained the queue, every mouse movement across the terminal currently costs a full redraw for an event nobody reads.

Crossterm's helper is all-or-nothing, so requesting `?1000h ?1002h ?1015h ?1006h` by hand is the only way to drop 1003. That is a deviation from the crossterm helper and PRD #242 signals sensitivity about this dependency, so it belongs in its own commit with the reasoning attached. Teardown keeps using `DisableMouseCapture`, which emits `?1003l` regardless and is a harmless no-op for a mode never enabled.

There is a secondary hypothesis worth *testing* here but not promising: 1003 is the tracking mode most likely to defeat a terminal's native Shift-drag bypass, which would explain #96's "Shift+drag has no effect". If dropping it restores that bypass on VTE terminals, it partially answers #197 for free.

### Cross-version safety

None. TUI-only, no daemon or protocol surface, patch bump. Recorded here so rule 12's check is visibly answered rather than skipped.

## Success Criteria

- Dragging in **any** visible pane selects text in that pane, on the first gesture, on both dashboard and mode tabs — no prior focus step, nothing lost.
- After releasing a drag, the user can see exactly what was selected.
- Copied text is on the system clipboard and pastes into other applications on macOS, on a VTE-based Linux terminal, and on a terminal with no OSC 52 support at all.
- Over ssh, copying still reaches the clipboard of the terminal in front of the user.
- The status message never claims a copy that did not happen.
- Text can be selected and copied with the keyboard alone, including from scrolled-back content.
- A wheel event never forwards a coordinate to a child process that is not where the pointer is.
- A user who has never read the docs can discover the gesture from `?`; a user whose clipboard is empty can find out why from `docs/troubleshooting.md`.
- #315 and #362 are closed by this work, and #96/#98 would not have been filed against it.

## Milestones

Ordered by user pain removed. Each is independently shippable — M1 or M2 alone is a coherent release.

- [ ] **M1 — Clipboard delivery that lands.** Native clipboard write plus OSC 52 fallback, process-lifetime handle, graceful headless degradation, truthful status message. Resolves the root cause of #96, #98 and the delivery half of #315.
- [ ] **M2 — Pointer-routed pane mouse events.** `pane_id` on `TextSelection`; per-pane rects stored for the Cards path; `Down` hit-tested against the pane under the pointer on both tab kinds; focus-and-select in one gesture; highlight drawn on the selection's own pane; wheel coordinate forwarding hit-tested. **Closes #362.** Resolves the issue title of #315.
- [ ] **M3 — Selection survives its own gesture.** Drag selections persist after release like multi-click ones, with a decided rule for what clears them.
- [ ] **M4 — Drop unused any-motion tracking.** `?1003h` no longer requested; one redraw per mouse-move removed.
- [ ] **M5 — Keyboard copy mode.** Enter, move, select, yank, exit — reaching scrollback, no mouse, visible by default.
- [ ] **M6 — Test coverage.** A `drag()` helper on `TuiDeck`; L1 coverage of selection geometry, pointer routing, pane scoping and highlight placement; `mode/scroll/001` extended for the wheel change; at least one PTY-attached L2 test driving copy mode against a real pane, per rule 4. Note for reel eligibility: a `cat` stand-in leaves this feature with no clip — a real agent on a cheap model plus the ` [reel]` marker is what earns one.
- [ ] **M7 — Docs and changelog.** A `## Copy and paste` section in `docs/troubleshooting.md` (per-terminal OSC 52 support, tmux `set-clipboard on`, `Cmd+V` vs `Ctrl+V` on macOS, and why native drag cannot respect pane columns); the mouse gesture and copy mode in `docs/keyboard-shortcuts.md`; a line in the `?` overlay (`src/ui.rs:14542-14543`); changelog fragment.

## Risks

- **A new dependency in the release path.** `arboard` reaching the Windows and cross builds is the main unknown in M1. Verify before committing; the shell-out variant is the fallback and costs nothing but Linux robustness.
- **X11 and Wayland clipboard ownership.** Copied text can disappear when the deck exits if no clipboard manager is running. Better than today, but it will read as a bug to someone, so document it rather than discover it in an issue.
- **The honest status message reading as a regression.** Users who saw an unqualified "Copied to clipboard" may experience a qualified message as the feature getting worse. Wording is part of the work, not a detail.
- **M2 changes long-standing `PaneInput` behaviour.** #362 flags this explicitly: "wheel anywhere drives the focused pane" has been true since well before PRD #341, and someone may rely on scrolling without moving the pointer.
- **Copy mode becoming a mode with its own rules.** Same creep risk PRD #313 names for zoom. If it starts acquiring keybindings and behaviours beyond select-and-yank, it has outgrown this PRD.
- **The reported symptom may not be the diagnosed one.** #315's reporter may simply have pressed `Ctrl+V` on macOS, where it is not a paste at all. M1 is correct regardless of which explanation holds, but the issue could close on M7 alone — worth knowing before sizing the work.
- **Live output moving under a selection.** See Open Question 6; this may turn out to be a real correctness bug rather than a polish item.

## Open Questions

1. **`arboard`, or shell out to the platform clipboard utility?** Leaning `arboard` for Linux robustness, contingent on it building clean for every release target. The shell-out route needs no dependency and dodges the X11 ownership problem, but fails on any Linux box without `wl-copy`/`xclip` — which is where #96 came from.
2. **Does the wheel follow the pointer or the focus?** #362 asks for this to be decided deliberately. Leaning pointer, for consistency with every other pointer affordance in the deck, while acknowledging that scroll-by-focus is genuinely useful because it lets you scroll without moving the pointer. Independent of this, the fabricated-coordinate forwarding gets fixed either way.
3. **What clears a persisted selection?** Candidates: the next `Down` in a pane, `Esc`, a tab switch, a mode change, typing into the pane. Too many is as bad as too few — a selection that survives everything becomes stale highlight.
4. **What key enters copy mode?** The command-mode single-letter pattern (`g`, `r`, `/`) is the established precedent and preserves passthrough of every `Ctrl+<key>` to the agent. Note PRD #313 is separately eyeing command-mode `z`, so pick without colliding.
5. **Can copy mode select from a non-focused pane?** The mouse path will be able to after M2. Copy mode operating only on the focused pane is simpler and probably right, but the asymmetry should be a decision.
6. **What happens when live output scrolls under a live selection?** Selection coordinates are pane-relative and the scrollback offset is resolved at copy time (`src/ui.rs:11175`), so an agent emitting output between `Down` and `Up` means the copied text is not the text that was highlighted — and the highlight itself visually slides over changing content. Options: anchor the selection to the scrollback position at `Down`, or freeze the pane's scrollback view while a selection is live. Needs deciding in M2; it may be the sharpest correctness bug in the whole family.

## Work Log

### 2026-08-05 — Created

Written from an analysis of #315, which on inspection turned out to contain four independent defects rather than the one it reports, and to share a root cause with #362 (mouse events routed by focus rather than by pointer) and with the already-closed #96/#98 (OSC-52-only delivery on terminals that discard it). #362 is folded in as part of M2 rather than left to re-derive the same decision separately.

Two scoping decisions taken at creation: #197's `mouse.enabled` toggle is explicitly **not** the fix here (native terminal selection cannot respect pane columns, which is the actual complaint in #315's follow-up), and copy mode ships **visible by default** — rule 9's experimental-flag question was asked and answered no.

Reproducibility, for whoever picks this up: M2, M3 and M4 are fully reproducible locally on any platform — but note that a stacked dashboard draws only the focused pane (PRD #311), so reproducing the non-focused-pane bug needs a mode tab or a tiled dashboard. M1's failure mode depends on the terminal and the iTerm2 variant needs macOS; `printf '\033]52;c;%s\033\\' "$(printf 'probe' | base64)"` followed by a paste establishes whether a given terminal honours OSC 52 at all.
