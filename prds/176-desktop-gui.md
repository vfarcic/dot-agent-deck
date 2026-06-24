# PRD #176: Desktop GUI app — alternative front-end to the TUI

**Status**: In Progress
**Priority**: Medium
**Created**: 2026-06-20
**GitHub Issue**: [#176](https://github.com/vfarcic/dot-agent-deck/issues/176)
**Related**: PRD #93 (always-external daemon — the daemon is the single source of truth this whole design depends on), PRD #76 (attach protocol / length-prefixed frames — the wire the GUI consumes), PRD #126 (agent-driven notifications — the daemon-side signal the GUI surfaces as OS-native notifications), PRD #174 (cross-project orchestration dispatch — the relationships the agents-graph must render and the source of the new structured events it needs), PRD #139 (the `experimental` feature flag — explained as N/A here, see Design Decisions)

## Problem Statement

The TUI is the **only** front-end to the daemon. That is not an architectural accident we need to fix — the daemon already owns 100% of the business logic (agent lifecycle, PTY ownership, hooks, orchestration dispatch, scheduling), the TUI is a pure client, and the wire protocol (`src/daemon_protocol.rs`, `src/daemon_client.rs`) is already consumed by three non-TUI clients today: the CLI daemon commands, the e2e tests, and the scheduler callback. The boundary is clean and client-agnostic. What is missing is a *second kind* of client.

A terminal front-end has a structural ceiling. Four experiences are hard-or-impossible in a character grid and get *more* valuable as the product leans into orchestration: (1) **visualizing the agent-to-agent communication graph** — delegate/work-done/dispatch relationships are a live DAG, exactly what ASCII can't render or animate, and PRD #174 makes that graph cross-project and bigger; (2) **OS-native ambient notifications** — agents run long and the human tabs away, so "agent X is waiting for input / finished" belongs in the OS notification center, not a terminal bell; (3) **rich content rendering** — diffs, markdown, file trees, images, cost/usage charts, all flattened to text today; (4) **free-form multi-agent layout** — draggable/resizable panes, a minimap, multi-monitor, none of which a single viewport offers.

This PRD does **not** claim a GUI replaces the TUI. The TUI keeps permanent advantages (SSH/tmux, near-zero install, keyboard-native, lives where developers already are), and the agent panes themselves are terminals either way (Claude Code is a TUI). The bet is narrower and honest: a GUI earns its keep in the *chrome around the terminals and the views terminals can't draw* — and the flagship proof of that is the agents-communication graph.

## Solution Overview

Build a desktop GUI as a **complementary, opt-in second client** of the existing daemon protocol. It reuses the daemon unchanged for everything the daemon already exposes; it requires daemon work **only** for the one richer view that needs new data (the graph — see M3.1).

Five ideas carry the design:

1. **The GUI is a fourth client, not a fork.** It holds no business logic. It connects to the same Unix-socket protocol the TUI uses, issues the same `AttachRequest` variants (`ListAgents`, `StartAgent`, `StopAgent`, `AttachStream`, `Resize`, `SetAgentLabel`, `WriteAndSubmit`, `SubscribeEvents`, `Hello`), and renders the results. "The daemon is the single source of truth" (PRD #93) is what makes a second front-end cheap.

2. **GUI-native chrome, terminal-native panes.** Decks, tabs, layout, focus, and the new views are GUI-native (HTML/CSS/JS). Agent panes stay embedded terminals rendered by **xterm.js** fed raw PTY bytes from `KIND_STREAM_OUT`. We are explicitly *not* chasing 1:1 implementation parity with the TUI — only conceptual parity for the shared concepts, plus net-new GUI-only views.

3. **Tauri.** The app is a Tauri app: the Rust **core** is a thin client that holds the `DaemonClient` and bridges the socket protocol into the webview; the webview renders the UI. Because the daemon and the core are both Rust, the core depends on the wire types **directly** (no duplicated/hand-maintained protocol). The webview brings the mature JS ecosystem we need for the chrome — a graph/DAG renderer and xterm.js.

4. **In-repo, as a Cargo workspace member, with the protocol extracted to its own crate.** The wire types move out of the TUI binary into a `protocol` workspace crate that the TUI, the CLI, the tests, and the GUI core all depend on. Co-locating means protocol changes (we will make some, for the graph) land atomically across daemon + all clients in one PR. The protocol crate is also the seam that makes a *future* repo split mechanical if the GUI ever develops an independent release cadence.

5. **The flagship is the agents-communication graph, and it is the one thing that is not free.** Today orchestration dispatch writes prompts directly into target PTYs and is not surfaced as structured events the protocol layer can subscribe to. So the graph requires the daemon to **emit structured `delegate` / `work-done` / `dispatch` events** on the event stream (M3.1). This is the real, non-obvious cost of the project and is scheduled early so we discover it before the chrome is polished against a dead end.

### Why the easy-sounding parts are easy, and the risky parts are risky

| Capability | Daemon work needed? | Risk |
|---|---|---|
| Decks/tabs/layout/focus, pane lifecycle, input | **None** — existing `AttachRequest` surface | Low |
| Embedded terminal panes (xterm.js) | None — raw `KIND_STREAM_OUT`/`KIND_STREAM_IN` | **Throughput over Tauri IPC** (M1.3) |
| Pane statuses / waiting-for-input | None — `SubscribeEvents` hook events | Low |
| **Agents-communication graph** | **Yes** — new structured orchestration events | **Highest** (M3.1) |
| OS-native notifications | Reuses PRD #126 daemon signals | Medium (per-OS) |

## User-facing behavior & documentation (documentation-first)

### Launching

The GUI is a separate, opt-in binary. A user who wants it builds/installs it deliberately; it is not part of the default release artifacts and does not replace `dot-agent-deck`. On launch it connects to the running daemon over the same socket the TUI uses, **auto-starting the daemon** per the existing always-external-daemon rules (PRD #93) when none is reachable — so launching the GUI alone is enough to get connected, exactly like the TUI. The daemon binary is located via `DOT_AGENT_DECK_BIN` (explicit override) → `dot-agent-deck` on `PATH` (the normal case) → a workspace dev build (`target/debug`/`target/release`, so `npm run dev` works from a checkout). The connect/retry state now appears only on a genuine failure (daemon binary not found, spawn failed, or the socket never appeared within the start budget).

### What it shows

- **Decks and tabs** as a GUI-native **top tab bar** (Design Decision #9), mirroring the TUI's Mode vs Orchestration bucketing from `AgentRecord.tab_membership`; the focused deck's panes list in the sidebar. Keyboard-navigable via a TUI-parity command mode, not click-only.
- **Agent panes** as real terminals (xterm.js): full scrollback, mouse, copy/paste, truecolor, resize. Keystrokes route to the focused pane via `KIND_STREAM_IN`.
- **Pane status** (running / waiting-for-input / finished) driven by the hook-event stream, shown as GUI affordances (badges, color, a "needs you" cue) rather than a terminal bell.
- **Agents-communication graph** (the flagship): a live node-graph where nodes are agents and edges are delegate/work-done/dispatch relationships, with status-colored nodes, edges that light when a hand-off fires, and click-a-node-to-focus-its-pane. Cross-project dispatch edges (PRD #174) appear here once #174's events are structured.
- **OS-native notifications** when an agent needs input or completes, so the human can be tabbed away.

### What it deliberately does NOT do

It does not re-implement the TUI's rendering, does not hold orchestration logic, and does not aim to be runnable over SSH/tmux — that is what the TUI is for. The two front-ends share the daemon and the *concepts*, not the code.

## Scope

### In Scope

- A `protocol` Cargo workspace crate: the wire types (`AttachRequest`, `AttachResponse`, frame kinds/codecs, `AgentRecord`, event structs) extracted from the current binary so the TUI, CLI, tests, and GUI core all share one definition. No behavior change to the existing clients; the existing round-trip tests move with it and stay green.
- A Tauri app as a workspace member: Rust core that connects to the daemon socket via the `protocol` crate, performs the `Hello` handshake/version negotiation, and bridges frames to the webview; web frontend scaffolding (build tooling contained entirely in the GUI subdirectory).
- Embedded terminal panes: `AttachStream` → xterm.js, bidirectional, with resize coalescing (single-slot, latest-wins, mirroring `embedded_pane.rs`) and a deliberate **throughput stress test** for many busy panes (M1.3).
- GUI-native chrome: decks, tabs, multi-pane layout, focus, and pane lifecycle from the GUI (`ListAgents`, `StartAgent`, `StopAgent`, `SetAgentLabel`, `Resize`, `WriteAndSubmit`).
- Pane-status surface driven by `SubscribeEvents` hook events.
- **Daemon change (graph data):** emit structured `delegate` / `work-done` / `dispatch` events on the event stream so any client can reconstruct the communication graph; the TUI may ignore them. Protocol-versioned and additive.
- Agents-communication graph view consuming those events: nodes, edges, live status, click-to-focus.
- OS-native notifications wired to PRD #126's agent-driven notification signals.
- Opt-in packaging: a separate build target / artifact, not bundled into the default release, unadvertised until proven.
- Tests appropriate to the new surfaces: Rust tests for the `protocol` crate (the migrated round-trip tests) and the core's bridge/handshake; lightweight web component/e2e tests for the chrome and one terminal round-trip; the daemon event-emission change covered by daemon-side tests.
- Docs: a developer doc under `docs/develop/` for building/running the GUI and its toolchain (linked from `CONTRIBUTING.md`), and a user doc once it is past spike quality.

### Out of Scope / Non-Goals

- **Replacing the TUI.** The TUI remains the primary, SSH/tmux-capable front-end. This is additive.
- **1:1 implementation parity.** Only conceptual parity for shared concepts plus net-new GUI-only views. Feature-by-feature mirroring is an explicit non-goal (it is the thing that makes a second front-end a permanent tax).
- **Non-terminal agent panes / re-rendering agent output as structured GUI.** Panes stay terminals; the agent-interaction surface is intentionally frozen at terminal fidelity for v1.
- **Mobile/web-hosted (browser) build.** Tauri desktop only for v1; a browser-served variant is a deferred follow-up.
- **Native code-signing/notarization/auto-update pipeline.** Out for v1 (opt-in local build); revisit if/when the GUI graduates to public distribution.
- **The `experimental` feature flag (PRD #139).** N/A by construction — see Design Decisions #6. A separate opt-in binary is inherently the opt-in; there is no TUI render/input seam to gate, so no `features.rs` wrapper and no `graduate-` follow-up issue.

## Design Decisions

1. **Reuse the daemon as the single source of truth; the GUI holds no logic.** The entire feasibility of a cheap second front-end rests on PRD #93's "daemon owns everything" model, already proven by three non-TUI clients. The GUI adds a fourth consumer of an existing, tested contract — not a parallel brain.

2. **Tauri over Electron/native.** Tauri's core is Rust, so it depends on our `protocol` crate directly (no foreign runtime, no duplicated wire types), and ships far smaller than Electron (~3–10 MB vs ~100 MB+). The webview gives us a mature graph renderer and xterm.js — the two JS pieces we actually need. Native Rust GUI (egui/iced) was rejected: its embedded-terminal story is immature and would mean hand-rolling the one widget xterm.js gives us for free. The accepted cost is webview inconsistency across platforms (WebKitGTK on Linux is the weakest, and we develop on Linux) and Tauri IPC throughput for the terminal byte-stream — both are tested early (M1.3), not assumed.

3. **Panes stay terminals (xterm.js), chrome goes GUI-native.** This splits the work along the seam where each technology is strongest and avoids re-rendering agent output. It also makes the honest trade explicit: fidelity quirks (escape sequences, truecolor, mouse) improve over the `vt100` crate, but daemon-side quirks (PTY sizing, the snapshot-then-stream attach, the agent's own width assumptions) are below the protocol boundary and survive unchanged — so "terminals will fix our pane quirks" is only partly true and is not the justification.

4. **In-repo workspace + extracted `protocol` crate.** Co-located because the daemon and GUI will change together during the graph work (atomic protocol bumps in one PR); the `protocol` crate is the seam that keeps a future repo split mechanical. We split the repo only if/when the GUI develops an independent release cadence (desktop signing/notarization on its own schedule) and a distinct contributor pool — not before, because splitting before the protocol stabilizes is the expensive mistake.

5. **The flagship graph drives the only required daemon change, and it is scheduled early.** Orchestration relationships are not currently structured events. Emitting `delegate`/`work-done`/`dispatch` events (additive, version-negotiated) is the one piece of real backend work; doing it in Phase 3 — before the chrome is polished — surfaces the cost early rather than after sunk investment. This is the de-risking the "full build" scope folds in instead of a hard go/no-go gate.

6. **The `experimental` flag (PRD #139) does not apply.** That flag is a *presentation switch* that gates render/input seams inside the **TUI binary** (`features::show_<feature>()`). A separate GUI binary has no such seam — the act of building/running it is the opt-in. So maturity is handled by *packaging* (kept out of default artifacts, unadvertised) rather than a runtime flag. (Recorded per CLAUDE.md permanent instruction #9; the user chose "inherently opt-in".)

7. **Conceptual parity, not implementation parity.** Shared concepts (decks, tabs, panes, focus) must feel coherent across both front-ends, but their code is independent and the GUI is free to diverge where GUI-native affordances are better. Chasing literal parity is the named non-goal because it is what converts a complementary app into a permanent two-front-end maintenance burden.

8. **GUI test suite (web e2e + throughput stress) deferred to a follow-up task; the build is validated by hand until then.** Per the maintainer's call (2026-06-22), the initial build prioritizes a launchable, visibly-working GUI over its automated test layer. The web component/e2e tests for the chrome (M2.1/M2.2), the M1.3 **throughput stress harness**, and the one terminal round-trip are deferred to a dedicated follow-up testing task (see M5.2). The Rust-side tests already in place remain authoritative and stay in `cargo test-fast`: the protocol crate's migrated round-trip tests (M1.1) and the GUI core's connect/`Hello`/bridge integration test (M1.2). The Tauri build toolchain (WebKitGTK and friends) is provisioned **cross-platform via `devbox.json` nix packages** — not OS-specific `apt` commands — because the maintainer develops on both macOS (system WebView, no extra libs) and Linux (webkitgtk).

9. **Decks/tabs live in a top tab bar, and keyboard navigation mirrors the TUI via a command mode.** Decided with the maintainer (2026-06-23). The GUI *will* be keyboard-navigable, not click-only, and it keeps the TUI's bindings (from `src/keybindings.rs`): `Ctrl+d` enters command mode, then `h`/`l` or `←`/`→` move between decks (tabs), `j`/`k` or `↑`/`↓` move between panes, `Enter` focuses the selected pane, `Esc` leaves command mode. Because moving between decks is a *horizontal* motion (`←`/`→`), the decks must render as a **horizontal top tab bar** for the spatial metaphor to hold — a vertical sidebar driven by left/right would force remapping to `↑`/`↓` and break keybinding parity. This is *why* tabs go on top rather than in the sidebar (which the M2.1 first cut used). The load-bearing GUI-native detail: each pane is a real xterm.js terminal that otherwise captures every keystroke, so the leader key is intercepted in a capture-phase `keydown` handler **before** xterm.js sees it — the same reason the TUI reserves `Ctrl+d`. **Shortcuts must be byte-for-byte identical across TUI and GUI** (maintainer, 2026-06-24): switching front-ends should never re-train muscle memory. That is now enforced *by construction* — the keybinding model is the shared `keybindings` workspace crate (extracted 2026-06-24), so the GUI core resolves bindings from the SAME source as the TUI (the user's `keybindings.toml` + identical defaults), honoring user remaps, rather than hand-mirroring a hardcoded copy. Concretely this means **digit `1–9` → pane** (the TUI's `FocusCard`), *not* digit → deck — an earlier digit-→-deck idea was dropped because it would diverge.

## Success Criteria

- The GUI connects to the running daemon over the existing socket, completes the `Hello` handshake, lists agents, and renders the same decks/tabs the TUI shows — driven entirely by existing protocol responses, with **zero** daemon changes for this path.
- An agent pane in the GUI is a fully interactive terminal: scrollback, truecolor, mouse, copy/paste, and resize all work; keystrokes reach the agent and output streams back, verified against a real agent (e.g. Claude Code) running in the pane.
- **Throughput holds under load:** with several agents producing high-volume output simultaneously, the GUI stays responsive and lossless (the terminal-in-webview-over-Tauri-IPC risk is measured and passes a defined bar, not assumed) — M1.3.
- The `protocol` crate extraction leaves the TUI, CLI, and existing tests behaviorally unchanged; all migrated round-trip tests pass.
- The daemon emits structured `delegate`/`work-done`/`dispatch` events; an old TUI ignores them (forward-compat), and the GUI reconstructs a live agents-communication graph whose nodes/edges match an orchestration actually run across panes (and across projects per #174).
- Clicking a node in the graph focuses that agent's pane; a hand-off animates an edge in real time.
- The GUI raises an OS-native notification when an agent needs input or finishes while the window is unfocused.
- Maturity is enforced by packaging: the GUI is a separate artifact, absent from the default release, and documented as opt-in/preview.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` pass for the Rust crates; the GUI's own test suite passes; `cargo test-e2e` passes before the PR (CLAUDE.md rules 2 & 5).

## Milestones

### Phase 1 — Foundations & the throughput risk

- [x] **M1.1** — Extract a `protocol` Cargo workspace crate from the current binary (wire types, frame codecs, `AgentRecord`, event structs). TUI/CLI/tests depend on it; migrated round-trip tests stay green; no behavior change. **Done** — `crates/protocol`; wire byte-identical (`PROTOCOL_VERSION` unchanged at 3, no `.breaking.md`); 44 migrated round-trip tests green via re-export shims.
- [x] **M1.2** — Tauri app skeleton as a workspace member: Rust core connects to the daemon socket via `protocol`, performs `Hello` version negotiation, and bridges frames to the webview. JS toolchain contained in the GUI subdirectory. **Done** — `gui/core` (`dad-gui-core`, testable connect/`Hello`/bridge lib, workspace member) + `gui/src-tauri` (`dad-gui`, thin Tauri v2 shell, workspace-excluded so missing webview libs can't break Rust gates) + `gui/dist` vanilla frontend; socket discovery centralized in `protocol::socket` (TUI delegates to it); connect/`Hello`/bridge integration test green in `test-fast`. **Daemon auto-start on launch** (commit `81d7c21`): `connect_or_autostart` mirrors the TUI's lazy-spawn bootstrap (`src/daemon_attach.rs`) so launching the GUI alone brings the daemon up — new `gui/core/src/daemon.rs` does the `flock`-serialized spawn + 50 ms/5 s socket poll + `verify_socket_trusted`, resolving the binary `DOT_AGENT_DECK_BIN` → `PATH` → workspace `target/{debug,release}`; gates on `PROTOCOL_VERSION` only (tolerates a different daemon *build*, no wire change, no `.breaking.md`); 7 new `gui/core` unit tests, `fmt`/`clippy`/`test-fast` green.
- [x] **M1.3** — Single embedded terminal pane: `AttachStream` → xterm.js round-trip, bidirectional, with single-slot resize coalescing **and a defined throughput stress test** (multiple busy panes; measure responsiveness/loss over Tauri IPC). This is the load-bearing feasibility check. **Done (build + hand-validation)** — pane built in commit `8f1e003` and validated by hand by the maintainer (live xterm.js round-trip, keystrokes → `KIND_STREAM_IN`, single-slot latest-wins resize). _The **throughput stress test stays deferred** to the follow-up GUI test task (M5.2, Design Decision #8) — not part of this hand-validated slice._

### Phase 2 — GUI-native chrome

- [ ] **M2.1** — Decks, tabs, and multi-pane layout from `ListAgents`/`AgentRecord` (Mode vs Orchestration buckets); focus and keyboard routing to the focused pane. _Implemented, pending hand-validation: the shell projects `tab_membership` into a bucket (`mode`/`orchestration`/`dashboard`) + tab name + role index/name (`AgentSummary`); the frontend renders the decks as a **top tab bar** (Design Decision #9), the focused deck's panes in the sidebar, and one focused terminal. **Keyboard navigation mirrors the TUI** via a command mode (`Ctrl+d` leader → `h`/`l`/`←`/`→` decks, `j`/`k`/`↑`/`↓` panes, `Enter` focus, `Esc` exit), with the leader intercepted in a capture-phase handler before xterm.js. Per the maintainer's parity-first working agreement (2026-06-23), a **simultaneous multi-visible-pane layout** (grid/splits/draggable) is treated as GUI-native net-new and **deferred to later** — the parity model here is "decks/tabs + one focused pane," like the TUI._
- [ ] **M2.2** — Pane lifecycle from the GUI: `StartAgent`, `StopAgent`, `SetAgentLabel`, `Resize`, `WriteAndSubmit`; pane-status surface (running / waiting-for-input / finished) driven by `SubscribeEvents`. _Current state (spike): the frontend lists agents via a one-shot `agents` call with a **manual Refresh button** as a stand-in; the live, event-driven agent-list + status updates (no manual reload) land here once the GUI subscribes to `SubscribeEvents` — the daemon already emits these, the GUI just doesn't consume them yet._

### Phase 3 — Flagship: agents-communication graph (the one daemon change)

- [ ] **M3.1** — Daemon emits structured `delegate`/`work-done`/`dispatch` events on the event stream (additive, version-negotiated; TUI ignores them). Daemon-side tests; forward/backward-compat tests.
- [ ] **M3.2** — Graph view consuming those events: nodes=agents, edges=relationships, live status colors, edge animation on hand-off, click-node-to-focus-pane; cross-project edges rendered per #174.

### Phase 4 — Ambient affordances

- [ ] **M4.1** — OS-native notifications wired to PRD #126's agent-driven notification signals (needs-input / finished while unfocused), with the obvious de-dupe/quiet-when-focused behavior.

### Phase 5 — Packaging, tests, docs & release gate

- [ ] **M5.1** — Opt-in packaging: a separate build target/artifact, excluded from the default release, labeled preview/opt-in.
- [ ] **M5.2** — Tests: `protocol`-crate and core bridge/handshake (Rust); lightweight web component/e2e for chrome + one terminal round-trip; daemon event-emission coverage. **(The web component/e2e + terminal round-trip + M1.3 throughput stress are the deferred follow-up testing task — Design Decision #8; the Rust `protocol`/core tests already landed in M1.1/M1.2.)**
- [ ] **M5.3** — Docs: developer build/run + toolchain doc under `docs/develop/` (linked from `CONTRIBUTING.md`); user doc once past spike quality; changelog fragment via `dot-ai-changelog-fragment`.
- [ ] **M5.4** — Pre-PR gate: `cargo test-e2e` green; review (Greptile) settled per CLAUDE.md rule 8.

## TUI → GUI feature parity (living checklist)

This is the answer to "how do we know we've ported everything we should?" — every user-facing TUI capability, its GUI status, and the deliberate non-ports. It is a **living checklist**, updated as slices land. Derived from the TUI's `Action` set (`src/keybindings.rs`) plus the broader feature areas. Status legend: ✅ ported · 🔲 pending · 🚫 not porting (TUI-only by nature) · ➕ GUI-native-only (net-new, not a TUI feature, sequenced later). Rows marked **confirm** are divergence/scoping calls for the maintainer (per Design Decision #7 and the 2026-06-23 parity-first working agreement); the rest follow from parity.

**Two navigation axes (clarifies the digit-jump rows):** a deck/tab usually holds *several* panes (orchestration role panes, Mode panes, the multi-card Dashboard), so "deck N" ≠ "pane N" — a digit can only index one axis. The TUI uses digits `1–9` for **panes** (`FocusCard`) and `h`/`l` (+ `←`/`→`) for **decks**. The GUI matches that **exactly** (identical-shortcuts decision, 2026-06-24): **digit → pane**, decks on `h`/`l`/`←`/`→`. (An earlier digit-→-deck idea was dropped — it would have diverged from the TUI.)

### Chrome & navigation
| TUI capability | GUI status | Notes |
|---|---|---|
| Decks/tabs (Mode vs Orchestration) | ✅ M2.1 | top tab bar (Design Decision #9) |
| Dashboard is the leftmost tab | ✅ | parity fix landed (`BUCKET_ORDER` = dashboard first) |
| Command mode (`Ctrl+d` leader) | ✅ M2.1 | capture-phase intercept before xterm.js |
| Move between decks (`h`/`l`, `←`/`→`) | ✅ M2.1 | |
| Move between panes (`j`/`k`, `↑`/`↓`) | ✅ M2.1 | |
| Focus pane (`Enter`) | ✅ M2.1 | |
| Jump by number (`1`–`9`) | 🔲 pending | digit → **pane** (`FocusCard`, exact TUI parity); resolved from the shared `keybindings` crate |
| Bindings resolved from shared `keybindings` crate | ✅ extracted / 🔲 GUI wiring | crate landed (TUI + GUI share one source); GUI core consuming it is the next slice |
| New pane (`Ctrl+n` / `StartAgent`) | 🔲 M2.2 | |
| Close pane (`Ctrl+w` / `StopAgent`) | 🔲 M2.2 | |
| Rename pane (`r` / `SetAgentLabel`) | 🔲 M2.2 | |
| Filter + clear (`/`, `Esc`) | 🔲 pending | sidebar filter box |
| Help overlay (`?`) | 🔲 pending | keybinding cheatsheet panel |
| Toggle layout (`Ctrl+t`) | 🚫 **confirm** | TUI-specific pane layout toggle; GUI layout differs — likely a GUI-native equivalent, not a 1:1 port |
| Generate config (`GenerateConfig`) | 🚫 **confirm** | TUI setup flow; GUI-N/A? |
| Open scheduled tasks | 🔲 pending | scheduling view |
| Approve / deny permission (`y`/`n`) | 🔲 pending | permission-prompt surface |

### Panes / terminals
| TUI capability | GUI status | Notes |
|---|---|---|
| Embedded terminal pane (live PTY) | ✅ M1.3 | xterm.js |
| Scrollback / truecolor / mouse / copy-paste / resize | ✅ M1.3 | |
| Pane status (running / waiting-for-input / finished) | 🔲 M2.2 | via `SubscribeEvents` (also retires the manual Refresh) |
| Send prompt (`WriteAndSubmit`) | 🔲 M2.2 | |

### Lifecycle / connection
| TUI capability | GUI status | Notes |
|---|---|---|
| Daemon auto-start on launch | ✅ | mirrors PRD #93 |
| Connect / retry state | ✅ M1.2 | |
| Reconnect / hydrate agents on connect | ✅ partial | lists on connect; live updates pending (M2.2) |

### Deliberate non-ports (TUI-only by nature)
| TUI capability | GUI status | Notes |
|---|---|---|
| SSH/tmux remote operation; `connect` / `remote` CLI | 🚫 | GUI is desktop-only (Non-Goals) |
| Quit via `Ctrl+C` modal | 🚫 → adapt | window close / app-quit instead |

### GUI-native only (net-new, not TUI parity; sequenced later)
| Capability | Status | Notes |
|---|---|---|
| Agents-communication graph | ➕ Phase 3 | the flagship (M3.1/M3.2) |
| OS-native notifications | ➕ Phase 4 | adapts PRD #126's bell signal |
| App zoom / font size (`Ctrl` `+`/`−`/`0`) | ➕ pending | no TUI analog (terminal-emulator's job there); confirmed wanted 2026-06-24 |
| Collapsible sidebar deck groups | ➕ optional | small frontend follow-up |

## Risks & Mitigations

- **Terminal-in-webview throughput over Tauri IPC (highest technical risk).** Many busy panes streaming raw PTY bytes across the IPC bridge could lag or drop. Mitigation: M1.3 stress-tests this first, before any chrome polish; use Tauri's fast channel/raw-payload path and xterm.js WebGL/canvas rendering; if it fails the bar, the terminal-in-webview approach is reconsidered while sunk cost is still small.
- **The graph needs daemon work that "looks free" but isn't.** Orchestration relationships aren't structured events today. Mitigation: M3.1 makes this explicit and scheduled; the change is additive and version-negotiated so it can't break the TUI.
- **WebKitGTK (Linux) is the weakest webview and we develop on Linux.** Rendering/feature/perf gaps land exactly where we work. Mitigation: test on Linux first, not last; keep the frontend within broadly-supported web features; treat macOS/Windows as validation targets, not assumptions.
- **"Embedding terminals removes our pane quirks" is only partly true.** Fidelity quirks improve; daemon-side sizing/attach quirks survive; new webview (DPI/font/reflow) quirks appear. Mitigation: name the three buckets up front (Design Decision #3) so expectations are calibrated and daemon-side issues are fixed daemon-side, not chased in the GUI.
- **Two front-ends diverge conceptually.** Decks/tabs/focus now exist twice. Mitigation: conceptual-parity-not-implementation-parity is an explicit decision; shared *concepts* are documented; the GUI is allowed to diverge only toward GUI-native betterment, not arbitrary difference.
- **JS toolchain pollutes a Rust-centric repo and its gates.** Mitigation: contain all JS build tooling in the GUI subdirectory; keep `cargo fmt`/`clippy`/`nextest` gates authoritative for the Rust crates; add a separate, non-blocking-to-Rust GUI test job.
- **Scope creep dressed up as "richer."** "We'll find richness later" is how a complementary app becomes an unbounded second product. Mitigation: the graph is the single flagship that justifies the build; everything else (notifications, rich rendering, dashboards) is upside, added only after the graph proves the thesis.
- **Maturity leakage.** An unproven GUI shipped in default artifacts would set expectations it can't meet. Mitigation: packaging-level opt-in (separate artifact, unadvertised) instead of a runtime flag, since a separate binary has no TUI seam to gate.
