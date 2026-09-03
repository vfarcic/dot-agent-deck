# PRD #819: Move project resolution behind the daemon, so a client holds no project state

**Status**: Not started — this document was written 2026-09-03 from a reconnaissance pass, then revised after a code review and a security audit. Four of the issue's premises did not survive the reconnaissance, and one of this document's own did not survive the audit. See [What the reconnaissance changed](#what-the-reconnaissance-changed) before treating the issue as authoritative.
**Priority**: High — this is [#741](https://github.com/vfarcic/dot-agent-deck/issues/741)'s long pole, and it will never be cheaper than it is now.
**Created**: 2026-09-03
**Issue**: [#819](https://github.com/vfarcic/dot-agent-deck/issues/819)
**Related**: [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) (connect to any daemon), [#742](https://github.com/vfarcic/dot-agent-deck/issues/742) (fleet view), [#745](https://github.com/vfarcic/dot-agent-deck/issues/745) (the overview's data ceiling), [#803](https://github.com/vfarcic/dot-agent-deck/issues/803) (settings — the other half of the boundary), [#824](https://github.com/vfarcic/dot-agent-deck/issues/824) (the four `localStorage` keys), [#828](https://github.com/vfarcic/dot-agent-deck/issues/828) (racing writers on a small per-user file), [#801](https://github.com/vfarcic/dot-agent-deck/issues/801) (compatibility from the contract), [#405](https://github.com/vfarcic/dot-agent-deck/issues/405) (the TUI's local attach path has no version gate), [#328](https://github.com/vfarcic/dot-agent-deck/issues/328) (size-bounded reads for caller-supplied input), [#633](https://github.com/vfarcic/dot-agent-deck/issues/633) (execution telemetry), [#176](https://github.com/vfarcic/dot-agent-deck/issues/176) (the desktop GUI, which deferred this), `prds/done/76-remote-agent-environments.md` (built and deleted this architecture once), `prds/done/220-dispatcher-mode-worktree-dispatch.md` (the actual code precedent)

## Problem Statement

The desktop app reads the project from its **own** filesystem, and gets away with it only because its daemon has so far always been on the same machine. Against a remote daemon the config is read from the wrong host and the coordinator context is written where no agent will ever read it — and **workflow launch does not error**. It silently validates against the wrong filesystem. Measured 2026-08-29 and recorded in #741: a real remote session put a local `/Users/...` path in the header beside the remote's `/home/vfarcic/...` panes.

Three production lines in `desktop/src-tauri/src/lib.rs` do it — `:16` (the `load_project_config` import), `:126` (the read), and `:170`, which reaches a `create_dir_all` and a `write` indirectly through the shared composer at `src/orchestrator_context.rs:191`/`:201`. They sit on one path: `desktop_run_action` (`:751`) → the `StartWorkflow` arm (`:815`) → `prepare_workflow_launch` (`:838`, defined `:149`) → `validate_workflow_against_project` (`:121`) → `load_project_config` (`:126`), plus `prepare_orchestrator_prompt` (`:170`).

**Workflow launch is the only desktop feature that resolves a project config from a filesystem.** The project the header displays (`desktop/src/lib/bridge.ts:359-363`, a four-way `cwd` fallback) and the project list the launcher offers (`desktop/src/hooks/useProjects.ts`) are the only other project-aware surfaces, and this PRD addresses both. Everything else the app shows — agent list, PTY streams, status, hook events — already comes from the daemon. So the surface is one code path plus two identity sources, not the nine milestones PRD #76's rejected Phase 6 faced, and every feature added before this lands arrives as one more branch to port.

Two PRDs already deferred this work outward (`prds/176-desktop-gui.md:85` and #741) and **the tracker both of them deferred to was never created**, which is why #741's critical path read as murky: its long pole was not in the backlog. This PRD is that tracker.

## The governing principle

> **A client gets everything from the daemon, wherever that daemon runs. The only thing it owns is its own settings plus genuinely presentational state.**

This is stronger than "fix remote", and the difference is the point. Under it, **local and remote stop being separate cases**: if the client never resolves a project itself, there is nothing for a remote daemon to do differently. #741's "if a daemon is remote and project resolution is unavailable, block launch-type actions" becomes unnecessary rather than conditional.

It also explains why the bug shipped. The local case is what hid it — reading the local filesystem happens to give the right answer when the daemon is local, so nothing looked wrong until a remote session made the two disagree. A rule that applies only to remote leaves the same trap for the next feature. A rule that applies always cannot.

This is **PRD #76's reason-1 tax, accepted deliberately and permanently.** #76 rejected laptop-as-real-client partly because "every new project-aware feature would need a protocol op and a `ProjectIO` branch". That cost is real and does not go away. #76's escape hatch was TUI-on-remote — `ssh -t` forwards a terminal, so the client can be a dumb pipe — and **there is no `ssh -t` for a window**. A desktop app cannot run on the daemon's host and be forwarded. So the cost #76 avoided is not avoidable here; it is the price of having a GUI that can be remote, and the choice is to pay it deliberately now or discover it later. CLAUDE.md rule 12 therefore applies to this surface routinely rather than occasionally.

## What the reconnaissance changed

The issue is a placeholder written from a `grep`, and its measurements were taken on `main` at `45528bc`. A reconnaissance pass over the current tree falsified four of its load-bearing claims. **Whoever reads the issue after this document should read this table first.**

| the issue says | measured on this branch |
| --- | --- |
| `load_project_config` is called **client-side** from `src/dispatch.rs:124` and `src/spawn.rs:196`, so the TUI shares the bug | **Both already run in the daemon.** `dispatch.rs:124` is inside `list_targets_response`, whose only production caller is `daemon.rs:1927`; `src/spawn.rs:3-5` states outright that it "runs in-process against the daemon's `AgentPtyRegistry` — it does NOT go over the attach socket". The client-side sites are **five in `src/ui.rs`** (`:5272`, `:8297`, `:11113`, `:11842`, `:12323`) that the issue never names, plus `main.rs:1359` (the `validate` CLI linter). |
| `prds/done/128-orchestrator-spawn-prompt-daemon-side.md` is "the same shape of change … start there rather than from a blank page" | **#128 is not a precedent.** It offered Direction A (move daemon-side) and Direction B (a client-side trigger change) and **shipped B-1** — a 500 ms timing buffer — explicitly recording "Direction A deferred". `SPAWN_TIME_READINESS_BUFFER` and `should_inject_spawn_time_prompt` are still client-side at `src/ui.rs:1870`/`:1884`. It added no verb, moved nothing daemon-side and bumped nothing. **The real precedent is PRD #220**: `list_targets_response` (`src/dispatch.rs:110`) called from `daemon.rs:1927`, returning the wire-shaped `ListTargetsResponse` / `ListedOrchestration` (`src/event.rs:959`/`:968`). |
| `desktop_project_cwd()` "resolves the project", so replacing it replaces project identity | It is **third in a four-way fallback**. `bridge.ts:359-363` prefers a **daemon-reported agent `cwd`** first, and the launch cwd actually comes from a `localStorage` project list the user types into (`useProjects.ts:5`, key `dot-agent-deck.desktop.projects.v1`; `addProject()` mints `cwd: ""`). A verb that replaces only `desktop_project_cwd()` moves nothing. |
| "Decide before someone clicks Browse" | **There is no Browse button.** No `@tauri-apps/plugin-dialog` in `desktop/package.json` or `desktop/src-tauri/Cargo.toml`, and no dialog usage in `desktop/src/`. The picker decision 2 describes is built from nothing, which is cheaper than the issue implies and removes a migration constraint. |
| `desktop/src-tauri/src/daemon_bridge.rs:101` classifies compatibility from a `git describe` string | `:101` is now `set_session_build_mismatch_allowance` — unrelated. Classification is `classify_handshake` (`:205`), and **#801's release-version half already landed** via PR #779. The issue's *conclusion* survives and sharpens: `major.minor` still cannot express "speaks the protocol but lacks `ResolveProject`", and `daemon_bridge.rs:180-191` says so itself. |
| the four call sites are at `:16`, `:124`, `:138`, `:175` | `:16`, `:126`, `:134-137`, `:170`. `#[cfg(test)]` starts at `:1035`, not `:1020`. |
| "the entire production-code filesystem surface of `lib.rs` is two lines" | True as a `grep` and misleading as a claim: `:170` performs a `create_dir_all` and a `write` through the shared composer, which a `grep` for `fs` in that file cannot see. **Three** production lines touch a filesystem, one of them indirectly. |

**And one correction to the issue's Decision 2, from the security audit.** The issue argues the attach endpoint is safe to extend because "every verb on it is scoped to agents the daemon owns". That is **false**, and the protocol documents its own opposite at `src/daemon_protocol.rs:380-392`: `AttachRequest::StartAgent` accepts arbitrary `command`, `cwd` and `env`, and the comment states that the daemon deliberately does not sandbox them because "the same user has equivalent local-exec capability via `sh -c`, and the daemon's job is to expose PTY plumbing, **not to be a privilege boundary**". The consequence for this PRD is in [What Decision 2 is and is not](#what-decision-2-is-and-is-not); the decision itself stands, on narrower and more honest grounds.

## Solution Overview

The daemon grows **three** verbs on the attach socket — enumerate the projects it knows about, resolve one, and prepare a launch — the last of which is the only one that writes. The desktop's launch flow stops resolving anything and asks. `desktop_project_cwd()` and the client-persisted project list both go away, and nothing replaces them: the client persists no project state at all.

Four commitments shape it.

**The daemon is an API.** Everything comes from it; the client owns only its own settings and genuinely presentational state (window size, focused tab, zoom, keybindings). Where the daemon does not currently expose something a client legitimately needs, **the daemon is extended** — "the client can compute it locally" is not an acceptable answer even when the client is on the same machine.

**The daemon exposes the projects it knows about, not the filesystem.** No `ListDir` / `ReadFile` / `Stat`, no parent walk, no child walk, no implicit widening. That trio is exactly what PRD #76's rejected Phase 6 proposed.

**A project is a property of a launch, not of the app.** There is no "current project" in the client, because a global current-project *is* client-held project state. The unit is the launch: `(daemon, project, workflow)`, with the project choices coming from that daemon and the workflow choices coming from that project. This is what makes the design survive #741 and #742 unchanged — with N daemons you pick a daemon first, and everything below it is already scoped.

**Single-daemon today, daemon-shaped from day one** — following #745's precedent. Enumeration is per connection and the identifier is a daemon-canonical path, while there is still exactly one daemon and it costs nothing. #742 then adds a sibling group rather than forcing a refactor. The daemon **selector** is out of scope; with one daemon it is a single-entry field.

## Scope

### In Scope

- **An ownership sweep** — every value the desktop presents or persists, classified as daemon-sourced, client-owned by policy, or computed locally, recorded as a table with a per-value verdict. This is what lets this PRD state its boundary as a verified list instead of "everything except settings", which is the false-absolute shape CLAUDE.md rule 17 exists to stop and which this PRD's own history has already required twice.
- **Three verbs on the attach socket**: enumerate known projects, resolve one, prepare a launch. Plus the owned projection they carry.
- **`PROTOCOL_VERSION` 8 → 9**, because new `AttachRequest` variants are on the bump list (`src/daemon_protocol.rs:6-14`).
- **A bounded, symlink-safe project-config reader**, run off the async runtime's worker threads behind a concurrency bound.
- **A symlink-safe, atomic, owner-only coordinator-context publish**, performed daemon-side at launch.
- **Daemon-side enumeration** from what the daemon already holds, with every candidate revalidated before it is offered.
- **Resolve-by-path** — one explicit path in, resolved, never listed. Bounds in [Resolve-by-path](#resolve-by-path-and-its-bounds).
- **The desktop's launch flow**: project choices from the daemon, workflow choices from the project, `desktop_project_cwd()` deleted, and the `dot-agent-deck.desktop.projects.v1` `localStorage` list removed with nothing persisted in its place.
- **One `capabilities` field on the `Hello` reply, and the fail-safe check helper that consumes it.** Additive and optional, so it costs no bump of its own.
- **A regression tripwire on the invariant** — a `xtask/linkage-check` rule over `desktop/src-tauri/src/`, honestly labelled as a tripwire rather than as enforcement (see [The invariant](#the-invariant-a-tripwire-not-a-boundary)).
- **Correcting two now-false doc comments** in `src/daemon_protocol.rs` (`:16-18` and `:496-499`) that sit exactly where whoever implements the degradation policy will read them.
- **The rule-12 cross-version manual test**, and **M3's remote proof** over the manual `ssh -N -L` tunnel.
- **The `projects.v1` key from #824**, which is this PRD's own subject matter rather than a borrowed concern.

### Out of Scope

- **The TUI's five `src/ui.rs` sites** — the issue's M2, **split out** on the reconnaissance evidence. See [Milestone mapping](#milestone-mapping-to-the-issue).
- **`main.rs:1359`**, the `validate` CLI linter. It is a static config checker with no daemon in the picture; a lint that needed a running daemon would be worse than the one we have.
- **#741's in-app transport.** M3 is discharged over a hand-made tunnel; #741 productises it afterwards.
- **#742's fleet view**, and the daemon selector.
- **Relaxing the desktop's exact-equality protocol check.** That is #801's per-capability work on the client side, and weakening a gate that protects users today is not a side effect this PRD should have.
- **Authentication or authorization on the attach protocol.** The audit is emphatic that bounding the project verbs is not a substitute for it if #741 ever admits a peer with less than full account authority. Recorded as a finding for #741 in [What Decision 2 is and is not](#what-decision-2-is-and-is-not), not attempted here.
- **Any persisted known-projects store.** Considered and rejected — see [Nothing remembers a project](#nothing-remembers-a-project).
- **Daemon-side configured roots** — a list on the daemon's host declaring what projects it offers. Its value is a remote-fleet administration story, which is #741/#742's problem, and it costs a new user-level config surface plus a bounded-scan policy this PRD would have to invent.
- **`ListDir` / `ReadFile` / `Stat`, and arbitrary-path *browsing*.** If selection beyond one-path-at-a-time resolution is needed later it arrives as an explicit, separately-argued verb with its own bounds.
- **The other three `localStorage` keys** (agent profiles, prompt library, workflow role order). They are per-project draft *content*, not project identity; each needs a daemon-side store with its own CRUD, ordering and concurrency questions, and #828 is a live bug in that class. They stay with #824, which closes for one key here and stays open for three.
- **Telemetry** — model, cost, tokens, context window. That is #633. Out of this PRD's scope but **not** out of the principle: it too comes from the daemon, and #633's discovery work is choosing how the *daemon* acquires it. PR #779's compile-time `Pick<>` guard stays as a guard against **fabrication**, and the sanctioned route through it is "extend the daemon, then widen the projection" — a speed bump with a documented route, not a wall.
- **`session.toml`'s `SavedPane.dir`** (`src/config.rs:214`) — persisted project directories written exclusively by `src/ui.rs`. Under the governing principle this is on the wrong side and nobody has classified it. Named in [Open Questions](#open-questions) rather than fixed here, because it is TUI state and the TUI half got split out.
- **The protocol-crate extraction** (#176 M1.1). It would give compiler-enforced reachability instead of a tripwire, and decision 1 means it stops being optional as a general position — but it is a large change and this PRD does not need it to ship. Named in [The invariant](#the-invariant-a-tripwire-not-a-boundary).
- **Windows desktop**, which also needs #754.
- **Building a Tauri end-to-end harness.** None exists; see [Testing](#testing-what-rule-4-means-here).
- **Verifying a launched bundle's `current_dir()`.** The claim is not merely unobserved but currently unreachable — `desktop/src-tauri/tauri.conf.json` sets `"bundle": { "active": false }`, so `tauri build` produces no bundle. This PRD stops depending on the claim instead of proving it; the experiment is in [Open Questions](#open-questions).

## Technical Approach

### Three verbs, and the one thing that must not be the same as the TUI

It is worth stating up front how little of the *resolution* is new, because the shape is easy to over-build.

**The resolution logic is the TUI's, and it already runs daemon-side.** A directory string identifies a project; something reads `.dot-agent-deck.toml` there and hands back what it found. That is what `list_targets_response` does today, against a cwd the **daemon** holds (`AgentRecord.cwd`) rather than the caller's own `current_dir()` — which `src/daemon.rs:1906-1913` states in substance as "the whole point", because the two diverge the moment an agent has `cd`'d.

So the verbs are:

- **Enumerate known projects** — read-only. Genuinely new, and new because a GUI cannot `cd`. A TUI user selects a project by starting the deck somewhere or by picking a directory; a window has no equivalent, and against a remote daemon it cannot browse to find out.
- **Resolve a path** — read-only. The existing logic, reachable over the attach socket. This is the primitive the desktop lacks.
- **Prepare a launch** — the only verb that writes. See [The launch verb](#the-launch-verb-which-the-first-draft-of-this-design-lacked).

**And here is the one thing that must NOT be the same as the TUI.** The TUI's client legitimately asserts a path out of its own environment, because `ssh -t` puts it on the daemon's host — its filesystem *is* the daemon's, so a path it names is a path the daemon can see. A desktop client has no such guarantee: a path from its own `current_dir()` may not exist on the daemon's host, and if it does exist it may be a different project. That is the entire bug.

So the desktop may send back only a path that the **daemon** supplied or that the **user** typed. It may never derive one from its own environment. Expose the primitives and keep that habit, and we will have shipped an API and kept the defect — which is the argument for M7's tripwire.

### The projection: what crosses the wire

`ListedOrchestration` (`src/event.rs:968`) carries `name`, `roles: usize` and `default: bool`. The desktop needs, per orchestration, its `name` plus **per role** `name` and `start` (`desktop/src-tauri/src/lib.rs:73-119`, `order_workflow_roles`). So the projection widens by one nested list. `default` is carried too — not because a current call site reads it, but for picker pre-selection, and it reuses the shape that already exists.

**The payload is small, and it gets smaller once the composer moves.** Every other field the desktop touches — `command`, `description`, `prompt_template`, `agent`, `clear` — is consumed **only** inside `prepare_orchestrator_prompt`. Move that daemon-side and none of them ever crosses the wire, which the audit notes is a security benefit as well as a size one: command strings and prompt templates stay daemon-side.

**Do not serialize `ProjectConfig`.** None of the seven public config types derives `Serialize`, `ProjectConfig` deserializes via `#[serde(try_from = "RawProjectConfig")]` — so a naive derive would emit a non-round-tripping shape with `extends` flattened and defaults materialised — and `DefaultOrchestration<'a>` carries a lifetime and can never be serialized as-is. This is asymmetry, not a missing derive. Use a purpose-built response DTO, as PRD #220 already did. The projected names are untrusted text: bound their count and length, and treat them as untrusted in logs and UI.

### Reading a caller-selected config, safely

This is the audit's first high finding, and it is the reason the resolve verb cannot simply call the existing loader.

`load_project_config` (`src/project_config.rs:934-962`) uses `std::fs::read_to_string` with no type or size check. That is fine for a file this process wrote and not fine for a caller-supplied path — and the repo already says so: `src/bounded_read.rs:1-16` (issue #328) documents the exact three shapes, that an enormous or growing file exhausts memory, that a FIFO with no writer blocks forever, and that `/dev/zero` never ends. The unbounded helper is **pre-existing**; reaching it from a caller-selected path over the attach socket is **new**.

Canonicalising the project directory does not help, because `.dot-agent-deck.toml` is resolved separately and may itself be a symlink pointing outside the project or at a special file.

So the resolve path needs a bounded project-config reader that:

- validates the path's string form at the wire boundary before touching a filesystem — reuse `is_valid_orchestration_cwd`'s absolute / control-free / 4096-byte shape (`src/agent_pty.rs:556`), reject relative and empty paths, and define non-UTF-8 as refusal rather than lossy conversion;
- opens the final config **once**, verifies from the open handle that it is a regular file, and refuses a symlinked `.dot-agent-deck.toml` by default — if links must be supported, resolve the target and prove it stays beneath the canonical project root;
- checks a documented byte limit and still caps the read, so growth after the metadata check is caught;
- caps orchestration count, role count and projected string lengths, so a small request cannot generate work approaching the frame ceiling;
- runs the blocking filesystem work off the async worker threads behind a **bounded** concurrency limit. A timeout around an uncancellable blocking read is not a substitute for preventing the blocking open.

Tests: an oversized and a growing config, a FIFO and a device, a symlinked config, a malformed and a control-bearing path, excessive role cardinality, and concurrent slow resolves.

### The launch verb, which the first draft of this design lacked

**The audit found a genuine hole here rather than a hardening opportunity, and it changes the verb count.**

After this PRD the **daemon** performs the coordinator-context write: `create_dir_all` plus a write at `.dot-agent-deck/orchestrator-context.md` beneath a client-named project. But the *task prompt* that `prepare_orchestrator_prompt` needs exists today only on `DesktopAction::StartWorkflow`, and `StartAgent` is issued once per role and carries no task and no config revision. **So the two-verb design had no defined operation at which the write happens** — enumerate and resolve are the wrong place for it, and per-role spawns are too late and too many.

Hence a third verb. Preparing the context is an explicit launch phase, not an incidental side effect of resolution:

- It carries the **daemon-returned canonical project identity**, the orchestration name, a **bounded** task, and an expected config revision.
- The daemon loads **one** validated config snapshot, resolves the requested orchestration, builds the context, and publishes it before returning success.
- Enumerate and resolve stay **read-only**. With nothing persisted (below), that is unconditional: neither has any mutation to perform.
- If spawning remains a later sequence of `StartAgent` calls, the reply carries a short-lived preparation token or revision and stale use is rejected — otherwise the resolve-and-write atomicity this design promises is not delivered, and config or path replacement between picker, write and spawn stays a TOCTOU race.
- Preparation failure returns a typed error and **starts no roles**.

And the publish itself must be safe, which today's is not. `prepare_orchestrator_prompt` (`src/orchestrator_context.rs:186-214`) follows a destination symlink, truncates in place, is not atomic, and swallows the cause behind an `Option`, creating the file under ambient umask. Joining a fixed suffix does block lexical `..` escape, and canonicalising the root removes symlinks present *at that moment*, but neither protects the child directory or the destination. So: refuse a symlinked `.dot-agent-deck` final component, replace rather than follow a destination symlink, create the directory owner-only, write via an atomic `create_new` owner-only temp file plus rename, and bound both the task input and the generated context server-side rather than trusting the desktop's current 64 KiB check. The content carries the task, a repository-supplied prompt template, role names and descriptions — it should not be assumed public merely because guidance discourages secrets.

Tests: a symlinked directory, a destination symlink, a permissive umask and parent, a partial-write failure, stale-path replacement, and the resulting permission bits.

### Nothing remembers a project

**The client persists no project state, and neither does the daemon.** Enumeration answers from what the daemon already holds:

1. **The daemon's own startup cwd**, when it holds a `.dot-agent-deck.toml`. Free, and it covers a daemon lazily spawned from the project directory. It contributes nothing for a daemon started by `systemd`/`launchd` with `cwd=/`, which is fine — it is a seed, not a guarantee.
2. **`AgentRecord.cwd`** for every live agent (`src/agent_pty.rs:2035`) — already on the wire via `ListAgents`.
3. **`TabMembership::Orchestration::orchestration_cwd`** for every orchestration role (`src/agent_pty.rs:331`) — already on the wire, and the strongest signal, since an orchestration cwd is a project by construction. Every scheduled task's `working_dir` (`src/scheduler.rs:256`) comes along free and is the only one of these that survives a daemon restart.

A project the daemon has nothing live in is named by pasting its path, which the daemon resolves.

**An earlier draft added a fourth seed: a persisted previously-resolved-projects file. It is rejected, on two independent grounds.**

The first is consistency, and it is the decisive one. **The TUI remembers nothing and needs to remember nothing, because `cd` is its selection mechanism** — the user supplies the project fresh at every launch by navigating there. What the desktop lacks is not memory but an equivalent of `cd`. That asymmetry demands a *selection mechanism*, and the earlier draft silently turned it into *persistence*. The design self-seeds through the very action it exists to support: the launch you just performed puts an agent in that project, so its cwd is enumerable for as long as anything runs there — which is exactly when you would want to launch something else there. So the file bought one thing: not re-pasting a path after everything in that project has exited. That is the same cost the TUI already imposes, and solving it for the desktop alone was the tell that it was an invented problem.

The second is that **the discipline the draft specified would not have worked.** It required `schedules.toml`'s "atomic single-validated-writer" pattern, and the audit showed that atomic temp-file rename prevents a torn *read* but does not serialize read-modify-write: `serve_attach_with_counter` spawns each accepted connection independently (`src/daemon_protocol.rs:963-1017`), so one daemon process is not one writer. Two handlers could read the same set, add different paths, and let the later rename silently discard the earlier — #828's class exactly. Closing that would have needed a shared registry behind an async mutex, a bounded parser, a cardinality and byte cap with deterministic eviction, and revalidation of every entry on load. All of that is real work for a convenience the TUI does without.

**The residual cost, stated plainly:** pasting a path is worse than picking from a list, and a GUI has no shell history to lean on. Accepted, because adding memory later is purely additive and would then be shaped by a real complaint rather than a guess. **And the shape it must not take:** today's `localStorage["…projects.v1"]` is the *source of truth* for the launch cwd with nothing validating it against the daemon's world, which is why #824 places it on the wrong side. Any future convenience must be prefill for a field the daemon still resolves — never an authority.

### Every candidate is revalidated, and the canonical spelling propagates

The audit's fifth finding, and it matters more now that enumeration is entirely seed-derived: **the seeds are candidate directories, not projects.** An ordinary agent cwd or a scheduled task's `working_dir` need not contain a `.dot-agent-deck.toml`. Treating seed origin as proof would return directories that are not projects, which breaks Decision 2's own boundary.

So every candidate is untrusted: validate its string form, canonicalise it, and resolve it through the same bounded reader before it enters the returned list or the primary nomination. Include only projects that currently resolve. Deduplicate **after** canonicalisation, cap the set **before** doing filesystem work, skip non-UTF-8 paths explicitly, and define a deterministic primary precedence among the survivors. On resolver error, return nothing for that candidate rather than the raw path.

**The canonical path the daemon returns must replace the typed or seed spelling** in the client's selection and in every later `StartAgent.cwd` / `orchestration_cwd`. This is what makes "the same directory string resolves the listing and the spawn" true rather than aspirational — and it is load-bearing, because canonicalising a symlinked path **changes its basename** (`/home/v/current` → `/home/v/code/foo`) and an empty orchestration name is derived from the basename (`src/project_config.rs:947-951`). Canonicalise once at the boundary and let the canonical form flow through to the spawn; resolve or spawn against any other form and the listing says `current` while the spawn says `foo`, which is PRD #220's bug verbatim (`src/dispatch.rs:17-24`).

Note the trade the audit asks to be recorded rather than presented as free: a user who supplies an alias or symlink gets back a different absolute spelling, that spelling becomes the spawn cwd, and a workflow deliberately spelled through a symlink changes behaviour. Test it.

### Resolve-by-path, and its bounds

- **Resolve-only, never list.** One path in, resolved. No directory walk, no children, no parents.
- **No implicit widening.** Resolving `/a/b` does not make `/a` or `/a/b/c` known.
- **Bounded disclosure on refusal.** The wire response does not directly distinguish "no such directory" from "no config there".

**That last bound is narrower than the first draft claimed, and the audit is right to narrow it.** The draft said a uniform refusal means the verb "is not an existence oracle". Two problems. Canonicalisation, traversal, open and parse do observably different amounts of work, so **timing uniformity cannot be promised** and this document does not claim it. And more concretely, the draft simultaneously required "surface parse errors; do not swallow them" — but `ProjectConfigError`'s `Display` renders **the offending TOML source line verbatim** (`src/project_config.rs:27-65`, whose own doc comment says so and notes that escaping control bytes is not redaction). Returning that for an arbitrary caller-selected path discloses file *content*, not merely existence, and distinguishes a malformed config from both a missing directory and a non-project directory. Two requirements written separately were in direct conflict.

The resolution splits by trust:

- **An arbitrary pasted path** gets a stable bounded error code and generic client text. The detailed parser and OS diagnostic stays daemon-local.
- **A path already in the daemon's known set** — meaning the daemon has something live there — gets the detailed diagnostic, which is where the "your config is broken, not empty" behaviour actually earns its keep.

Response-shape tests must prove that no parser source line, raw OS error, or attacker-controlled path escapes in an arbitrary-path refusal.

### What Decision 2 is and is not

Decision 2 — projects, not the filesystem — stands, and after the read/error hardening above, resolving one exact fixed filename beneath one explicit path remains materially narrower than PRD #76's Phase 6 API. But the **reason** the issue gives for it is wrong and must not be repeated.

The issue says the attach endpoint is safe to extend because "every verb on it is scoped to agents the daemon owns". `AttachRequest::StartAgent` accepts arbitrary `command`, `cwd` and `env`, and `src/daemon_protocol.rs:380-392` states plainly that this is deliberate, that sandboxing them "would be security theater", and that "the daemon's job is to expose PTY plumbing, **not to be a privilege boundary**". The protocol also supports shutdown and PTY input.

So: **Decision 2 is API minimisation, not authorization.** Against a peer authenticated as the daemon's Unix account — which is what #741's demonstrated SSH tunnel produces — withholding `ReadFile` creates no meaningful confidentiality boundary, because that party already has equivalent shell authority. The narrow project API is still worth having, for three reasons that survive: it limits the blast radius of a *compromised or buggy UI* rather than a malicious user, it protects against accidental misuse, and it keeps least privilege available later instead of designing it out.

**The finding for #741:** if it ever admits a client with less than full account authority, the whole attach protocol needs authentication and authorization, or a restricted facade. Bounding only the project verbs is insufficient, and this PRD does not attempt it.

### The protocol bump, and why it settles the older-daemon question for the desktop

`PROTOCOL_VERSION` is 8 (`src/daemon_protocol.rs:259`), identical to `v0.39.2`, and its own policy at `:6-14` puts **new `AttachRequest` variants on the bump list**. Only new response *fields* are exempt. So this is 8 → 9, and rule 12's contract question is answered **yes — the wire shape moved**.

The consequence is larger than a version number, and it is good news. The desktop's handshake check is **exact equality** — `server_protocol_version != Some(PROTOCOL_VERSION)` (`desktop/src-tauri/src/daemon_bridge.rs:230`) — and `:66-68` states it "runs first and is never bypassed", pinned by `desktop/src/App.test.tsx:290`. So a new desktop refuses to connect to any v8 daemon outright, and an old desktop refuses a v9 one; the build-mismatch allowance does not bypass it. Neither reaches a project verb.

That **removes the dangerous option** the issue's decision 3 flags. "Fall back to client-side resolution when the daemon is local" would have reinstated the silent-wrong-filesystem behaviour on the least-tested path; it is now not merely rejected but unreachable. Decision 6 — refuse with an explanation, naming the version and what to upgrade — is delivered by the **existing** protocol-mismatch message, with no new client code.

The honest cost: a client refusing to connect is harsher than the attach socket's graceful per-request error, and the desktop bundles its own daemon sidecar so it upgrades in lockstep. If that is ever unwanted, the equality check is the thing to revisit, and that is #801's territory.

### Capability negotiation: one field, and a helper

`Hello` carries **no capability list** today: two request fields (`src/daemon_protocol.rs:506-510`), and on the reply only `server_version`, `build_version`, `daemon_version`, `running_agents` and one capability boolean, `guarded_send` (`:725`). #801's release-version half landed via PR #779; its per-capability half did not exist anywhere in the tree, so this PRD builds the first piece.

One additive optional field — `capabilities: Option<Vec<String>>` — is preferred over N booleans, and it costs no bump of its own because `AttachResponse` is a **struct of all-optional fields** (`:647-735`) rather than an enum. There are consequently no response *variants* and no unknown-response-variant problem; any enum nested inside a response needs `#[serde(other)]` (`src/event.rs:396-405`), and `DefaultOrchestrationReason` should either be omitted from the wire or carry the catch-all.

**What this PRD builds is the plumbing, not a live decline.** With the TUI's five `ui.rs` sites out of scope, the TUI has no project-aware action wired to the new verbs, so "the TUI declines on absence" is not a checkable outcome here. M5 therefore advertises the capability on `Hello`, captures the set **once at handshake** (rather than re-probing per call — the existing probe opens a fresh connection and a second `Hello`), and provides the fail-safe check helper following `guarded_send`: declaration `src/daemon_protocol.rs:713-725`, client check `src/daemon_client.rs:504-512`, probe `:551-564`, which checks the **explicit capability, not the version number**. The in-scope test is the helper's absence-handling, on the `lifecycle/handshake/007` env-var model.

It is built now rather than with the TUI adoption because it is the cheap half: additive on a struct-of-optionals reply, and shipping it alongside the bump avoids a second protocol touch later.

Three constraints on its use, from the audit. Absence must **withhold** actions, never enable them — a daemon can lie about support, but capability advertisement is compatibility metadata rather than authentication, and a daemon already controls its own replies. Unknown capability strings are ignored. And **nothing** may branch on serde's unknown-operation error text; cache the set only for the handshake's endpoint and daemon generation, invalidate on reconnect or endpoint change, and treat a later clean `ok:false` as a displayed operation failure rather than a licence to resolve client-side.

### The invariant: a tripwire, not a boundary

`desktop/src-tauri/Cargo.toml:15` carries `dot-agent-deck = { path = "../.." }`. That is *why* `load_project_config` is reachable from `lib.rs:126`: every `pub` item in the root crate is callable from the desktop crate. The issue treats this purely as a convenience — daemon and desktop move atomically inside one workspace, which is true and is why #176 M1.1 is not a prerequisite. It is **also the hole in this PRD's central invariant**: afterwards, nothing stops the next feature re-introducing a client-side project read.

So M7 adds a `xtask/linkage-check` rule over `desktop/src-tauri/src/`. **The first draft called this enforcement. It is not, and the audit is right.** A source-text rule catches today's `load_project_config`, `prepare_orchestrator_prompt`, the project filename and obvious filesystem calls, and can be bypassed — unintentionally as easily as deliberately — through an alias, a fully qualified path, a root-crate wrapper with an innocuous name, a newly public helper, a macro, or an existing imported module gaining a new method. **It is a narrow regression tripwire, and it is not a security boundary.**

Only removing the desktop's dependency on the full root crate gives compiler-enforced reachability, and that is #176 M1.1, out of scope here and named in Out of Scope with this as one more reason to do it. Meanwhile, to make the tripwire worth having: parse Rust rather than matching raw substrings; reject imports and qualified calls to `project_config`, `orchestrator_context`, `load_project_config` and `prepare_orchestrator_prompt`; reject project-state path literals and the current-directory fallback; allowlist the root-crate symbols the production desktop may use; and test aliases, grouped and multi-line imports, qualified calls, comment and string false positives, and a wrapper-shaped bypass.

**And the criterion has to be narrowed too**, because as the issue writes it, it is unachievable. *"The client touches no filesystem"* fails on day one: the desktop must read its own filesystem to locate the bundled daemon sidecar for **Replace daemon** (`daemon_bridge.rs:375`, `:411-421`) and to read its own settings. The honest criterion is **"the client resolves no project against a filesystem"**, with the legitimate client-local reads named in M1's sweep rather than waved at.

### The ordering inversion at the launch site

Resolution currently runs **two lines before the first daemon contact** — `prepare_workflow_launch` at `lib.rs:838`, `trusted_daemon()` at `:840` — and that order is deliberate: `lib.rs:837` claims "preparing the file first keeps a context-write failure atomic". Moving resolution daemon-side inverts it. A connection must exist first, and the failure modes reorder.

The launch verb is what replaces that claim, and with a better version of it: one validated config snapshot, resolve, build, publish, then report success — with no roles started if any step fails. The daemon is the party that can actually make that pair atomic, which the client never could.

### Testing: what rule 4 means here

**There is no harness that can verify the criterion end to end, and building one is out of scope.** There is no `desktop/src-tauri/tests/`, no playwright / webdriver / tauri-driver, every TS test mocks `invoke`, and no `tests/e2e_*.rs` mentions the desktop. So verification is decomposed:

- **Daemon-side, automated, lane 1.** The verbs, the enumeration, the bounded reader, the publish and the capability helper are all daemon-side and testable without a GUI. Extend existing catalog entries rather than inventing new ones: `orchestration/dispatch/004` (L2 lane 1 — `--list-targets`, daemon-side config load, `extends`, default selection) is the closest, `error/config/001`–`002` cover invalid and missing `.dot-agent-deck.toml`, and `lifecycle/handshake/001`–`007` cover the handshake, with **`007` the model for simulating an absent capability field via an env var**.
- **The hardening tests named above** — the bounded reader's six cases and the publish's six — which are the ones an implementer is most likely to skip because the happy path works without them.
- **Rust unit tests** in the desktop crate for the launch flow's new shape, following `desktop/src-tauri/src/lib.rs:1035+`. Note that the one test covering the migrated chain (`lib.rs:1258`) writes a real `.dot-agent-deck.toml` and reads the context file back off disk — afterwards it asserts against a fake daemon, so the only end-to-end check that the context has the right **content** has to be re-established daemon-side or it is lost.
- **TS tests** for the launch flow's project source, mocking the new `invoke`.
- **One manual pass** for the GUI half, and M3's remote proof. Being manual is not grounds for deferring: rule 12's cross-version test is also manual, and PR #779 discharged one inside its own scope.

`desktop_project_cwd()` has **zero tests**, so there is no captured expectation of the behaviour being replaced. Capture the intent in the new tests rather than in the old code.

`cargo xtask list-tests` groups by **branch delta**, not by area, and reports nothing on a clean branch. It is a PR review aid; `tests/CATALOG.md` and the `#[spec(...)]` annotations are the discovery routes.

### Cross-version safety

Rule 12 applies in full and the contract answer is **yes, the wire shape moved** — new `AttachRequest` variants, `PROTOCOL_VERSION` 8 → 9. The changelog fragment is a feature fragment, not `.breaking.md`: this is a wire-shape change caught by the handshake, not a same-wire/different-meaning break behind a stable version. If implementation turns up a field whose *meaning* shifts behind an unchanged shape, that reclassifies and `changelog.d/819.breaking.md` follows.

**The sandbox needs eleven environment variables — four more than rule 12 enumerates.** Rule 12's own: `DOT_AGENT_DECK_SOCKET` (`src/platform/paths.rs:1234`), `DOT_AGENT_DECK_ATTACH_SOCKET` (`:1270`), `HOME` (`:26`, `:59`), `DOT_AGENT_DECK_STATE_DIR` (`:1307`), `DOT_AGENT_DECK_LOG` (`src/main.rs:1465`), `DOT_AGENT_DECK_EXPERIMENTAL` (`src/config.rs:1103`), and `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` (`src/daemon.rs:35`; `0` means never, `daemon.rs:34-44` — and an **unparseable value silently falls back to 30**). Four more, each with a read site: `XDG_RUNTIME_DIR` (`paths.rs:1240`, `:1276` — the socket fallback), `XDG_STATE_HOME` (`:1315`), `DOT_AGENT_DECK_SESSION` (`config.rs:206` — the saved-session snapshot, **which holds project dirs**, so leaking it pollutes exactly what is being measured), and `DOT_AGENT_DECK_FEATURES_CONFIG` (`config.rs:1134`).

Do **not** set `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` or `DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS`: they fake the thing being measured, and the latter is `cfg`-gated off in release anyway.

Rule 12's two false-green traps both apply and each has already cost another PRD an attempt. **With no agents** under the previous-release daemon the branch TUI silently terminates it and lazy-spawns its own in under a second, with nothing in the output saying so. **And with more than 30 seconds** between `daemon serve` and the first attach, the idle window elapses and the same substitution happens. The sequence that reaches the real scenario: previous-release daemon → **previous-release TUI** to bring roles up under it → close that TUI (daemon and agents survive) → **branch TUI**, which shows the mismatch prompt naming the live agents → **decline** it. The tell for both traps is the same: exactly **one** `Attach protocol listening` line for the whole run, and one daemon pid and build id serving it end to end. Do the status-hook check **last**, since sending `agent-event --type running` first makes the daemon classify that pane's agent type as `Pi` and routes prompt delivery differently.

## Success Criteria

- **The desktop resolves no project against a filesystem, and persists no project state.** No `.dot-agent-deck.toml` read, no coordinator-context write, no `desktop_project_cwd()`, no project list. Its remaining filesystem reads — its own settings, and locating the bundled sidecar — are enumerated in M1's sweep with a per-value verdict, so the boundary is a checked list rather than a claim.
- **Launching a workflow against a local daemon behaves identically to today**, proven by the daemon-side tests plus one manual pass. Local is the case that hides the bug, which is what makes it the honest test of the principle.
- **The projects offered come from the daemon, and each one currently resolves** — a candidate that no longer holds a config is not offered.
- **The canonical path the daemon returned is the string used for the spawn**, so PRD #220's bug does not recur.
- **A daemon with nothing live presents a state that says so and offers a path field.** That is the only first-run behaviour, not one of two.
- **A path that leaves the known set between listing and launch is a normal outcome**, presented like the empty state rather than as an error. Enumeration is derived from live state, so this is expected rather than exceptional.
- **A malformed `.dot-agent-deck.toml` in a project the daemon knows reads as "your config is broken", not as "no orchestrations".** For an arbitrary pasted path it reads as a generic refusal, and no parser source line, raw OS error, or caller-supplied path appears in that reply.
- **A caller-selected path cannot exhaust or stall the daemon**: the six bounded-reader cases and the six publish cases all have tests, and the publish is atomic, owner-only and symlink-safe.
- **The launch verb is the only new verb that writes**, and a failed preparation starts no roles.
- **A new client against an older daemon fails safe and says why**: the desktop refuses at the handshake with the version named, and the capability helper withholds rather than enables on absence.
- **Against a remote daemon over the manual `ssh -N -L` tunnel, workflow launch is correct rather than silently wrong** — the header and the panes name the same host, and the coordinator context lands where the agent will read it.
- **Rule 12 is discharged**: `PROTOCOL_VERSION` 8 → 9, and the cross-version manual test run with a live agent under the previous-release daemon, with exactly one `Attach protocol listening` line recorded for the run.

## Milestones

### Milestone mapping to the issue

The issue lists M1 (daemon-side resolution, local), M2 (the TUI uses the same verbs) and M3 (remote proven), and says the PRD "completes as a unit". **M2 is split out**, and the reasoning is worth recording because the issue argued the opposite.

The issue's case for keeping M2 was that "M1-alone actively manufactures the problem M2 exists to close". **The reconnaissance falsifies the premise.** The authoritative resolution — the one the *spawn* uses — is already daemon-side (`src/dispatch.rs:124` from `daemon.rs:1927`, and `src/spawn.rs:196` in-process), so M1 aligns the desktop *with* existing daemon-side truth rather than forking it. The five `src/ui.rs` sites are pre-existing **display and selection** paths, not spawn paths.

And M2 is roughly five times the size the issue estimated, with a design problem the issue does not name: `ui.rs:5272` sits on a **per-frame** path (`ui.rs:5202` — "process at most ONE surface per frame"), so it cannot become a network round trip and needs a push or a cache rather than a call. `src/ui.rs` is ~27k lines, `run_tui` alone spans `:11605`–`:13500+` and is synchronous, and two of the five sites are inside it. It also widens the wire payload with `ModeConfig`, `ModePersistentPane` and `ModeRule` — types this PRD has no use for.

The issue pre-authorised this outcome: "if that proves substantial, **split it out with this rationale** rather than leaving it unfunded." The split-out issue must carry this rationale, the five line numbers, the per-frame constraint and the `ui.rs:5272` finding — the last of which is a genuine argument *for* that work that the issue did not have: the daemon **pushes** an orchestration and the TUI then re-reads the local config to describe it.

**What the split costs, stated plainly:** the TUI keeps a client-side resolution path, and the new verbs ship with one consumer instead of two, so they lose a second consumer as a check on their shape. That is a real cost, and it is smaller than the issue assumed because the paths that actually spawn things already have the cwd discipline the verbs reuse.

So this document is **M1 + M3**. It has no external dependency, and M3 is dischargeable over the already-proven manual tunnel. The milestones below do depend on each other in the stated order.

### Iteration 1 — daemon-side resolution, proven against a local daemon

- [ ] **M1 — The ownership sweep.** Every value the desktop presents or persists, classified as daemon-sourced, client-owned by policy, or computed locally, as a table in `docs/develop/desktop-gui.md`, with an issue filed for anything this PRD does not fix. **Its done-condition names mechanical sources so "every value" is checked rather than claimed**: every field of the frontend snapshot DTO, plus every `localStorage` key and every settings key. This lands first because it defines the boundary every later milestone claims — and because the rule-17 trap the sweep exists to close could otherwise recur inside the sweep.
- [ ] **M2 — The verbs, the projection, and the bump.** New `AttachRequest` variants on the attach socket, an owned response DTO carrying per orchestration `name` and `default` plus per role `name` and `start`, and `PROTOCOL_VERSION` 8 → 9. The resolve verb is functional against the existing `list_targets_response` resolver before M4 refines it, so the ordering is genuinely incremental. **If the resolver's signature changes, the hook-socket `ListTargets` path (`daemon.rs:1927`) changes in the same commit** — that is the one way "two transports over one resolver" regresses into two resolvers. Correct the two now-false doc comments at `src/daemon_protocol.rs:16-18` and `:496-499` here, since this is the file and this is the reader.
- [ ] **M3 — The bounded reader, and enumeration.** The bounded, symlink-safe, off-thread project-config reader with its concurrency bound and its six tests. Enumeration from the three free sources, with **every candidate revalidated** through that reader, deduplicated after canonicalisation, capped before filesystem work, non-UTF-8 skipped, and a deterministic primary precedence. No new persistence.
- [ ] **M4 — The launch verb and the safe publish.** The third verb, carrying canonical project identity, orchestration name, bounded task and expected config revision; one validated snapshot; resolve, build, publish, then succeed — with no roles started on failure, and a short-lived token or revision if spawning stays a later `StartAgent` sequence. The publish is atomic (`create_new` temp plus rename), owner-only, and refuses a symlinked `.dot-agent-deck` component or destination. **Canonicalisation happens once at the boundary and the canonical path is the single string used for resolution, for identity and for the spawn.** Blocking filesystem work runs off the async worker threads. Six publish tests.
- [ ] **M5 — `capabilities` on `Hello`, and the fail-safe helper.** One additive optional field; captured once at handshake; a check helper following `guarded_send` that withholds on absence, ignores unknown strings, and branches on no serde error text. Tested via the `lifecycle/handshake/007` env-var model. The live decline wires when the TUI adopts the verbs.
- [ ] **M6 — The desktop launch flow.** Project choices from the daemon, workflow choices from the project, in that order. `desktop_project_cwd()` deleted. The `projects.v1` `localStorage` list removed with nothing persisted in its place. The paste-a-path surface, the "this daemon knows nothing live" state, and the leaves-the-set-between-listing-and-launch state. Every path the client sends came from the daemon or from the user — never from its own environment. Record the `projects.v1` decision on #824 and leave its other three keys open.
- [ ] **M7 — The regression tripwire.** A `xtask/linkage-check` rule over `desktop/src-tauri/src/`, parsing Rust rather than matching substrings, with an allowlist and the bypass-shaped tests named above, plus its own runtime tests per rule 5's note that linkage-check's assertions are runtime-only. **Documented as a tripwire and not as a security boundary**, with the residual named.
- [ ] **M8 — Docs and changelog.** `docs/develop/desktop-gui.md` gets M1's ownership table, the new launch flow, the resolve bounds, the disclosure split and the empty states. Changelog fragment via the `dot-ai-changelog-fragment` skill.
- [ ] **M9 — Rule 12 cross-version manual test.** Run with the eleven-variable sandbox and the sequence above; record the `Attach protocol listening` count, the daemon pid and the build id in the Work Log as evidence the run reached the real scenario.

### Iteration 2 — remote proven

- [ ] **M10 — Remote proven.** Point the desktop at a remote daemon over a hand-made `ssh -N -L` tunnel plus `DOT_AGENT_DECK_ATTACH_SOCKET` — the route proven on 2026-08-29 — and confirm project-aware features are **correct** rather than silently wrong. Needs a remote host the user provides. This is the only milestone that tests the principle against the case it was written for, which is why the PRD is not done without it.

## Risks

- **The criterion is unachievable as the issue writes it.** "The client touches no filesystem" fails on day one — the client must read its own filesystem for its settings and the bundled sidecar. Narrowed above, but the wide version is quotable and will be quoted.
- **The happy path works without any of the hardening.** A resolve verb that calls the existing loader will pass a manual smoke test and every functional assertion, while remaining unbounded, symlink-following and blocking on the runtime. This is the risk most likely to ship: the twelve hardening tests in M3 and M4 are the protection, and they are the ones under schedule pressure.
- **`ProjectConfig` cannot go on the wire without a deliberate projection.** Seven `Deserialize`-only types, a `#[serde(try_from)]` that would not round-trip, and a lifetime on `DefaultOrchestration<'a>`. The temptation to just add `Serialize` produces a shape that looks right and is not.
- **Canonicalisation is not free.** It changes a symlinked path's basename, and an empty orchestration name is derived from that basename — so getting the canonical form only *partly* through the flow reproduces #220. It also changes behaviour for a workflow deliberately spelled through a symlink.
- **The invariant has only a tripwire.** The desktop crate path-depends on the root crate, so every `pub` item stays reachable, and a source-text rule is bypassable by an alias or an innocuously named wrapper. Only #176 M1.1 closes it. Do not let M7's existence read as enforcement.
- **No harness can verify the criterion end to end.** No Tauri e2e exists, so the invariant is neither compile-checked nor runtime-tested at the GUI level.
- **The one test covering the migrated chain has its premise destroyed by the migration.** `lib.rs:1258` writes a real config and reads the context file off disk; afterwards it asserts against a fake daemon, and the content check must be re-established daemon-side or it is quietly lost.
- **`desktop_project_cwd()` has zero tests**, so the replaced behaviour was never captured.
- **Enumeration does filesystem work per candidate.** Revalidating every seed means N bounded reads per call. Correct, but it argues for a small cap and makes the concurrency bound load-bearing rather than defensive.
- **`ListTargets` on the hook socket will look like a way to avoid the bump.** It is not — different socket, different peers, and an unknown variant there gets no reply at all rather than a clean error.
- **Any enum nested in a response needs `#[serde(other)]`**, or one unknown value makes an older client reject the entire `AttachResponse`. `src/event.rs:396-405` records this as a real past concern.
- **The bump makes the desktop upgrade in lockstep with its daemon**, harsher than a per-request error. Accepted; revisiting it is #801's job.
- **Decision 2 is not authorization, and reading it as such is the dangerous mistake.** `StartAgent` already accepts arbitrary `command`, `cwd` and `env` by design. A narrow project API limits a compromised UI's blast radius; it does not defend against a peer holding the daemon user's authority.
- **Filesystem timing stays observable.** Concurrency limits protect availability, not constant time. Accepted, and not claimed otherwise.
- **One client's activity is visible to another on the same daemon.** Launching in a project makes it enumerable for every client of that daemon. Reasonable for per-user daemon state, and worth stating rather than discovering.
- **Scope creep toward #741, #742 and #824.** This PRD adds verbs and a shape, not a transport, a selector, or three more keys.
- **Splitting M2 leaves the TUI on a client-side path**, so the verbs ship with one consumer rather than two.

## Open Questions

1. **Does a launched bundle's `current_dir()` need observing at all?** UNVERIFIED and currently unreachable — `tauri.conf.json` sets `"bundle": { "active": false }`, so no bundle is produced. This PRD stops depending on it. If someone wants it settled: flip `bundle.active`, build, move the artifact to a second machine (or rename the checkout so the compile-time path stops resolving), launch **from the desktop shell rather than a terminal** — a terminal launch inherits the shell's cwd and would produce a false pass — and read the resolved value. Worth doing once for its own sake, since a bundle has apparently never been produced.
2. **Where does `session.toml`'s `SavedPane.dir` belong?** (`src/config.rs:214`.) A persisted project directory written exclusively by `src/ui.rs`; the ownership boundary names settings and presentational state, and it is neither. Out of scope because it is TUI state, but it should not stay unclassified.
3. **Do the new verbs supersede the hook socket's `ListTargets`, or does that path delegate to the same code?** Two transports over one resolver is intended; two resolvers would not be. Decide in M2, which is also where the same-commit constraint bites.
4. **Should the desktop's exact-equality protocol check ever be relaxed?** Named because this PRD deliberately does not touch it, and because the answer sizes what is left of #801.
5. **Does the launch verb spawn, or does it hand back a token for a later `StartAgent` sequence?** M4 must pick one. Spawning inside it gives real atomicity; a token keeps the existing per-role flow at the cost of a staleness window to police.

## Work Log

### 2026-09-03 — Created, then revised by review and audit

Written from issue #819's body and its three comments, plus a reconnaissance pass over the current tree (`agent/dispatch-prd-819`, based on `main` at `364a182`). The issue is a placeholder measured at `45528bc`; the reconnaissance falsified four of its load-bearing claims, tabulated in [What the reconnaissance changed](#what-the-reconnaissance-changed). The largest were that **both** call sites the issue cites as the TUI's client-side bug already run in the daemon, and that **PRD #128 — the precedent the issue says to start from — moved nothing daemon-side**, having shipped its Direction B instead. PRD #220 is the actual code precedent, and its resolver is what this PRD reuses.

**Three decisions were carried in from the issue's comments**, already agreed and not re-derived: the daemon is an API and the client owns only its settings plus presentational state; the daemon exposes the projects it knows about and not the filesystem; and "what an older daemon does" is a client-side degradation policy plus capability negotiation on `Hello`.

**Four more were settled with the user in this session.** Project identity comes from enumeration rather than a client-held path. **A project is a property of a launch, not of the app** — which followed from noticing that workflow launch is the only feature that resolves a config, and that with N daemons a user picks a daemon first; so the unit is `(daemon, project, workflow)` and there is no global "current project", which would itself be client-held project state. Older daemon: refuse with an explanation and never fall back — and the reconnaissance then showed the local-fallback option is not merely unwise but **unreachable** for the desktop once the bump lands. And M2 split out, with capability work scoped to one field, both put to the user with the evidence.

**The user then challenged the design twice, and it got smaller both times.**

First: *"When it comes to cwd, the logic will be the same as what's currently done with TUI, except that we might need to expose some primitives/APIs from the daemon."* Correct, and taking it seriously reframed the work as **exposing primitives** rather than designing a subsystem — the resolution logic already runs daemon-side with the right cwd discipline. It also collapsed add-by-path into resolve-plus-remember rather than a third feature, and settled the identifier as the **path** rather than an opaque id, since an id adds a translation step and that step is where PRD #220's bug lives.

Second: *"We don't remember them when working with TUI. What makes desktop different?"* **Nothing does, and the earlier draft was wrong.** The TUI needs no memory because `cd` is its selection mechanism, supplied fresh at every launch. What the desktop lacks is not memory but an equivalent of `cd` — and the draft had quietly converted a selection problem into a persistence one. The design self-seeds through the action it exists to support, so the persisted file bought only "not re-pasting a path after everything in that project has exited" — the same cost the TUI already imposes. It is gone, along with the `(daemonId, path)` client-side selection, Open Question 4's bound, and the #828 risk class. The audit independently found that the discipline the draft had specified for that file **would not have worked anyway**: atomic rename prevents a torn read but does not serialize read-modify-write, and `serve_attach_with_counter` spawns each connection independently, so one daemon process is not one writer. The file was removed for consistency and turned out to be unsound as specified.

**The reviewer returned "sound after fixes" with sixteen findings, all accepted.** Three dissolved with the persistence removal — including one where the reviewer had independently reached the user's conclusion from the other direction, finding the success criterion in tension with its own body. The sharpest surviving one: with the TUI's `ui.rs` sites out of scope, "the TUI declines on absence" is **not checkable in this PRD**, so M5 became plumbing plus a fail-safe helper. It also caught that canonicalising a **symlinked** path changes its basename while an empty orchestration name is derived from that basename, so canonicalisation and the same-string discipline are only jointly safe if the canonical form is what reaches the spawn — a hazard this document had left open. And it required M1's sweep to name mechanical sources of truth, on the grounds that the rule-17 trap the sweep exists to close could recur inside the sweep.

**The audit returned six findings and one of them was a design hole rather than hardening.** The task prompt `prepare_orchestrator_prompt` needs exists only on `DesktopAction::StartWorkflow`, and `StartAgent` carries no task and no config revision — so the two-verb design **had no defined operation at which the daemon-side write happens**. Hence a third verb: an explicit launch phase carrying canonical identity, orchestration name, a bounded task and an expected revision, with enumerate and resolve read-only. It also required a bounded symlink-safe off-thread config reader (the existing loader is unbounded, and `src/bounded_read.rs` already documents these exact failure shapes for issue #328), an atomic owner-only symlink-safe publish, and revalidation of every enumeration candidate — since an agent cwd or a scheduler working dir need not be a project at all, and treating seed origin as proof would break Decision 2's own boundary.

**Two of the audit's findings corrected claims this document made.** The "not an existence oracle" bound was over-stated in two ways: timing uniformity cannot be promised, and the document simultaneously demanded that parse errors be surfaced — while `ProjectConfigError`'s `Display` renders the offending TOML source line verbatim (`src/project_config.rs:27-65`, whose own doc comment says so). Two requirements written separately were in direct conflict; the resolution splits disclosure by trust. And M7 was called enforcement when a source-text rule is bypassable by an alias or an innocuously named wrapper; it is now labelled a tripwire, explicitly not a security boundary, with #176 M1.1 named as the only thing that closes it.

**The most consequential correction is to the issue's own Decision 2.** It justifies extending the attach endpoint on the grounds that "every verb on it is scoped to agents the daemon owns". That is false, and `src/daemon_protocol.rs:380-392` documents the opposite in its own words: `StartAgent` takes arbitrary `command`, `cwd` and `env`, sandboxing them "would be security theater", and "the daemon's job is to expose PTY plumbing, **not to be a privilege boundary**". Verified directly, not taken on report. Decision 2 stands as **API minimisation** — limiting a compromised UI's blast radius, guarding against accidental misuse, keeping least privilege available later — and not as authorization. The finding for #741 is that admitting a peer with less than full account authority requires authentication on the whole protocol, and bounding the project verbs is not a substitute.
