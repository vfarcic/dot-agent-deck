# PRD #242: Unify the crossterm dependency (one version, not 0.28.1 + 0.29.0)

**Status**: Not started
**Priority**: Medium
**Created**: 2026-07-28
**GitHub Issue**: [#242](https://github.com/vfarcic/dot-agent-deck/issues/242)
**Feature flag**: **None** — no new user-visible surface (CLAUDE.md rule 9). This is dependency hygiene; the user-visible goal is that nothing changes.

## Problem Statement

The build compiles **two** copies of crossterm:

```
crossterm v0.28.1  <-  dot-agent-deck            (direct dep, Cargo.toml)
crossterm v0.29.0  <-  ratatui-crossterm v0.1.0  <-  ratatui v0.30.0  <-  dot-agent-deck
```

Cargo permits this because `0.28` and `0.29` are semver-incompatible under `0.x` rules, so it does not unify them. Both end up in the binary.

### Why two copies is a hazard, not just waste

Crossterm keeps critical state in **process globals**, not in a handle the caller owns:

- the raw-mode flag behind `enable_raw_mode` / `disable_raw_mode` / `is_raw_mode_enabled`
- a lazily-initialised event reader whose mio source binds permanently to whichever tty fd exists on first poll

With two compiled copies there are **two independent sets of that state**. Raw mode enabled through 0.28 is invisible to 0.29 and vice versa; an event reader initialised in one cannot be observed or reset through the other. Any code path that touches terminal state through ratatui's copy while the deck drives its own is operating on a different global than it appears to be.

Today production decoding runs entirely through **0.28.1** — `src/ui.rs` reads events via our direct dependency — so no failure has been observed. The hazard is that this is true by accident of which paths happen to be exercised, not by construction. It is the kind of defect that surfaces as an unreproducible terminal-state bug long after the change that enabled it.

### Why it also weakens a correctness guarantee

PRD #227 established that the deck's pane-input encoder (`ui::keyevent_to_bytes` / `ui::ctrl_c0_byte`) must be the exact inverse of crossterm's decoder, and added `keyevent_ctrl_c0_matches_crossterm_decoder` to prove it by driving crossterm's real decoder. That test necessarily pins **one** decoder — 0.28.1, our direct dep. With two decoders compiled in, the guarantee is structurally partial: it covers the version currently on the production path, and would silently stop covering it if that path ever shifted.

Two forwarding bugs in PRD #227 (`Ctrl+[`, then `Ctrl+3`–`Ctrl+8`) were both facts about crossterm's decoder rather than defects in our logic, which is why this guarantee is worth keeping whole.

### Scope of the change

The crossterm API surface the deck uses is narrow and confined to four files:

| File | Uses |
|---|---|
| `src/ui.rs` | `event::poll` / `read`, `execute!`, key + mouse event types, raw mode, `Push`/`PopKeyboardEnhancementFlags`, `supports_keyboard_enhancement`, `is_raw_mode_enabled` |
| `src/keybindings.rs` | key event / modifier types |
| `src/connect.rs` | terminal control |
| `src/build_version_handshake.rs` | terminal control |

This is a bounded change, not a sprawling one — which is precisely why it deserves its own PR rather than riding along with unrelated work.

## Solution Overview

Resolve the dependency graph to a single crossterm version. `ratatui-crossterm` makes this a **feature-flag decision** rather than necessarily a code migration:

```toml
# ratatui-crossterm 0.1.0
crossterm_0_28 = ["dep:crossterm_0_28"]   # package = "crossterm", version = "0.28"
crossterm_0_29 = ["dep:crossterm_0_29"]   # package = "crossterm", version = "0.29"
default        = ["underline-color", "crossterm_0_29"]
```

`ratatui 0.30` passes both through as its own `crossterm_0_28` / `crossterm_0_29` features, and re-exports the selected one as `ratatui::crossterm` (`ratatui-0.30.0/src/lib.rs:435`). So there are two viable end states.

### Option A — align ratatui onto 0.28.1

Depend on ratatui with `default-features = false` plus an explicit feature list including `crossterm_0_28`, so `ratatui-crossterm` binds to the same 0.28 our direct dep pins.

- **For:** minimal risk and likely zero source changes. Preserves the exact decoder contract PRD #227 documented by hand (with `parse.rs` line references) and pinned via `crossterm = "=0.28.1"`.
- **Against:** pins us to an older crossterm. ratatui's `crossterm_0_28` feature is a transitional escape hatch, not its default — it will eventually go away, making this a deferral rather than a resolution.

### Option B — move forward to 0.29 (recommended)

Take ratatui's default (`crossterm_0_29`) and align our own usage to 0.29, either by bumping the direct dependency or by dropping it entirely in favour of the `ratatui::crossterm` re-export — which structurally guarantees one version, since there is then only one declaration.

- **For:** lands where ratatui and the ecosystem already are, so it resolves the split rather than postponing it. Dropping the direct dep makes recurrence impossible by construction.
- **Against:** requires working through any 0.28 → 0.29 API changes across the four files above, and invalidates PRD #227's hand-derived decoder contract, which is documented against 0.28.1's `parse.rs`.
- **Why the second objection is manageable:** `keyevent_ctrl_c0_matches_crossterm_decoder` drives crossterm's *actual* decoder over the whole control range. Pointed at 0.29 it reports immediately whether decoding changed, so re-deriving the contract becomes verification rather than guesswork. **This PRD is much safer to attempt because PRD #227 landed that test first.**

**Recommendation: Option B**, with Option A as the fallback if 0.29's decoder diverges in a way that is expensive to absorb. Decide by prototyping A first (cheap, proves the unification mechanism) and then attempting B.

## Scope

### In Scope

- Resolve the graph to exactly one crossterm version; prove it with `cargo tree`.
- Whichever option is taken, keep the encoder↔decoder contract intact: `ui::ctrl_c0_byte`'s documented `parse.rs` references must match the version actually compiled, and `keyevent_ctrl_c0_matches_crossterm_decoder` must pass against it.
- Update `Cargo.toml`'s `=0.28.1` pin and its explanatory comment (added by PRD #227) to reflect the new reality — including the note that currently says ratatui pulls 0.29 separately.
- Verify no regression in terminal-state handling: raw mode, mouse capture, bracketed paste, and the keyboard-enhancement push/pop lifecycle PRD #227 added.

### Out of Scope

- Any change to the pane-input encoder's *behavior*. If 0.29 decodes differently, the encoder is updated to stay inverse — but the bytes forwarded to agents for a given physical keypress must not change.
- Upgrading ratatui itself beyond what unification requires.
- Broader dependency-tree deduplication. This PRD is about crossterm specifically, because of its process-global state.

## Success Criteria

- `cargo tree` shows exactly **one** crossterm version.
- `ui::ctrl_c0_byte`'s documented decoder contract references the compiled version, re-verified by hand against that version's `parse.rs`.
- `keyevent_ctrl_c0_matches_crossterm_decoder` passes, and still goes RED against both PRD #227 regressions (drop the `'3'..='7'` arm; drop the `'['` arm).
- `cargo test-fast` green; full `cargo test-e2e` green (pre-PR tier).
- Manual confirmation that Shift+Enter still inserts a newline in an embedded pane, that plain Enter still submits, and that no keyboard mode leaks after exit — the PRD #227 behaviors most exposed to a crossterm change.
- No user-visible change of any kind. Success is that nothing moves.

## Milestones

- [ ] **M1**: Unification mechanism proven — the graph resolves to one crossterm version and `cargo tree` shows it (Option A prototype is the cheapest way to establish this)
- [ ] **M2**: Option decided (A or B) with the reasoning recorded, having measured what 0.29 actually changes by pointing the round-trip test at it
- [ ] **M3**: Chosen option implemented across the four crossterm-using files; `Cargo.toml` pin and comment updated to match
- [ ] **M4**: Decoder contract re-verified against the compiled version and `ui::ctrl_c0_byte`'s documentation corrected; round-trip test green and still RED against both known regressions
- [ ] **M5**: Terminal-state lifecycle regression-checked (raw mode, mouse capture, bracketed paste, keyboard-enhancement push/pop on normal + error + panic exits); fast and e2e tiers green
- [ ] **M6**: Changelog fragment, cross-version contract answer per rule 12, and PRD #227's follow-up note closed out

## Risks and Mitigations

| Risk | Mitigation |
|---|---|
| 0.29 decodes control bytes differently, silently breaking pane key forwarding | `keyevent_ctrl_c0_matches_crossterm_decoder` detects this automatically over the whole `0x00..=0x1f` + `0x7f` range — the reason this PRD is attempted after PRD #227 rather than with it |
| Option A quietly enables *both* crossterm features, since cargo features are additive | Use `default-features = false` with an explicit feature list, and verify with `cargo tree` rather than by inspection |
| Dropping the direct dep for `ratatui::crossterm` couples our terminal handling to ratatui's release cadence | Weigh during M2. A version we declare ourselves is easier to pin; a version we inherit cannot desynchronise. The recurrence-proof property may be worth the coupling |
| The keyboard-enhancement APIs added by PRD #227 moved between 0.28 and 0.29 | These are the highest-risk call sites and the newest code. Check them explicitly in M3, and cover with `embed/key-forwarding/002`, which asserts the push/pop wire bytes directly |
| Two globals mask a bug that only appears *after* unification (code that accidentally relied on the split) | Full e2e tier plus the manual terminal-state checks in M5; the failure would surface as leaked or missing raw mode, which `002` and the panic-path coverage already pin |

## Open Questions

1. **Does ratatui 0.30 work with `crossterm_0_28` in practice**, or is that feature stale/untested upstream? M1's prototype answers this cheaply and gates whether Option A is even available as a fallback.
2. **Direct dependency or `ratatui::crossterm` re-export?** The re-export makes the split structurally impossible to reintroduce; a direct dep keeps us in control of the version and the pin. Decide in M2.
3. **Does 0.29 change the control-byte decoding** documented in `ui::ctrl_c0_byte`? Measurable, not speculative — point the round-trip test at it.
4. **Is the 0.28 event reader actually reached today, or is it dormant?** Worth knowing whether the hazard is live or merely latent, since it affects how urgently this should be scheduled.

## Notes

Discovered during PRD #227 while auditing a test probe that manipulated crossterm's process-global event reader — the same global state that makes the dual-version split a hazard. Deliberately kept out of PR #238 so a bug fix would not become a dependency migration, and so the review, audit, and e2e gate that cleared #227 would remain valid.
