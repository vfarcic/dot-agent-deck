# PRD #104: Snapshot replay preserves PTY dimensions so reattach doesn't scramble scrollback

**Status**: Defined; ready to implement
**Priority**: Medium
**Created**: 2026-05-22
**GitHub Issue**: [#104](https://github.com/vfarcic/dot-agent-deck/issues/104)

## Problem

When a user disconnects from a `dot-agent-deck connect` session and reconnects, the current viewport in each embedded pane renders correctly, but **scrolled-back content is scrambled**: overlapping text, narrow vertical text strips on the right edge, and inner-TUI status lines (e.g. Claude Code's `to mode on (shift+tab to cycle) · esc to interrupt`) duplicated in the middle of scrollback rows.

The scrambling is isolated to scrollback — the live screen recovers within a frame or two of reattach because the inner agent issues a fresh full-screen redraw on SIGWINCH. Scrolling up to read prior model output is the user-visible failure.

### Why this is happening

On reconnect, `EmbeddedPaneController::hydrate_from_daemon` (`src/embedded_pane.rs:714-877`) walks the daemon's `list_agents` reply and opens an `AttachStream` for each surviving agent. The daemon's first wire act is to emit the agent's **raw scrollback bytes** as one `KIND_STREAM_OUT` frame (`src/daemon_protocol.rs:903-917`), drawn from `AgentBus::scrollback` — a 1 MiB ring buffer of literal PTY output.

The client feeds those bytes into a freshly-constructed `vt100::Parser`. The parser's initial dimensions come from this site in `hydrate_from_daemon` (`src/embedded_pane.rs:851-866`):

```rust
// PRD #76 M2.15: at hydration time we don't know the daemon's
// current PTY dims (the daemon doesn't echo them via `list_agents`).
// Seed the local vt100 parser at 24×80 and let the post-hydration
// resize sweep in `ui.rs` immediately push the real viewport dims …
self.wire_stream_pane(pane_id, agent_id, conn, name, None, cwd, 24, 80);
```

That hard-coded `24, 80` is the root cause. The daemon's PTY was almost certainly opened at the previous TUI viewport size — easily 120+ cols by 40+ rows — and the snapshot bytes contain ANSI sequences sized for that geometry:

- absolute cursor positioning (`CSI row;col H`) referencing columns past 79,
- line wraps emitted from positions that don't wrap at 80,
- full-screen redraws (clear + reposition + content) issued by the inner TUI for an N-column screen.

When those bytes are parsed at 24×80:

- cursor-position sequences are clamped to col 79; content meant for cols 80+ overprints col 79;
- the parser inserts spurious wraps at col 80;
- frame redraws land on the wrong rows because the inner TUI's row arithmetic assumed a 40-row screen;
- residual text spills into the next row, producing the narrow vertical strips visible on the right edge of a scrolled-back pane.

The post-hydration `resize_pane_pty` sweep in `ui.rs` then reshapes the parser to the *current* local viewport. But **vt100 does not reflow scrollback on resize**: rows that have already aged out of the live screen are kept at the columns they were written at. The corruption from the 24×80 parse is therefore baked permanently into the scrollback ring inside the parser, and survives every subsequent resize.

Why the bottom looks fine: once the daemon's PTY is resized to match the new viewport, the inner agent (Claude Code / OpenCode) responds to SIGWINCH with a full-screen redraw. Those fresh bytes arrive at the *correct* dimensions and overwrite the live screen, so the current viewport is clean. Only the historical rows — the ones already pushed into scrollback before the resize landed — show the parse damage.

### Why the protocol can't tell us the dims today

`AgentRecord` (`src/agent_pty.rs:765-800`) — the per-agent payload returned by `list_agents` — carries `id`, `pane_id_env`, `display_name`, `cwd`, `tab_membership`, `agent_type`. It does **not** carry the agent's current PTY rows/cols. The comment in `hydrate_from_daemon` calls this out explicitly as a known shortcut from M2.15.

`RunningAgent` on the daemon side (`src/agent_pty.rs:699-755`) also does not store rows/cols as fields. `AgentPtyRegistry::resize` (`src/agent_pty.rs:1181-1203`) currently only ioctls the PTY master — it does not update any registry-side field. The kernel knows the dims (via `TIOCGWINSZ` on the master), but the registry does not, so `list_agents` has nothing to echo back even if we wanted it to.

### Why this matters

- **It silently corrupts a primary workflow.** Reading back the model's prior output is the obvious reason to scroll up in an agent pane. After every reconnect, that history is unreadable until the agent has produced enough fresh output to push the corrupted rows off the end of the 1 MiB scrollback ring — minutes to hours depending on agent verbosity, sometimes the rest of the session.
- **It hides under a "looks fine" first paint.** The live viewport recovers on its own, so the bug is invisible until the user happens to scroll. It survives manual smoke-testing and was only noticed in normal use.
- **It compounds with `connect` ergonomics.** PRD #76's whole point is that users can ssh-drop and reconnect freely without losing agents. This bug means every reconnect costs scrollback fidelity — exactly the property reconnect was supposed to preserve.
- **It's a protocol gap, not a render gap.** PRD #84 (rendering layer rework) addresses layout/PTY/widget invariants for the *live* screen. This bug is upstream of all of that — by the time bytes reach the parser, they've already been mis-parsed at the wrong width. No widget-level fix can recover scrollback rows whose cells were written from clamped escape sequences.

## Solution

Plumb the daemon's current PTY dimensions through `list_agents` and use them to size the client's vt100 parser before any snapshot bytes are fed in. Then close the residual case where a single snapshot spans multiple dimension epochs by clearing the snapshot ring on the daemon side whenever the PTY is resized.

The contract we are aiming for:

1. **Every `AgentRecord` carries the daemon's current PTY (rows, cols).** Optional in serde for wire compatibility with older daemons; an absent field decodes as `None` and falls back to today's 24×80 placeholder.
2. **`hydrate_from_daemon` initializes each local vt100 parser at the dims it received from `list_agents`.** The post-hydration resize sweep continues to run unchanged; its role is now to update from "daemon's dims" to "local viewport dims," not from "wrong dims" to "anything correct".
3. **The daemon clears `AgentBus::scrollback` on every PTY resize.** A snapshot returned to a client always represents a single (rows, cols) epoch — the agent's current one. The live agent's SIGWINCH-driven redraw repopulates the scrollback at the new dims within the first frame, so this is not a content-loss for normal interactive use.

### Shape of the change

- **`src/agent_pty.rs`**:
  - Add `rows: u16, cols: u16` fields to `RunningAgent`. Populate at spawn time from `SpawnOptions::rows/cols` (already present at `src/agent_pty.rs:322-324`). Update in `AgentPtyRegistry::resize` (`src/agent_pty.rs:1181-1203`) in the same critical section that ioctls the master — so the stored value is always the last value the daemon successfully ioctl'd.
  - Add `rows: u16, cols: u16` to `AgentRecord` (`src/agent_pty.rs:765-800`) with `#[serde(default)]` so older daemons round-trip as `0, 0`. Populate in `AgentPtyRegistry::agent_records` (the function that materializes the `Vec<AgentRecord>` `list_agents` returns).
  - In `AgentBus::push` and the `AgentBus`-owning side of `AgentPtyRegistry::resize`: on resize, drop the `scrollback: VecDeque<u8>` to length 0 before any further bytes are pushed. The lock that serializes push/snapshot already covers this; the resize handler takes the same lock to mutate.

- **`src/embedded_pane.rs:851-866`** (the placeholder site): replace the hard-coded `24, 80` with the dims carried on the `AgentRecord`. Fall back to `24, 80` only when the daemon returned `(0, 0)` — i.e. when talking to a daemon that predates this PRD. Add a single debug log at the fall-back so the case is observable.

- **`src/embedded_pane.rs` — `HydratedPane`** (`src/embedded_pane.rs:17`): no field change is strictly required, but threading dims through `HydratedPane` to the UI is reasonable if the resize sweep in `ui.rs` finds them useful. Decide during M2 implementation — the UI may simply continue to resize from its own viewport measurement.

- **Tests**:
  - Roundtrip: `AgentRecord { rows: 120, cols: 40, .. }` serializes and deserializes; an `AgentRecord` JSON literal *without* `rows/cols` (the pre-PRD shape) deserializes with `0, 0`.
  - Daemon-side: spawning an agent at 120×40 surfaces `120, 40` on the subsequent `list_agents`; calling `resize(id, 100, 30)` updates the stored value and the next `list_agents` reports `100, 30`.
  - Daemon-side: after `resize`, `snapshot(id)` returns empty bytes; subsequent writes appear in a fresh snapshot.
  - Client-side hydration: a fake `AgentRecord` with `rows: 120, cols: 40` causes `wire_stream_pane` to build a parser at 120×40, not 24×80 (verify via the parser's `screen().size()` immediately after wiring). An `AgentRecord` with `rows: 0, cols: 0` falls back to 24×80 and emits the debug log.

### Out of scope

- **Reflowing vt100 scrollback on resize.** vt100 doesn't, and writing a reflow layer is the wrong scope here. After this PRD, scrollback rows are correctly parsed at the daemon's dims at the moment they're written; if the local viewport is later wider or narrower, those rows are still rendered at their original width (today's behavior for resize during a session). This is acceptable degradation versus the current full scramble.
- **Changing the snapshot from raw bytes to rendered screen state.** That is a much larger redesign and is the right level of fix for the full mid-snapshot-resize case. It belongs with PRD #84 or its own follow-up.
- **Daemon-side parser maintenance.** We do not add a `vt100::Parser` on the daemon side. The daemon stays byte-blind; the dims travel as side metadata.
- **Live streaming.** Only the *initial* snapshot replay is changed. Live bytes after attach already arrive at the right dims because the client resizes the daemon's PTY immediately after attaching.
- **Changes to `connect.rs` / SSH plumbing.** The fix is entirely in the daemon protocol payload and the client's parser-init step.
- **PRD #76 wire-compat shims removal.** We add a new optional field; older daemons keep working unchanged.

## Milestones

- [ ] **M1 — Daemon stores and reports current PTY dims.** Add `rows/cols` to `RunningAgent` (populated at spawn, updated in `resize`). Extend `AgentRecord` with optional `rows/cols` (serde default 0). Update `agent_records()` to populate the fields. Unit tests cover spawn → list, resize → list, and the `0,0` fallback for older daemons. **Exit criterion**: `list_agents` reports the correct, current dims for every live agent, and the JSON shape is backwards-compatible with daemons predating this PRD.

- [ ] **M2 — Client uses daemon's dims for parser init.** In `hydrate_from_daemon`, read `rows/cols` from each `AgentRecord` and pass them to `wire_stream_pane` instead of the hard-coded `24, 80`. Fall back to `24, 80` only when the daemon returns `0, 0`, emitting a single debug log. Unit/integration test: a hydrated `AgentRecord` at `120×40` produces a vt100 parser at `120×40` (assert via `screen().size()` before any bytes flow). **Exit criterion**: snapshot bytes are no longer parsed at the wrong dimensions on reconnect against a same-PRD daemon.

- [ ] **M3 — Daemon clears snapshot on PTY resize.** In `AgentPtyRegistry::resize`, after the master `resize` ioctl succeeds, drop `AgentBus::scrollback` to empty under the same lock that serializes pushes and snapshots. Unit test: write bytes → snapshot non-empty → resize → snapshot empty → write more bytes → snapshot contains only the new bytes. **Exit criterion**: a snapshot delivered to a client always covers a single (rows, cols) epoch.

- [ ] **M4 — Failure-mode reproducer.** Build a deterministic test (in `tests/`, ideally riding the PRD #77 harness if it can drive a daemon detach+reattach) that: spawns an agent at 120×40, writes content that lands a recognizable sentinel string at col 100 of a row that will be in scrollback, simulates detach + reattach via the new path, scrolls back in the local parser, and asserts the sentinel is intact at col 100 (not clamped to col 79). This is the regression gate for this bug class. **Exit criterion**: reproducer fails on `main` (pre-PRD), passes after M1–M3.

- [ ] **M5 — Pre-PR validation.** Single end-to-end interactive pass per `feedback_validate_pre_pr.md`: ssh into a remote that's running `dot-agent-deck` with at least one active orchestrator agent that has produced ample scrollback content, disconnect (close the ssh session), reconnect via `dot-agent-deck connect <name>`, scroll up in the agent pane, confirm no scrambling, no narrow-strip artifacts, no mid-scrollback duplicated status lines. Then PR.

## Validation Strategy

Two layers, mirroring PRD #84's pattern:

1. **The M4 reproducer.** The bug is otherwise non-obvious because the live viewport always looks fine — only scrolled-back rows expose the failure. A deterministic test that writes a sentinel into scrollback and verifies it after a simulated reattach turns "did the user notice the scramble" into a pass/fail signal. This is the regression gate that lets future protocol or hydration changes catch this class without manual scrolling.

2. **M5 interactive pass.** The bug surfaced via real ssh + reconnect, so the final gate is a real ssh + reconnect (`feedback_validate_pre_pr.md`).

Per-milestone interactive validation is explicitly not required. M1 and M3 are validated by the unit tests they ship with; M2 is validated by the parser-size assertion in its own test; M4 is the catalog test; M5 is the user pass.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| The 1 MiB snapshot still spans a resize that happened just before the user attached. Old daemons (which don't clear-on-resize) talking to a new client would still produce mid-snapshot scramble. | M3 closes this for the same-version case. Cross-version (new client, old daemon) is acceptable degradation — the same client running against a new daemon is the common path on user upgrade because the daemon is the long-lived process. PRD #103's build-version handshake already provides the surface to warn or block here if it becomes an issue. |
| Clearing scrollback on resize loses recent history a user might have wanted to scroll back to mid-session. | The inner TUI's SIGWINCH-driven full-screen redraw repopulates the scrollback at the new dims within the first frame. For a non-TUI agent (rare in this codebase — Claude Code and OpenCode are both TUIs), the trade-off is "lose a few KB of mixed-dimension bytes" versus "scrolled-back rows are scrambled on every resize." The latter is worse. If a non-TUI use case emerges later, a follow-up can convert the clear into a dimension-tagged segment boundary instead. |
| `AgentRecord` is also persisted across daemon restarts via the registry. Adding fields with `#[serde(default)]` keeps the on-disk shape forward-compatible, but a state-file written by a new daemon and then read by an old daemon would drop `rows/cols` silently. | Acceptable: the *new* daemon would re-derive dims at spawn / resize regardless; the only consumer of the field on read is `list_agents`. A downgrade is a manual user action and an older daemon reverts to the 24×80 placeholder — same fallback as the wire-format path. |
| vt100's behavior when initialized at non-default dims has subtle edge cases (e.g. zero rows or cols) that could regress on construction with daemon-supplied values. | `AgentPtyRegistry::resize` already rejects zero values and clamps to `PTY_RESIZE_DIM_MAX`. The hydration path adds a defensive clamp: any value outside `[1, PTY_RESIZE_DIM_MAX]` reverts to the 24×80 placeholder with a debug log. Future-self gets a visible signal rather than a vt100 panic. |
| The dims captured at spawn time on `RunningAgent` could drift from the kernel's actual TIOCGWINSZ if a third party calls `resize` through some path that doesn't go through `AgentPtyRegistry::resize`. | The registry is the only resize call site (`embedded_pane.rs` resize worker → `daemon_client` → `AgentPtyRegistry::resize`). The audit in PRD #92 (process-boundary invariant audit) covers this kind of bypass; if a new resize path appears, the same audit catches it. |
| Tests covering hydration require a running daemon and a controllable PTY-bearing agent — that's a heavy fixture. | Keep the M4 reproducer in-process where possible: feed crafted bytes directly into the new `wire_stream_pane(..., 120, 40)` path and assert on parser state, without spawning a real child. The protocol round-trip is covered by M1's serde tests; the full daemon-attach-replay loop is covered by the M5 interactive pass. |
| Pre-resize bytes can leak past the M3 clear ring: `pump_reader` may have already returned from `reader.read()` with pre-resize bytes in its userspace buffer when `resize()` runs, and push those bytes after `clear_scrollback` releases the bus lock. Same applies to bytes the kernel buffered pre-ioctl that `pump_reader` reads after the ioctl returns. | Best-effort residual. The shared `AgentBus::state` mutex prevents data races (push/clear/snapshot serialize through one lock), but cannot enforce the temporal ordering "all pre-resize bytes are pushed before clear runs" without holding the lock across a blocking `read()` (intractable). The interactive recovery makes this acceptable: the inner TUI's SIGWINCH-driven full-screen redraw at the new dims overwrites the parser's live screen within a frame, so any leaked pre-resize bytes age out of the live area into scrollback. Documented inline in `AgentPtyRegistry::resize`. |

## References

- `src/embedded_pane.rs:851-866` — placeholder `24, 80` parser init (replaced in M2)
- `src/embedded_pane.rs:714-877` — `hydrate_from_daemon` (consumes the new `AgentRecord` fields)
- `src/embedded_pane.rs:549-680` — `wire_stream_pane` (already takes `rows, cols`; nothing to change here)
- `src/agent_pty.rs:699-755` — `RunningAgent` (gains `rows/cols` in M1)
- `src/agent_pty.rs:765-800` — `AgentRecord` (gains optional `rows/cols` in M1)
- `src/agent_pty.rs:1181-1203` — `AgentPtyRegistry::resize` (updates stored dims and clears scrollback in M1/M3)
- `src/agent_pty.rs:586-624` — `AgentBus::push` and friends (snapshot ring cleared in M3)
- `src/daemon_protocol.rs:885-917` — `handle_attach_stream` (sends scrollback snapshot; unchanged structurally — the snapshot it sends now has correct provenance)
- `src/daemon_protocol.rs:215-310` — `AttachRequest`/`AttachResponse` (no protocol-version bump required; new field is optional)
- `prds/76-remote-agent-environments.md` (done) — origin of the M2.15 placeholder this PRD fixes
- `prds/84-rendering-layer-rework.md` — complementary rendering-contract work; does not overlap with this PRD
- `prds/77-tui-testing-harness.md` — testing harness used by M4 if it can drive detach/reattach
- `feedback_validate_pre_pr.md` — single pre-PR validation pass policy
