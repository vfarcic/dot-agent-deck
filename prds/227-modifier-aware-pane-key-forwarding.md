# PRD #227: Modifier-aware key forwarding to embedded agent panes (Shift+Enter = newline)

**Status**: Not started
**Priority**: High
**Created**: 2026-07-26
**GitHub Issue**: [#227](https://github.com/vfarcic/dot-agent-deck/issues/227)
**Feature flag**: Undecided — see [Open Questions](#open-questions). This is a correctness fix to an existing input path rather than a new user-visible surface, so the working assumption is **no** `experimental` gate (CLAUDE.md rule 9), but the terminal-mode push in M2 is a global behavior change and the implementer should confirm the call before building it.

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

- [ ] **M1**: Encoder forwards `Enter + SHIFT` as `ESC[13;2u`, with L1 coverage — the documented Ghostty keybind path now works end to end
- [ ] **M2**: Enhanced keyboard protocol pushed at startup and reliably popped (exit + panic), gated on terminal support — Shift+Enter works with no terminal config
- [ ] **M3**: Remaining modifier-losing keys (Ctrl+Enter, Shift/Ctrl+arrows) forwarded faithfully; existing keybindings verified unregressed
- [ ] **M4**: PTY-attached real-agent test proves newline-without-submit; all four agents verified in the pre-PR e2e tier
- [ ] **M5**: Documentation corrected (`docs/troubleshooting.md` rewritten, `tests/CATALOG.md` entry updated) and changelog fragment added
- [ ] **M6**: Cross-version contract answered per rule 12; feature ready for user testing

## Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Enabling the protocol changes key delivery globally and breaks an existing binding | `normalize_chord` (`src/keybindings.rs:499`) already handles both forms; `DISAMBIGUATE_ESCAPE_CODES` does not re-encode text keys. Verify the full dashboard binding set with the protocol active (M3). |
| A leaked protocol push corrupts the user's shell after a crash | Pop in the panic hook next to the existing mouse-capture/bracketed-paste restores; test the crash path explicitly. |
| Terminals vary in what they emit for Shift+Enter even after the push | M1 is verified independently of terminal behavior, so the fix does not rest solely on M2. Confirm on the maintainer's terminal before relying on M2 alone. |
| An agent not yet tested rejects CSI-u | All four currently supported agents were verified. Re-run the matrix when adding an agent; `LF` is a fallback that also worked for pi and claude. |

## Open Questions

1. **What does the maintainer's Ghostty actually emit for Shift+Enter today?** Unresolved because it needs a physical keypress. Running `cat -v` in a fresh Ghostty window (press Shift+Enter, then Enter, then Ctrl+D) distinguishes the cases: `^[` alone → `ESC+CR` (which would confirm why Claude works and Pi does not, and means M1 plus an ALT normalization suffices); `^[[13;2u` → CSI-u already, so only Gap 2 bites; a bare empty line → legacy CR, making M2 the binding constraint. This determines whether M2 is required or merely preferable.
2. **Should `Enter + ALT` be normalized to `ESC[13;2u`?** It would make Pi work with neither a protocol push nor terminal config if the terminal emits `ESC+CR`, but it removes the ability to send a genuine Alt+Enter (which Pi treats as submit). Trade-off call, deliberately not pre-decided.
3. **Experimental flag?** CLAUDE.md rule 9 asks the question for user-facing surfaces. The working assumption is no flag, since this is a fix to existing key handling rather than a new surface — but M2 changes terminal mode globally, which is the part worth a second look.
4. **`opencode` / `codex` newline conventions** beyond CSI-u were not measured; worth completing the matrix if M3 touches ALT handling.

## Verification Notes (from diagnosis)

Recorded so the implementer does not need to re-derive them:

- `pi` startup writes `ESC[>7u` and `ESC[?u` (captured from a bare `pty.fork()` session).
- crossterm 0.28 parses injected `ESC[13;2u` as `Enter + SHIFT`, `ESC[13;5u` as `Enter + CONTROL`, and `ESC[9;2u` as `BackTab + SHIFT`, all with **no** flags pushed.
- With a terminal that CSI-u-encodes real keypresses (tmux `extended-keys always` + `extended-keys-format csi-u`), a genuine Shift+Enter keypress is delivered to crossterm as `Enter + SHIFT` — confirming the outer half of the chain.
- `supports_keyboard_enhancement()` returns `false` inside tmux.
- Agent acceptance matrix as tabled above, measured on rendered screens rather than raw byte streams (raw output is too noisy to distinguish submit from newline reliably).
