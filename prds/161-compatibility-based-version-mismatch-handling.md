# PRD #161: Compatibility-based version-mismatch handling

**Status**: Planning (design substantially revised 2026-06-21; shared-handshake behavior resolved to A — see Design Decisions)
**Priority**: Medium
**Created**: 2026-06-14
**Last updated**: 2026-06-21
**GitHub Issue**: [#161](https://github.com/vfarcic/dot-agent-deck/issues/161)
**Parent**: PRD #103 (local daemon build-version handshake + `daemon stop`) — this PRD revises #103's policy.
**Related**: PRD #93 (always-external daemon — why a stale daemon outlives the TUI), PRD #90 (remote daemon upgrade flow), PRD #76 M2.21 (`PROTOCOL_VERSION` handshake), PRD #22 (the bottom-right "update available" badge — a **separate** GitHub-release check, out of scope here; see D6).

## Design Decisions (2026-06-21 revision)

A design conversation reworked this PRD substantially. The model in this section **supersedes** the original "three-tier semver decision duplicated across a local path and a remote path" framing wherever they conflict. The original analysis further down (Problem Statement, breaking definition, detection strategy, release machinery) is retained because it is still accurate.

The central correction: **the TUI↔daemon skew is one shared mechanism, identical for local and remote.** The only genuinely remote-specific piece is a redundant laptop-side check that should simply be deleted. So the work splits into exactly two parts:

- **Part A (shared): the TUI↔daemon attach handshake** — what happens when a newer TUI meets an older still-running daemon. Same code, same decision, local and remote.
- **Part B (remote-only): delete the laptop-side `connect` probe** that blocks on a laptop↔remote version difference, and (optionally) add a one-step `remote upgrade` nudge in its place.

### D1 — Architecture: the daemon handshake is shared; only the laptop `connect` probe is remote-specific.

Verified in code (not docs):

- `connect` runs `ssh -t -- <host> env DOT_AGENT_DECK_VIA_DAEMON=1 <install_path>` (`src/connect.rs:670-704`), which execs the **remote** binary with no subcommand. A bare invocation dispatches `None => run_dashboard()` → `run_tui_session()` → `ensure_external_daemon_or_die()` → `run_tui()` (`src/main.rs:379,738,754,844`). So the **remote** binary renders the TUI and attaches to a **same-machine** Unix-socket daemon. The pre-2026-05-09 laptop-side daemon bridge was deleted; there is no networked daemon client. The laptop process just supervises `ssh` and is the terminal.
- The TUI↔daemon handshake `build_version_handshake::ensure_compatible_daemon_or_die` runs inside `run_tui_session()` (`src/main.rs:775`), i.e. on the **same `run_dashboard` path that both a local launch and an ssh-launched remote use**. It is therefore **one code path**, exercised identically in both modes.

**Consequence:** the new-TUI-meets-old-daemon skew is real and identical in both cases — locally (you `brew`/`cargo` upgrade the binary while a local daemon keeps running) and remotely (you `remote upgrade` the binary while the remote daemon keeps running). The "remote TUI and daemon can't differ" intuition holds only for an *un-upgraded* remote (binary on disk == running daemon); once the binary is upgraded in place, they differ exactly like local. The laptop↔remote comparison done by the `connect` probe, by contrast, guards nothing — the laptop is only ssh + a terminal, so its version has no bearing on the remote session's correctness.

### D2 — Part A: the shared handshake decision (local == remote). **Resolved: A.**

When a newer TUI attaches to an older still-running daemon, the options are:

- **(A) Always restart** the daemon on any version difference (lose its agents). Cheap; no version logic. This is essentially today's #103 behavior, made consent-based instead of fatal.
- **(B) Attach if compatible, restart only if incompatible** (keep agents when it is safe to). Needs a `semver`-crate caret helper, a `build.rs` exact-release-tag signal, the dev/dirty fallback to exact-build-id, and `DAD_VERSION` carried in the handshake.
- **(C) Always attach regardless of version — rejected.** Attaching a newer TUI to a genuinely *incompatible* older daemon is exactly the #103 silent-corruption class (delegate signals no-op). Unsafe; off the table.

**RESOLVED 2026-06-21: A (always-restart, consent-based). B is deferred, not built.** Reasoning:

- The acute bug (forced upgrade just to connect) is fixed by Part B alone, independent of this choice. A vs B only decides the *secondary* question — whether a *compatible* upgrade preserves running agents — which is a nice-to-have, not the reported problem.
- A matches the accepted workflow: nothing forces an upgrade anymore (Part B), so you upgrade when idle and accept the restart. The daemon's core value (agents surviving detach/sleep/network/machine-switch) is fully preserved under A; agents are lost only when *you* choose to upgrade-and-restart.
- B's cost (semver caret helper + `build.rs` exact-tag signal + dev/dirty fallback + a 5-case test matrix) and *risk* (it re-opens the #103 silent-corruption door — a breaking change mis-marked compatible would attach and no-op delegates, safe only via ongoing `.breaking.md` discipline) are high relative to how rarely the benefit triggers (upgrade-while-agents-running *and* compatible *and* clean release builds).
- The `PROTOCOL_VERSION`-only "B-lite" middle path is rejected: cheap, but it silently reintroduces the *semantic-break* corruption that build-id equality exists to catch.

**Forward-compatibility:** keep the M1.1 handshake fields additive and optional so adding B later is a non-breaking change. **Revisit B** only if real usage shows users repeatedly losing agents to *compatible* upgrades and asking for preservation.

When a restart is required and **agents are running**, the prompt must **name the live agents** and state that restarting stops them; declining keeps you on the existing daemon (agents intact). When **no agents** are running, restart silently. Preserve the non-TTY/CI behavior (clear stderr + non-zero exit) only on the mandatory restart path.

### D3 — Part B: delete the laptop-side `connect` enforcement; optional one-step `remote upgrade` nudge.

Because of D1, the laptop's version/build **comparison** in `connect` (the fatal `BuildVersionMismatch` block and the `VersionMismatch` gate) protects nothing and is removed. The reported bug — a laptop one patch ahead being forced to upgrade a perfectly-working older remote just to connect — is fixed by **removal alone**: with the block gone, the un-upgraded remote's matched TUI+daemon connect normally.

Keep the connect **floor**: reachability / binary-missing / "remote too old to handshake" errors stay; only the version/build *comparison* is removed.

Optionally, replace the deleted block with a **one-step**, laptop-side, pre-handover nudge — **newer-only** (never suggests a downgrade), default **N**, auto-skipped when stdin is not a TTY:

- `Remote 'X' runs 0.31.0; you have 0.31.1. Upgrade and connect? [y/N]`
- **`y`** → run `remote upgrade` (swap the remote binary), then connect. The shared Part-A handshake then restarts the daemon on attach (agents stopped, named in the prompt if running).
- **`Enter` / `n`** → connect as-is.

`remote upgrade` stays **binary-swap-only** (it must not kill agents); any daemon restart is the Part-A handshake's job. A failing `remote upgrade` falls back to connecting the existing version with a clear error (failure semantics owned by `remote upgrade`; connect just reacts to the `Result`). To state "N running agents" in the nudge, the laptop needs the count from the `daemon hello` probe (see M1.1).

### D4 — Invariant: an upgrade must never leave you unable to reach your running agents.

`n`/decline always lands you on a working session against the existing daemon (agents reachable). A restart only ever happens with explicit consent (or silently when there are no agents). The original deferred design risked a "new binary on disk but restart declined → locked out" state (under today's #103 the new TUI refuses the old daemon, and SSHing in runs the same new binary and refuses it too); the shared single-step model never creates that state.

### D5 — analyze.sh recalibration already landed.

The original premise that `analyze.sh` maps `breaking → major` (and would wrongly propose v1.0.0) is **stale**: commit `8efcd76` ("pre-1.0 bump policy: breaking→minor, feature/bugfix→patch") already recalibrated the vendored `.claude/skills/dot-ai-tag-release/analyze.sh:83-101`. Phase 2 M2.1 is effectively done here. Remaining: the `pyproject.toml`/docs wording (M2.2) and syncing the `prompts` source repo.

### D6 — Rejected: an in-TUI "update available" banner as the suggest surface.

A banner can't disambiguate "upgrade local vs remote vs both." The pre-handover connect prompt (D3) is unambiguous. The PRD #22 bottom-right badge is a *separate* GitHub-release check (remote-vs-latest-release, computed on whichever machine runs the binary) and is orthogonal to this PRD.

### Decision log

| ID | Decision | Rationale | Impact on PRD |
|---|---|---|---|
| D1 | Daemon handshake is one shared code path (local == remote); only the laptop `connect` probe is remote-specific | Verified in code | Splits work into Part A (shared) + Part B (remote-only); kills the duplicated local/remote framing |
| D2 | Part A: shared handshake = **A (always-restart, consent-based)**; B deferred (forward-compatible fields kept); C rejected | A fixes everything the reported bug needs; B's cost + corruption-risk outweigh a rare benefit | M1.3 builds A; the semver/`build.rs`/dev-dirty machinery is deferred |
| D3 | Part B: delete laptop `connect` comparison; keep floor; optional one-step `remote upgrade` nudge (newer-only, default N, non-TTY skip) | Laptop is ssh+terminal; the block guards nothing | Replaces original M1.4; remote keeps no version logic of its own |
| D4 | Invariant: never strand running agents | UX correctness | Decline = working session; restart only on consent / no-agents |
| D5 | analyze.sh recalibration already landed (8efcd76) | Repo fact-check | Phase 2 M2.1 effectively done |
| D6 | Reject in-TUI banner; PRD #22 badge is separate | Disambiguation | Suggest surface = connect prompt |

---

## Problem Statement

PRD #103 closed a real correctness gap (a stale daemon silently no-op'ing delegate signals after an upgrade) by enforcing **exact `DAD_BUILD_ID` equality** between the TUI and the daemon it attaches to. `DAD_BUILD_ID` has the form `<version>-g<sha>[-dirty]` and therefore changes on **every commit**. The check is correct but far stricter than "the two are incompatible" — it refuses on *any* build difference, including builds that are fully backwards-compatible.

This over-strictness has two concrete costs:

1. **Forced double-upgrade on every change.** A user cannot upgrade one side without immediately aligning the other, even when nothing about the contract between them changed.
2. **Loss of running agents.** The daemon deliberately outlives the TUI so managed agents survive detach (PRD #93). Recycling the daemon to align build-ids SIGTERMs every child agent — destroying exactly the in-flight work the persistent daemon exists to protect.

This is observable today. Running `dot-agent-deck connect dot-agent` with a remote on `0.31.0` and a laptop on `0.31.1` — a **patch** bump, same `PROTOCOL_VERSION` (v3), i.e. wire-compatible — still hard-blocks the connect purely on the build-id (commit-sha) difference:

```
warning: remote 'dot-agent' runs dot-agent-deck 0.31.0; laptop runs 0.31.1. Run `dot-agent-deck remote upgrade dot-agent` to align.
Remote 'dot-agent' was built as 0.31.0-g0458192; laptop was built as 0.31.1-g879c8e5. Run `dot-agent-deck remote upgrade dot-agent` to re-install the remote at the current build.
```

> **Root cause (2026-06-21, D1):** for the **remote** case this message comes from the *laptop-side* `connect` probe comparing laptop↔remote, which — given the laptop is only ssh + a terminal — guards nothing and is deleted in Part B. The genuine skew (a newer TUI meeting an older daemon) is handled by the shared Part-A handshake on the daemon's own machine.

## The "breaking" definition (dot-agent-deck-specific)

For this project, **breaking = a change to the TUI↔daemon protocol/handler contract such that an older and a newer build cannot safely interoperate.** This is *not* the same axis as user-facing breaking; it is specifically the cross-process contract. The framing is natural here because the TUI and daemon are the same binary in two modes — the only cross-process contract is the attach protocol and the handler semantics behind it. The skew is a *runtime* condition (a persistent daemon outlives the TUI and survives a binary upgrade-in-place), not a build-time difference.

A change is "breaking" in this sense when it would make an older peer mis-behave against a newer one — including **semantic** changes behind a stable wire (a field whose meaning changes, a role-map value type that shifts), which is the residual risk class that `PROTOCOL_VERSION` alone cannot see. (This is the axis a future option B would classify on; under the resolved A model the handshake always restarts and does not classify.)

## Detection strategy (no mechanical CI gate)

A mechanical schema/snapshot gate over the protocol surface was **considered and rejected**: structural wire breaks are already caught by `PROTOCOL_VERSION` discipline, while the actual residual risk — semantic breaks behind a stable wire — is invisible to a type snapshot. Detection is layered instead:

1. **`PROTOCOL_VERSION` stays the structural fatal floor.** Wire-shape breaks must bump it and remain mandatory in the Part-A handshake.
2. **Human-marked `.breaking.md` changelog fragment for semantic breaks.** The only feasible signal for a same-wire/different-meaning change (and the input a future option B would need).
3. **A cross-version manual-test step** in `prd-done` for PRDs touching the daemon / protocol / orchestration / hooks: run the new TUI against a daemon spawned from the *previous* release and confirm delegate + hooks still flow.
4. **(Optional) a non-blocking CI nudge** when a PR touches `src/daemon_protocol.rs`, the event schema, or the role-map types.

## Release machinery (semver bump correctness)

The 0.x bump policy is: **while major is 0**, `breaking → minor`, `feature → patch`, `bugfix → patch`; **from 1.0 onward**, `breaking → major`, `feature → minor`, `bugfix → patch`.

> Status (D5): **already implemented** in the vendored `analyze.sh` via commit `8efcd76`. Remaining: `pyproject.toml`/docs wording (M2.2) and syncing the `prompts` source repo.

### Versioning-cadence consequence

While in 0.x, feature-only releases become patch releases (e.g. `0.31.1 → 0.31.2`); only a protocol-breaking change bumps the minor. The minor digit stops meaning "has new features" and starts meaning "broke compatibility." Already in effect on the release side (D5); document it (M5.1).

## Three-repo coordination

- **`dot-agent-deck`** (this repo) — the runtime change (Parts A + B) + vendored skill copies.
- **`prompts`** (source for `dot-ai-tag-release` / `dot-ai-changelog-fragment`) — the `analyze.sh` 0.x recalibration (done here, still to sync to source) + generic fragment guidance.
- **`dot-ai`** (source for `dot-ai-prd-done`) — the cross-version manual-test step + the "did this change the TUI↔daemon contract?" prompt.

**Generic vs local boundary**: the `analyze.sh` 0.x recalibration is generically correct and belongs in the shared skill source. The dot-agent-deck-specific bits (the breaking definition, protocol-surface specifics) stay local.

## Scope

### In Scope

- **Part A — shared TUI↔daemon handshake** (`src/build_version_handshake.rs`): implement **A** (D2) — demote #103's exact-match refusal to a consent-based restart: name live agents and prompt when agents are running, restart silently when idle, preserve non-TTY mandatory behavior. Applies to local and remote alike.
- **Part B — remote `connect` (`src/connect.rs`)**: delete the laptop↔remote version/build comparison (`VersionMismatch` gate + `BuildVersionMismatch` fatal); keep the reachability/binary-missing/handshake floor; add the optional one-step nudge (newer-only, default N, non-TTY skip; `y` → `remote upgrade` + connect; upgrade-failure → connect existing version).
- **Agent count in the probe**: extend `daemon hello` / `AttachResponse` with an additive optional running-agent count (and ideally names) so the nudge and the handshake prompt can state "N running agents."
- **Release machinery**: `pyproject.toml`/docs wording (M2.2) and `prompts`-repo sync (M2.1 source); the vendored `analyze.sh` recalibration is already done (D5).
- **Detection / discipline**: `prd-done` cross-version step + contract prompt (M3.1); optional CI nudge (M3.2).
- **Docs** (M5.1): the connect nudge + agent cost, agent preservation across detach, and the 0.x cadence.
- **Tests** (Phase 4).

### Out of Scope

- **Compatibility-based agent preservation (option B)** — **deferred** (D2): the `semver` caret helper, `build.rs` exact-tag signal, dev/dirty fallback, and version-tag compatibility classification. Keep the handshake fields additive so B stays a non-breaking future add; revisit only if users repeatedly lose agents to compatible upgrades. The `PROTOCOL_VERSION`-only "B-lite" variant is rejected outright (reintroduces semantic-break corruption).
- **Separate local-path and remote-path compatibility logic** — there is one shared handshake (D1/D2); the remote path carries no version logic of its own beyond deleting its probe.
- **(C) Always-attach regardless of compatibility** — rejected as the #103 corruption class (D2).
- **A mechanical CI snapshot/schema gate** over the protocol surface — rejected.
- **A separate compatibility epoch** distinct from the version tag — rejected.
- **An in-TUI "update available" banner** as the suggest surface — rejected (D6); the PRD #22 badge is separate.
- **`remote upgrade` killing agents itself** (it stays binary-swap-only), cross-version shims/negotiation, retroactive `PROTOCOL_VERSION` bumps, Windows.

## Success Criteria

- **Remote, un-upgraded:** a laptop at `0.31.1` connecting to a remote at `0.31.0` **connects normally** (no block, no forced upgrade); the remote's matched TUI+daemon attach, agents intact. Verified by tests (fake-ssh executor).
- **Shared handshake, no agents:** a newer TUI meeting an older daemon with no agents restarts/attaches silently. Verified by tests (local + remote).
- **Shared handshake, agents running:** the restart prompt **names the live agents** and states they will be stopped; declining keeps you on the existing daemon (agents intact). Verified by tests.
- **Never strand (D4):** no path leaves the user unable to reach running agents. Verified by tests.
- **Remote nudge:** newer-only; default N; non-TTY skip; `y` upgrades then connects; a failing upgrade falls back to connecting the existing version with a clear error. Verified by tests.
- **Release cadence:** `analyze.sh` on `0.31.x` proposes **minor** for `.breaking.md`, **patch** for feature/bugfix (already satisfied — D5).
- **Docs** explain the handshake behavior, the connect nudge, the agent cost, agent preservation across detach, and the 0.x cadence.

## Milestones

### Phase 1: Runtime (the user-facing payoff)

- [x] **M1.0 — Resolve A vs B (D2).** ✅ **A (always-restart, consent-based)**; B deferred (revisit if users lose agents to compatible upgrades). Keep M1.1 fields additive so B stays a non-breaking future add.
- [ ] **M1.1 — Probe/handshake additive fields.** A running-agent count/names field in `Hello`/`AttachResponse` (required for the restart prompt + the nudge), additive + optional; serde round-trip + older-shape-deserializes-to-None tests. No `PROTOCOL_VERSION` bump. Optionally also add `DAD_VERSION` now (additive) so a future option B is non-breaking.
- [ ] **M1.2 — Part B: remote `connect`.** Delete the version/build comparison; keep the floor; add the one-step nudge (newer-only, default N, non-TTY skip, agent count); `y` → `remote upgrade` + connect, upgrade-failure → connect existing version.
- [ ] **M1.3 — Part A: shared handshake (`build_version_handshake.rs`).** Implement **A**: demote #103's exact-match refusal to a consent-based restart — name live agents and prompt when agents are running, restart silently when idle, preserve non-TTY mandatory behavior. Applies to local and remote alike. (Option B's `semver`/`build.rs`/dev-dirty machinery is deferred.)

### Phase 2: Release machinery

- [x] **M2.1 — `analyze.sh` 0.x recalibration** (vendored copy) — done in `8efcd76` (D5). Remaining: sync the `prompts` source repo.
- [ ] **M2.2 — Sharpen breaking-definition guidance** in `pyproject.toml` comment + docs (local); keep shared `changelog-fragment` wording generic.

### Phase 3: Detection / discipline

- [ ] **M3.1 — `prd-done`** cross-version manual-test step + "did this change the TUI↔daemon contract?" prompt (source in `dot-ai`, sync copy here).
- [ ] **M3.2 — (Optional) non-blocking CI nudge** on changes to `src/daemon_protocol.rs` / event schema / role-map files.

### Phase 4: Tests

- [ ] **M4.1 — Shared handshake tests** (local + remote): no-agents silent restart; agents-running names + prompt; declining keeps the existing daemon; never-strand. (The compatible/incompatible/dev-dirty matrix is deferred with option B.)
- [ ] **M4.2 — Probe/handshake field tests** for the additive agent-count field (M1.1).
- [ ] **M4.3 — Remote connect tests** (fake-ssh executor): un-upgraded connects; nudge `y`/`n`; upgrade-failure fallback; non-TTY skip; newer-only.

### Phase 5: Docs and release

- [ ] **M5.1 — Docs**: handshake behavior, the connect nudge + agent cost, agent preservation across detach, the 0.x cadence.
- [ ] **M5.2 — Changelog fragment** (the first real exercise of the breaking-vs-not decision).
- [ ] **M5.3 — PR, review, audit, cross-version manual test, merge, close.**

## Key Files

- `src/build_version_handshake.rs` — **Part A**, the shared handshake (M1.3). Runs for both local and ssh-launched remote via `run_tui_session`.
- `src/connect.rs` — **Part B**: remove the laptop↔remote comparison (`VersionMismatch` / `BuildVersionMismatch`); add the one-step nudge (M1.2). The error enum (`RemoteConnectError`) and `run_connect` staging.
- `src/remote.rs` — `remote upgrade` stays binary-swap-only (must not kill agents); the connect `y`-path orchestrates it.
- `src/daemon_protocol.rs` — `Hello` / `AttachResponse` gain additive optional agent-count (and optionally `DAD_VERSION`) fields; `PROTOCOL_VERSION` unchanged (M1.1).
- `src/daemon_attach.rs` — `ensure_external_daemon_or_die` (reuses an existing daemon; relevant to restart sequencing).
- `src/main.rs` — `run_tui_session` calls the handshake (`main.rs:775`); the shared entry for both modes.
- `build.rs` — exact-release-tag signal — **deferred** (only needed for option B).
- `.claude/skills/dot-ai-tag-release/analyze.sh` — already recalibrated (D5); mirror to the `prompts` repo (M2.1).
- `pyproject.toml` — `breaking` type comment, sharpened locally (M2.2).
- `prds/103-local-daemon-build-version-handshake.md` — parent; this PRD revises its policy.

## Risks and Mitigations

- **Risk (deferred B): attach-if-compatible would remove #103's blunt backstop.** A breaking change mis-marked as a patch would let two incompatible builds read as "compatible" and silently corrupt (delegate no-ops). This is a primary reason B is deferred (D2). *If B is ever built:* `PROTOCOL_VERSION` still catches structural breaks; the cross-version manual test targets semantic breaks; the dev/dirty fallback preserves exact-match during development.
- **Risk: `connect` `y`-path double-prompt / stranding.** *Mitigation (D3/D4):* `remote upgrade` is binary-only; the Part-A handshake owns the restart and only prompts when agents exist; `n` always connects to the matched existing daemon.
- **Risk: upgrade failure mid-`y`.** *Mitigation:* fall back to connecting the existing version with a clear error; failure semantics owned by `remote upgrade`.
- **Risk: three-repo drift** — shared skills are vendored copies. *Mitigation:* change source repos then sync; keep dot-agent-deck specifics out of shared skills.

## Open Questions / Decisions

- **Resolved (D2): A** (always-restart, consent-based) for the shared handshake; B deferred with additive fields kept so it stays a non-breaking future add. Revisit B only if users repeatedly lose agents to compatible upgrades.
- **Resolved (D1):** one shared handshake (local == remote); the laptop `connect` probe is the only remote-specific piece and is deleted.
- **Resolved (D3):** remote suggest UX = a pre-handover one-step connect prompt (not an in-TUI banner — D6); newer-only; default N; non-TTY auto-skips; `remote upgrade` stays binary-only.
- **Resolved (D4):** never strand running agents.
- **Escape hatch** (`DOT_AGENT_DECK_FORCE_ATTACH=1` to attach despite a *mandatory* incompatibility): lean no; revisit only if asked.
