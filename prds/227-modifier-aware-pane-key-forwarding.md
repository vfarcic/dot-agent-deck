# PRD #227: Modifier-aware key forwarding to embedded agent panes (Shift+Enter = newline)

**Status**: Implemented (2026-07-28) — M1–M6 landed on `prd-227-modifier-aware-key-forwarding-to-embedded-agent-pa`; review + audit resolved; pre-PR e2e gate green (2167 tests, 0 failures). Awaiting PR, CI, and merge.
**Priority**: High
**Created**: 2026-07-26
**GitHub Issue**: [#227](https://github.com/vfarcic/dot-agent-deck/issues/227)
**Feature flag**: **None** — decided (CLAUDE.md rule 9). This is a correctness fix to an existing input path, not a new user-visible surface, so it ships visible by default with no `experimental` gate and no `graduate-` follow-up. The M2 terminal-mode push was the part worth a second look; it is gated on `supports_keyboard_enhancement()`, so it self-disables where the terminal could not deliver it anyway.

## Problem Statement

Pressing **Shift+Enter** in an embedded agent pane submits the prompt instead of inserting a newline. Running the same agent directly in the same terminal — outside dot-agent-deck — works correctly. The break is entirely deck-side, and it is two independent defects that compound.

### Gap 1 — the deck never enables the enhanced (kitty) keyboard protocol

Nothing in the repository pushes `KeyboardEnhancementFlags`; TUI startup enables only mouse capture and bracketed paste (`src/ui.rs:7137`). Without that push, a kitty-capable terminal stays in legacy mode, where Shift+Enter has no distinct encoding and is delivered as a bare `\r` — so the SHIFT modifier never reaches the deck at all.

Agents that negotiate the protocol themselves cannot compensate. Captured from a real `pi` process in a bare PTY, `pi` writes at startup:

```
ESC[>7u      push enhancement flags (disambiguate + report event types + report alternate keys)
ESC[?u       query current flags
```

Under the deck, that request lands in the deck's own vt100 parser, which ignores it. It cannot propagate outward to the user's terminal, so the negotiation the agent performs for itself is lost the moment it runs embedded.

### Gap 2 — the encoder drops SHIFT even when it does arrive

`keyevent_to_bytes` (`src/ui.rs:3372`) handles only ALT (ESC prefix, `src/ui.rs:3434`) and CONTROL (`src/ui.rs:3377`). It then maps `KeyCode::Enter => vec![b'\r']` unconditionally (`src/ui.rs:3397`), and those bytes reach the PTY verbatim via `write_raw_bytes` (`src/ui.rs:6802`). Shift+Enter and Enter are therefore *literally the same byte* on the wire, for every agent.

This makes the workaround currently published in `docs/troubleshooting.md:8-49` — adding `keybind = shift+enter=csi:13;2u` to the Ghostty config — ineffective as written. Verified with a crossterm 0.28 probe: injecting `ESC[13;2u` is parsed as `Enter + SHIFT` **even with no enhancement flags pushed**, so the modifier does arrive and the encoder then discards it. That doc also misattributes the cause ("Ghostty intercepts Shift+Enter … when applications enable mouse capture mode"), and `tests/CATALOG.md:2736` records the whole area as "Outer-terminal config; no deck-side surface to test" — a conclusion this PRD invalidates.

### Why this looked Pi-specific

The defects are agent-agnostic, but the *symptom* is not, because agents differ in which newline encodings they tolerate. Claude Code also accepts `ESC+CR` as a newline; Pi treats `ESC+CR` as **submit**. So on a terminal that legacy-encodes Shift+Enter into something Claude forgives, Claude appears to work and Pi appears broken, and the shared underlying bug goes unnoticed. Verified acceptance (real agents, tmux-rendered screens, submit vs. newline distinguished by whether the draft stayed in one input box):

| agent | `ESC[13;2u` | `ESC+CR` | `LF` (0x0a) | `CR` (0x0d) |
|---|---|---|---|---|
| pi | **newline** | submit | newline | submit |
| claude | **newline** | newline | newline | submit |
| opencode | **newline** | inconclusive | not tested | not tested |
| codex | **newline** | not tested | not tested | not tested |

`ESC[13;2u` is the only encoding confirmed to work across all four supported agents, which makes a single general fix possible instead of per-agent special-casing.

### Collateral currently masked by the same gap

SHIFT is not the only modifier lost. Shift+Up arrives as `Up + SHIFT` and is forwarded as a bare `ESC[A`, dropping the modifier that a legacy terminal would have encoded as `ESC[1;2A`. Shift+Tab happens to survive because crossterm reports it as `BackTab`, which the encoder handles explicitly (`src/ui.rs:3429`).

## Solution Overview

Make pane-input key forwarding **modifier-aware** rather than modifier-lossy, and enable the terminal mode that lets modifiers arrive in the first place. Two changes, each independently useful:

1. **Encoder (fixes Gap 2).** Emit CSI-u for modified keys that have no faithful legacy encoding — starting with `Enter + SHIFT` → `ESC[13;2u`. This alone makes the already-documented Ghostty keybind workaround actually work, and is verified against all four agents.
2. **Terminal mode (fixes Gap 1).** Push `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES` at TUI startup, popped at exit and in the panic hook, so Shift+Enter reaches the deck with **no per-user terminal configuration at all**.

With both in place the chain is faithful end to end: the terminal CSI-u-encodes the keypress, crossterm reports `Enter + SHIFT`, and the deck forwards an encoding every supported agent understands.

### Architecture

The fix sits at one seam — `keyevent_to_bytes` — which is the single translation point between crossterm `KeyEvent`s and PTY bytes for interactive typing. Two properties make this safe:

- **The submit-debounce is unaffected.** `submit_debounce_duration` (`src/ui.rs:1186`) triggers only on a `\r` byte, so a CSI-u newline correctly skips the debounce that exists to make a CR read as a standalone submit.
- **The keybinding layer already anticipates the enhanced protocol.** `normalize_chord` (`src/keybindings.rs:499`) exists precisely to reconcile the legacy and kitty forms of a shifted key, so M2 builds on existing groundwork rather than new ground.

Note that `DISAMBIGUATE_ESCAPE_CODES` alone does **not** re-encode text-producing keys (that would require `REPORT_ALL_KEYS_AS_ESCAPE_CODES`), so ordinary letters keep arriving as plain characters and existing bindings are untouched.

## Scope

### In Scope

- Make `keyevent_to_bytes` modifier-aware: `Enter + SHIFT` → `ESC[13;2u`, plus a principled CSI-u path for other modifier combinations that currently lose information (Ctrl+Enter, Shift/Ctrl+arrows).
- Push/pop `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES` around the TUI session, including the panic-hook teardown path (`src/ui.rs:7127`) alongside the existing mouse-capture and bracketed-paste restores.
- Verify no regression to existing dashboard keybindings once the protocol is active.
- L1 tests for the encoder, and a PTY-attached L2 test that drives a real agent (CLAUDE.md rule 4).
- Rewrite `docs/troubleshooting.md`'s Shift+Enter section (correct cause; the workaround becomes unnecessary) and update the `tests/CATALOG.md:2736` "nothing to test deck-side" entry.
- Changelog fragment and the cross-version contract answer (CLAUDE.md rule 12).

### Out of Scope

- **Replying to the inner agent's `ESC[?u` query.** The deck's vt100 layer does not answer the protocol query, so an embedded agent believes the protocol was not accepted. Empirically all four agents parse CSI-u input regardless, so this is not needed for the fix; a deck-side responder is a larger emulation change and deferred.
- **Propagating the inner agent's requested flag set outward.** Sniffing `ESC[>Nu` from agent output and mirroring it to the outer terminal (roughly what tmux does) is the "fully correct" design; this PRD instead picks one encoding all supported agents accept.
- **Non-Enter modifier work beyond the keys named above** — no general audit of every key/modifier pairing.
- Any change to the daemon-side prompt-injection encoder (`src/pane_input.rs`), which is the orchestration write path, not the interactive keystroke path.

## Technical Approach

### M1 — modifier-aware encoder

Add a CSI-u branch to `keyevent_to_bytes` for modifier combinations that legacy encoding cannot express, keeping the existing legacy output for the unmodified cases so nothing currently working changes shape. The kitty modifier parameter is `1 + bitmask` (shift=1, alt=2, ctrl=4), giving `ESC[13;2u` for Shift+Enter and `ESC[13;5u` for Ctrl+Enter. Keep ALT+Enter's existing `ESC\r` behavior unless the Open Questions decision says otherwise.

### M2 — enable the enhanced protocol

Push the flag after `enable_raw_mode`/`ratatui::init()` and pop it on every exit path. Gate on `crossterm::terminal::supports_keyboard_enhancement()`: verified to return **false inside tmux**, so gating self-disables the feature exactly where the outer terminal could not deliver it anyway. Confirm the pop actually restores state on a crash path, since a leaked protocol push leaves the user's shell in the enhanced mode after exit.

### M4 — real-agent validation

Per CLAUDE.md rule 4 this needs a PTY-attached L2 test driving a genuine agent, not a stand-in: `cat` cannot demonstrate newline-vs-submit semantics, which is the entire behavior under test. Drive a real agent on a cheap model, inject `ESC[13;2u` into the deck's PTY, and assert the draft became two lines **and that no submission occurred** — the negative half of that assertion is the one that actually pins the bug. The verification harness used during diagnosis (tmux session + `capture-pane`, rendered-screen assertions) is a workable model. This is a bug fix, so no ` [reel]` marker is expected.

### Cross-version contract (CLAUDE.md rule 12)

The interactive keystroke path writes opaque bytes through the existing attach/PTY channel, so no `PROTOCOL_VERSION` wire-shape change is anticipated and no semantic (same-wire, different-meaning) break is expected. The implementer still owes the explicit answer, and the manual older-daemon check if the diff drifts into the daemon or hook paths.

## Success Criteria

- Pressing Shift+Enter in an embedded pane inserts a newline **without submitting** for all four supported agents (pi, claude, opencode, codex).
- It works with **no user-side terminal configuration** on a kitty-capable terminal, and continues to work for a user who already has the `csi:13;2u` Ghostty keybind applied.
- Plain Enter still submits, and the submit-debounce behavior for CR-bearing keystrokes is unchanged.
- No existing dashboard keybinding regresses with the enhanced protocol active.
- Inside tmux (no enhancement support reported) the deck degrades to today's behavior rather than misbehaving.
- Terminal state is fully restored on normal exit **and** on panic — no leaked keyboard mode.
- `docs/troubleshooting.md` describes the real mechanism, and no longer prescribes a workaround as the fix.

## Milestones

- [x] **M1**: Encoder forwards `Enter + SHIFT` as `ESC[13;2u`, with L1 coverage — the documented Ghostty keybind path now works end to end (`c737a6e`)
- [x] **M2**: Enhanced keyboard protocol pushed at startup and reliably popped (exit + panic), gated on terminal support — Shift+Enter works with no terminal config (`476b767`; leak on `?`-error returns closed by `KeyboardEnhancementGuard` in `ed93ab4`; `panic = "abort"` builds closed in `bc842e6`)
- [x] **M3**: Remaining modifier-losing keys (Ctrl+Enter, Shift/Ctrl+arrows) forwarded faithfully; existing keybindings verified unregressed (`c737a6e`; the `Ctrl+[` regression M2 exposed fixed in `9dfe1a9`; the C0 map made exhaustive-by-construction via the caret rule `ch & 0x1f` plus xterm digit aliases in `bc842e6`)
- [x] **M4**: PTY-attached real-agent test proves newline-without-submit (`9dfe1a9`), and M2's own negotiation is pinned by a deterministic companion test (`ed93ab4`) — **scope amended, see below**
- [x] **M5**: Documentation corrected (`docs/troubleshooting.md` rewritten, `tests/CATALOG.md` entry updated) and changelog fragment added (`eefb2eb`)
- [x] **M6**: Cross-version contract answered per rule 12 — no daemon/protocol/hook/orchestration file touched, so no `PROTOCOL_VERSION` bump and no semantic break (`eefb2eb`)

### M4 scope amendment (2026-07-28)

M4 originally read "all four agents verified in the pre-PR e2e tier". **Shipped instead:** one real-agent e2e (`embed/key-forwarding/001`, live interactive `claude` on Haiku) plus a deterministic no-agent companion (`embed/key-forwarding/002`) that pins M2's push/pop, over the manual acceptance matrix already recorded in [Verification Notes](#verification-notes-from-diagnosis) — which measured all four agents (pi, claude, opencode, codex) accepting `ESC[13;2u` on rendered screens.

Rationale: three further live-agent PTY tests would each add API cost and flake surface to a tier that cannot run in CI, to re-confirm a matrix already measured by hand, for a bug fix. `ESC[13;2u` is a single agent-agnostic encoding — there is no per-agent code path for a per-agent test to cover. Re-run the manual matrix when adding a new agent adapter, per the existing risk-table mitigation.

### Test coverage as shipped

| Test | Tier | Pins |
|---|---|---|
| `ui::tests::keyevent_*` (incl. `keyevent_ctrl_c0_controls`, `ctrl_c0_byte_maps_exactly_the_documented_alias_set`) | L1 unit | The full encoder: modifier params 1–8, all 42 Ctrl→C0 aliases plus Alt+Ctrl forms, exhaustive over all 128 ASCII chars so a too-wide rule cannot pass |
| `ui::tests::keyboard_enhancement_wire_bytes_are_the_ones_the_e2e_asserts` | L1 unit | Push = `CSI>1u`, pop = `CSI<1u`, so a crossterm bump fails fast |
| `embed/key-forwarding/001` | L2 PTY, real agent | Shift+Enter inserts a newline **without submitting**, in one prompt-editor box, with the deck's startup negotiation asserted |
| `embed/key-forwarding/002` | L2 PTY, no agent | Push at startup, matching pop exactly once after clean exit, push-before-pop ordering |
| `ui::tests::keyevent_ctrl_c0_matches_crossterm_decoder` | L1 unit, nextest-only | Our encoder is the exact INVERSE of crossterm's REAL decoder: a PTY on fd 0 feeds `parse_event` every legacy control byte (`0x00..=0x1f` + `0x7f`), all 42 aliases in the M2 `CSI <cp>;5u` form, and the M1/M3 modifier sequences |

**Why the round-trip test is nextest-only:** it installs a PTY on fd 0 and drives crossterm 0.28's *process-global* raw-mode flag and cached event reader, so it is sound only when it owns the process. `cargo nextest` (i.e. `cargo test-fast`, the fast-tier gate) is process-per-test and runs it; a plain `cargo test` shares one process across the crate, where a competing event consumer could turn the probe's bounded wait into a hang, so there it prints a `SKIP:` line and returns without failing. Process isolation is also what makes its deliberately non-panic-safe fd surgery acceptable — a failure means a dedicated, already-failed process about to exit, with no sibling test left to corrupt.

**Residual human step:** the *line references* in `ctrl_c0_byte`'s doc comment (into `crossterm-0.28.1/src/event/sys/unix/parse.rs`) can still go stale silently, since the round-trip test proves agreement without checking where it is written down. `Cargo.toml` therefore pins the direct dependency to `=0.28.1`, so a decoder change requires a deliberate, reviewed edit that re-verifies those references. (ratatui 0.30 also pulls crossterm 0.29 in via `ratatui-crossterm`; the two coexist because 0.28/0.29 are semver-incompatible, and the pin binds only the dependency `src/ui.rs` reads events from. Unifying the split is out of scope here.)

## Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Enabling the protocol changes key delivery globally and breaks an existing binding | `normalize_chord` (`src/keybindings.rs:499`) already handles both forms; `DISAMBIGUATE_ESCAPE_CODES` does not re-encode text keys. Verify the full dashboard binding set with the protocol active (M3). |
| A leaked protocol push corrupts the user's shell after a crash | Pop in the panic hook next to the existing mouse-capture/bracketed-paste restores; test the crash path explicitly. |
| Terminals vary in what they emit for Shift+Enter even after the push | M1 is verified independently of terminal behavior, so the fix does not rest solely on M2. Confirm on the maintainer's terminal before relying on M2 alone. |
| An agent not yet tested rejects CSI-u | All four currently supported agents were verified. Re-run the matrix when adding an agent; `LF` is a fallback that also worked for pi and claude. |

## Open Questions

All four are now **resolved** (2026-07-28). Kept with their answers rather than deleted, since the reasoning is what a future change to this path needs.

1. **What does the maintainer's Ghostty actually emit for Shift+Enter today?** **Moot — never measured, and no longer load-bearing.** Both gaps were fixed independently, so the answer no longer decides anything: M1 makes the encoder faithful whatever arrives, and M2 makes the terminal emit CSI-u without user configuration. The question mattered only while we were choosing *which* gap to fix.
2. **Should `Enter + ALT` be normalized to `ESC[13;2u`?** **No.** `Alt+Enter` keeps its legacy `ESC\r`, preserving the ability to send a genuine Alt+Enter (which Pi reads as submit). ALT folds into the CSI-u modifier bitmask only when combined with SHIFT/CONTROL. With M2 landed, the normalization's only benefit — rescuing a terminal that emits `ESC+CR` — is unnecessary.
3. **Experimental flag?** **No flag.** A correctness fix to existing key handling is not a new surface (CLAUDE.md rule 9). M2's global terminal-mode change was the part worth the second look; it is gated on `supports_keyboard_enhancement()` and popped on every exit path, so it self-disables where unsupported and restores state on the way out.
4. **`opencode` / `codex` newline conventions beyond CSI-u.** **Not needed.** M3 did not change ALT handling (see Q2), so the matrix gap it was contingent on never opened. All four agents were already measured accepting `ESC[13;2u`, which is the single encoding shipped.

## Verification Notes (from diagnosis)

Recorded so the implementer does not need to re-derive them:

- `pi` startup writes `ESC[>7u` and `ESC[?u` (captured from a bare `pty.fork()` session).
- crossterm 0.28 parses injected `ESC[13;2u` as `Enter + SHIFT`, `ESC[13;5u` as `Enter + CONTROL`, and `ESC[9;2u` as `BackTab + SHIFT`, all with **no** flags pushed.
- With a terminal that CSI-u-encodes real keypresses (tmux `extended-keys always` + `extended-keys-format csi-u`), a genuine Shift+Enter keypress is delivered to crossterm as `Enter + SHIFT` — confirming the outer half of the chain.
- `supports_keyboard_enhancement()` returns `false` inside tmux.
- Agent acceptance matrix as tabled above, measured on rendered screens rather than raw byte streams (raw output is too noisy to distinguish submit from newline reliably).
