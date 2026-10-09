# PRD #1487: Upgrade daemons from any client with one shared implementation

**Status**: Complete
**Priority**: Medium
**Created**: 2026-10-02
**Issue**: [#1487](https://github.com/vfarcic/dot-agent-deck/issues/1487)
**Builds on**: [PRD #161](https://github.com/vfarcic/dot-agent-deck/issues/161) (the `connect` upgrade prompt and the build-version handshake), [#1049](https://github.com/vfarcic/dot-agent-deck/issues/1049) (`StopDaemon`, the first daemon lifecycle verb on the wire), [#1372](https://github.com/vfarcic/dot-agent-deck/issues/1372) (upgrading a Homebrew-installed remote through `brew`), [#1350](https://github.com/vfarcic/dot-agent-deck/issues/1350) (one deck list, `remotes.toml`, shared by the CLI and the desktop app)
**Related**: [#1472](https://github.com/vfarcic/dot-agent-deck/issues/1472) (Connect anyway — the escape the desktop offers today instead of an upgrade)

## Problem Statement

**Only two places can upgrade a remote daemon, neither restarts it, and the desktop app cannot do it at all.**

- `dot-agent-deck remote upgrade <name>` (CLI) and the TUI's `dot-agent-deck connect` prompt both call `remote::upgrade()` in `src/remote.rs`. That function is **binary-swap-only by contract** (`src/connect.rs`, the `RemoteUpgrader` doc: it "MUST NOT restart the remote daemon or kill agents"). After it succeeds, the remote keeps running the **old** daemon until something restarts it; for `connect`, that something is the build-version handshake on attach, which restarts an idle daemon and prompts when agents are live.
- The desktop app holds many daemons at once and already links the root crate (`desktop/src-tauri/src/decks.rs` uses `dot_agent_deck::remote` for the shared deck list), but offers no upgrade. Its **Replace daemon** is local-only (`require_local("Replace daemon")` in `desktop/src-tauri/src/lib.rs`), shown only behind the experimental flag, and refused while agents run. A user whose remote daemons are older than the app — the situation every release with a `.breaking.md` fragment creates, such as v0.45.0's `505-unsolicited-work-done-label-reworded` — gets a refusal and **Connect anyway**, and has to leave the app for a terminal to fix it.

**The deeper problem is where the decision lives.** "Install the new build, then decide what happens to the running daemon and its agents, then restart it" is one procedure, but today half of it is a CLI command and the other half is a TUI attach-time handshake. Adding a desktop button the obvious way would write that policy a third time, and three copies of a policy about stopping somebody's agents will disagree. The maintainer's requirement, stated while scoping this: **the whole upgrade procedure is one implementation that every client calls — no client carries its own upgrade policy.**

## Solution Overview

Split the procedure by who owns each half, and join the halves in one shared function:

1. **Install — shared library, run from the client machine.** `remote::upgrade()` stays the one installer. It is an SSH operation from where the user is: it reads the user's `remotes.toml`, uses the user's SSH keys and agent, installs the client's own version (through `brew` on a Homebrew remote, #1372), and refreshes the remote's hooks. It cannot move into a daemon: the remote daemon cannot safely replace itself mid-install, a local daemon may not exist (the desktop app can be connected only to remotes), and no daemon holds the user's SSH credentials.
2. **Restart — a daemon request.** The running daemon is the only component that knows which agents and orchestration roles it holds and what a restart would destroy (CLAUDE.md rule 15's `daemon stop` refusal already lives there). So the remote daemon gains one request: restart into the binary now installed at its own path. The daemon applies the restart policy itself and answers with what it did or why it did not. Per CLAUDE.md rule 18 this is an additive request **gated on a capability string**, so it needs no `PROTOCOL_VERSION` bump, and every sender checks the capability in the client library, not at each call site.
3. **One orchestrating function.** A single shared function runs install → ask the daemon to restart → returns a **structured result**: upgraded and restarted; installed but not restarted, with the reason and, when agents are running, which ones; installed, and this daemon is too old to restart itself; or a failure in plain words. The CLI prints it; the TUI and the desktop app render it. `connect`'s existing upgrade prompt is rebuilt on the same function, so the TUI path and the desktop path become the same code.

## What the user sees

- **Desktop app:** a daemon whose build is older than the app shows an **Upgrade** action on its card (Dashboard and Daemons screen), beside or replacing today's version-mismatch guidance. Pressing it shows progress (installing, restarting), then the outcome in plain words. When agents are running, the user sees which agents a restart would stop and chooses what happens, per the policy this PRD settles.
- **CLI:** `dot-agent-deck remote upgrade <name>` installs **and** restarts (or reports why not), instead of only installing. Its output is the same result the apps render.
- **TUI:** `connect`'s upgrade prompt behaves identically to the desktop's action, because it calls the same function; and the TUI offers the same upgrade from its own daemon views, or this PRD records why the prompt is enough (CLAUDE.md rule 22).

## Scope

### In scope

- One orchestrating function in the root crate that every client calls, returning a structured result.
- A capability-gated daemon request to restart into the newly installed binary, applying the restart policy daemon-side.
- The desktop **Upgrade** action for remote daemons, with progress, outcome and failure states.
- The CLI's `remote upgrade` and the TUI's `connect` prompt moved onto the shared function.
- TUI parity per rule 22, and docs for both clients.

### Out of scope

- Upgrading the **desktop app** itself (the bundle, the macOS `.dmg`, auto-update).
- Choosing a version other than the client's own (downgrades, pinning a remote to an older release).
- Upgrading every daemon at once ("upgrade all") — a natural follow-up once one upgrade is solid; listed in Open Questions.
- The local daemon's **Replace daemon**: whether it moves onto the same function is decided in M1, but redesigning it is not the goal.

## Design decisions and constraints

- **D1 — One implementation (maintainer, 2026-10-02).** No client carries upgrade policy. A client decides only how to *present* a step or a question; what to *decide* is in the shared function or the daemon.
- **D2 — Install is client-side, restart is daemon-side.** For the reasons in the Solution Overview. The restart request must work for both install methods `remote::upgrade()` knows: the `~/.local/bin` download and a Homebrew install (#1372), so "the binary now installed" is resolved by the daemon from its own install record, not passed in by a client.
- **D3 — The restart request is additive and capability-gated (rule 18, rule 12).** A client that finds no capability falls back honestly: it reports "installed; this daemon is too old to restart itself", says what the user can do, and never sends the request blind. `StopDaemon` (#1049) is the counter-example to avoid: it has a capability string but its sender does not consult it. Rule 12's cross-version check is owed: an older daemon receiving the new client must still work, and the new daemon must still serve an older TUI.
- **D4 — Never strand the user** (carried from PRD #161's D4). A failed install or a refused restart leaves the user connected to the daemon they had, with the reason on screen.
- **D5 — The restart policy for live agents is the daemon's** and is decided in M1 (Open Question 1). Whatever it is, the daemon names every agent and orchestration role a restart would stop before stopping any, and never discards orchestration role maps silently (rule 15).

## M1 decisions (maintainer, 2026-10-02)

- **D6 — The upgrade combines `remote upgrade` and `connect` (OQ1).** Today the two halves exist in two commands: `remote upgrade` installs only (binary + hooks + `remotes.toml`), and `connect` offers that install (`Upgrade and connect? [y/N]`) and then, on attach, the build-version handshake restarts an idle daemon silently or lists the live agents and asks (`s` restarts and stops them, any other key keeps the current daemon). The shared function is exactly that combination, and it keeps `connect`'s existing policy rather than inventing one:
  - **Install** as `remote upgrade` does.
  - **No live agents or roles:** restart onto the new build without asking.
  - **Live agents or orchestration roles:** the daemon names every agent and role a restart would stop; the user chooses **Restart now** (stops exactly those) or **Keep current daemon** (new binary installed, old daemon still running, and the client says so). Orchestration roles follow the same choice — named, never silent — rather than a hard refusal.
  - **No one to answer** (CLI without a TTY): install and keep the current daemon, as `connect` does non-interactively today.
  - What moves is only the mechanism: the restart becomes a capability-gated daemon request (D3) instead of the remote TUI terminating the daemon from outside, so a client with no remote TUI (the desktop) can trigger it. The "Restart now" choice is sent with the named set, and the daemon refuses again if the set it holds no longer matches.
- **D7 — One result type (OQ2).** One serde-serializable outcome type in the root crate — restarted (from → to); installed but not restarted (with the agents/roles that blocked it, or that the user kept it); installed, but the daemon is too old to restart itself; failed (stage + plain-language reason) — plus stage progress events (installing, restarting, verifying). The CLI prints it, the TUI and desktop render it.
- **D8 — When the action is offered.** Only when the daemon's version is **older** than the client's — the same newer-only rule `connect` uses (`laptop_is_newer`). Hidden when the versions are equal (a differing build stamp at the same release is not a mismatch), when the daemon is newer than the client (the card may say so; upgrading the app is out of scope), and when the daemon's version is unknown.
- **D9 — Surfaces (OQ3).** Desktop: an **Upgrade** action on a remote daemon's card on the Dashboard and the Daemons screen, and in the version-mismatch banner in place of a dead end. CLI: `remote upgrade` gains D6's restart offer as a terminal prompt. TUI: the `connect` prompt only, rebuilt on the shared function — the TUI has no other per-remote daemon view to put an action in, so the prompt is the parity surface (rule 22). "Upgrade all" is a follow-up issue, not this PRD.
- **D10 — Local Replace daemon (OQ4).** Moves onto the same shared path, so it gains the same live-agent policy.
- **D11 — Version (OQ5).** Always the client's own version, as `connect` does. No "latest release" offer.
- **D12 — Experimental flag (rule 9).** No. The action ships visible by default: it replaces the refusal-plus-**Connect anyway** dead end users hit on every release with a breaking fragment.

## Milestones

- [x] **M1 — Decisions recorded.** Open Questions 1–4 answered with the maintainer and written into this document: the restart policy for live agents, the result shape, the desktop and TUI surfaces, and whether the local Replace daemon joins the shared path. The rule 9 experimental-flag answer for the new desktop surface recorded.
- [x] **M2 — The daemon restart request.** Capability-gated request on the daemon that restarts into its newly installed binary under the M1 policy, with its structured answer; client-library helper that checks the capability before sending. Rule 12 answered explicitly and the cross-version check run (`cargo xver`, both directions). **Done:** capability-gated `restart-daemon`, sent only by `DaemonClient::restart_daemon`; no `PROTOCOL_VERSION` bump; `cargo xver` forward and reverse pass.
- [x] **M3 — The shared orchestrating function.** Install → restart request → structured result, covering both install methods and the too-old-daemon fallback; unit tests over a fake SSH executor and a fake daemon for every outcome. **Done:** one orchestrating function covering both install methods and the too-old-daemon fallback, tested over a fake SSH executor and a fake daemon.
- [x] **M4 — CLI and TUI on the shared function.** `remote upgrade` and `connect`'s upgrade prompt both call it; the binary-swap-only contract in `src/connect.rs` is replaced by the new one; TUI parity per rule 22 delivered or its absence justified in this document. **Done:** `remote upgrade` and `connect` call the shared function; the binary-swap-only contract is replaced.
- [x] **M5 — Desktop Upgrade action.** The action on a remote daemon's card, progress, outcome, live-agent choice and plain-language failures (rule 21), every button doing something visible (the #1472 lesson); vitest and Playwright coverage. **Done:** Upgrade action on cards, the Daemons screen and the mismatch banner; vitest and Playwright `upgrade.spec.ts`.
- [x] **M6 — End-to-end coverage.** A PTY-attached L2 test that upgrades a daemon and sees it restarted on the new build, a test with live agents exercising the M1 policy, and a test against an older daemon without the capability (rule 4). **Done:** lane-1 files plus the real-Haiku lane-2 test `remote/upgrade/008`.
- [x] **M7 — Docs.** User docs for both clients (rules 21 and 22, the docs-screenshots-review skill, screenshots of the desktop action) and developer docs for the request and the shared function; changelog fragments as users see the change (rule 19). **Done:** user docs for both clients and developer docs; changelog fragments `1487.feature.md` and `1487.bugfix.md`.

## Delivered

All of M1–M7 shipped in one PR. `remote upgrade` installs the matching build on a remote and restarts its daemon; `connect` uses the same function; the desktop has an Upgrade action on daemon cards, the Daemons screen and the mismatch banner; and the local **Replace daemon** goes through the same path. A hook-config fix was folded in at the maintainer's request after a test run polluted the operator's real `~/.codex/hooks.json`: test-write containment is armed by default under nextest, there is one deck entry per event, a no-op is not a write, custom cargo target directories are detected, and automatic installs keep a live installed entry (an explicit `hooks install` replaces it).

Decisions taken during the run:

- Replace daemon has no experimental gate and drops its zero-agents condition.
- Local Replace uses `ClientSpawns`.
- An older local daemon without the capability falls back to the PID stop, after asking.
- The remote restart goes over SSH.
- Idle with no TTY restarts without asking.
- Supervised daemons exit 75 so their service manager restarts them.

Deferred (D9): upgrading all daemons at once, tracked in [#1600](https://github.com/vfarcic/dot-agent-deck/issues/1600).

## Risks

- **Restarting a daemon stops its agents.** That is the whole reason the restart belongs to the daemon and the policy is settled first; a mistake here destroys someone's running work (rule 15's history).
- **A daemon restarting into a binary it cannot run** — wrong architecture, a half-written file, a Homebrew upgrade that failed after the old binary was unlinked. The restart must verify the new binary answers before handing over, or keep the old one running.
- **Two clients upgrading the same daemon at once** (the desktop and a TUI, or two desktops). The daemon must serialise restart requests and the installer must tolerate a concurrent install.
- **SSH reachability differs from the daemon link.** The desktop may reach a daemon through a tunnel whose SSH target the user's `remotes.toml` describes differently; the install must use the same route the deck list records.

## Open questions

1. **Restart policy with live agents.** Refuse and list what would stop; wait until the daemon is idle, then restart; drain (finish running turns, then restart); or let the user choose in the moment. And for orchestration roles specifically, whether a restart is ever allowed while roles are live.
2. **The result shape** the CLI prints and the apps render — one type in the root crate, so the three clients cannot drift.
3. **Surfaces.** Where the desktop action lives (card, Daemons screen, the refusal banner), whether the TUI gets an action in its daemon views beyond the `connect` prompt, and whether "upgrade all" belongs in this PRD or a follow-up.
4. **The local daemon.** Whether the desktop's local **Replace daemon** moves onto the same request (it would gain the same live-agent policy) or stays separate.
5. **Version selection.** The client's own version is the default (as `connect` does); whether the desktop should instead offer the latest release when the app itself is older than the remote.

## Success criteria

- A user on the desktop app upgrades a remote daemon to the app's version without leaving the app, and the daemon is running the new build afterwards — or the app says exactly why not and what to do.
- The CLI, the TUI and the desktop app produce the same outcome for the same daemon state, because they run the same code; a test asserts the clients call the shared function and carry no upgrade policy of their own.
- No upgrade stops an agent or orchestration role without the user having seen it named first.
- An older daemon without the restart capability is handled honestly by every client, and the cross-version check passes in both directions.
