# PRD #161: Compatibility-based version-mismatch handling

**Status**: Planning
**Priority**: Medium
**Created**: 2026-06-14
**GitHub Issue**: [#161](https://github.com/vfarcic/dot-agent-deck/issues/161)
**Parent**: PRD #103 (local daemon build-version handshake + `daemon stop`) — this PRD revises #103's policy from *detect-and-refuse* to *detect-classify-and-{suggest|refuse}*.
**Related**: PRD #93 (always-external daemon — the reason a stale daemon outlives the TUI), PRD #90 (remote daemon upgrade flow), PRD #76 M2.21 (`PROTOCOL_VERSION` handshake on the `connect` strict path).

## Problem Statement

PRD #103 closed a real correctness gap (a stale daemon silently no-op'ing delegate signals after an upgrade) by enforcing **exact `DAD_BUILD_ID` equality** between the TUI and the daemon it attaches to. `DAD_BUILD_ID` has the form `<version>-g<sha>[-dirty]` and therefore changes on **every commit**. The check is correct but far stricter than "the two are incompatible" — it refuses on *any* build difference, including builds that are fully backwards-compatible.

This over-strictness has two concrete costs:

1. **Forced double-upgrade on every change.** A user cannot upgrade one side without immediately aligning the other, even when nothing about the contract between them changed. Upgrading the laptop binary while a daemon from the previous build is still running forces an immediate recycle.

2. **Loss of running agents (local daemon).** The local daemon deliberately outlives the TUI so that managed agents survive detach (PRD #93's core design). Recycling the daemon to align build-ids SIGTERMs every child agent — destroying exactly the in-flight work the persistent daemon exists to protect. So a *compatible* bump needlessly kills agents.

This is observable today. Running `dot-agent-deck connect dot-agent` with a remote on `0.29.0` and a laptop on `0.29.1` — a **patch** bump, with the **same** `PROTOCOL_VERSION` (v3), i.e. wire-compatible — still hard-blocks the connect purely on the build-id (commit-sha) difference and forces `dot-agent-deck remote upgrade`:

```
warning: remote 'dot-agent' runs dot-agent-deck 0.29.0; laptop runs 0.29.1. Run `dot-agent-deck remote upgrade dot-agent` to align.
Remote 'dot-agent' was built as 0.29.0-g0458192; laptop was built as 0.29.1-g879c8e5. Run `dot-agent-deck remote upgrade dot-agent` to re-install the remote at the current build.
```

Two of the three signals already say "compatible" (semver is a patch bump; `PROTOCOL_VERSION` matches), yet the strictest signal (build-id) overrides and blocks. The remote path *already* has a three-tier structure — `VersionMismatch` warns and proceeds, `ProtocolMismatch` is fatal, `BuildVersionMismatch` is fatal — but the build-id tier (added by #103) overrides the philosophy the other tiers already embody.

## Solution Overview

Make the upgrade **mandatory only when the two builds are genuinely incompatible**; otherwise **warn and proceed** ("suggest"). A three-tier runtime model replaces today's binary match/refuse:

- **Exact build match** (`DAD_BUILD_ID` equal) → silent attach, as today.
- **Different build but compatible** → non-blocking *suggest*: attach normally, surface a dismissible notice ("a newer build is available; run `… upgrade`/`daemon stop` to align"). **Agents are never touched.**
- **Incompatible** → mandatory: today's blocking behavior (local stop-the-daemon prompt / fatal remote connect), and the message **explicitly states the incompatibility** so the forced disruption is justified rather than arbitrary. When live agents would be lost, the prompt names them.

The compatibility verdict comes from a trustworthy signal, not from build-id equality:

- **`PROTOCOL_VERSION` mismatch → mandatory** (the existing structural floor — genuine wire incompatibility, already fatal).
- **Semver-incompatible → mandatory** (see the compatibility rule below — this catches *semantic* breaks behind a stable wire).
- **Otherwise, build differs → suggest.** Same `PROTOCOL_VERSION`, compatible semver, only the commit-sha differs.

### Compatibility rule (semver of the version tag)

The compatibility signal is the **version tag's semver** — chosen deliberately over a separate "compatibility epoch" because it is one number, legible to both users and maintainers, and reuses machinery that already exists (changelog fragments + `tag-release`). The rule is the standard 0.x convention (and exactly what Cargo's `^` operator implements):

- **While major is 0** (we are at `0.29.x`): a **minor** bump is the incompatibility boundary; everything else is a patch. `0.29.* ↔ 0.29.*` are compatible; `0.29.* ↔ 0.30.*` are not.
- **From 1.0 onward**: the boundary moves up to **major**. `1.x ↔ 1.y` compatible; `1.x ↔ 2.x` not.

Comparison uses the `semver` crate's caret/`Comparator` matching so the 0.x→1.x transition is handled correctly without hand-rolled digit logic. This is spec-permitted (SemVer 2.0.0 leaves 0.x semantics open — "anything MAY change") and is the de-facto ecosystem convention.

### Dev / dirty fallback (the granularity caveat)

Semver only discriminates between **clean release builds at distinct tags**. During development every build between two tags shares the same nearest tag, and a dirty tree changes nothing in the tag — so two dev builds can report the same semver while their handler code has diverged (this is the exact same-tag-different-commit case that motivated #103). Therefore:

- **Both sides are clean, exact-tag release builds** → trust semver (suggest vs mandatory per the rule above).
- **Either side is a dev build (commits past the nearest tag) or dirty** → fall back to **exact `DAD_BUILD_ID` match** (any difference is mandatory), preserving today's safety where semver cannot be trusted.

This requires `build.rs` to expose whether HEAD is **exactly at a release tag** (e.g. via `git describe --tags --exact-match`, or a commits-since-tag count), since `DAD_VERSION` (`git describe --tags --abbrev=0`) collapses that information away.

## The "breaking" definition (dot-agent-deck-specific)

For this project, **breaking = a change to the TUI↔daemon (or remote) protocol/handler contract such that an older and a newer build cannot safely interoperate.** This is *not* the same axis as user-facing breaking; it is specifically the cross-process contract. The framing is natural here because **the TUI and daemon are the same binary in two modes** — the only cross-process contract is the attach protocol and the handler semantics behind it. The skew is a *runtime* condition (a persistent daemon outlives the TUI and survives a binary upgrade-in-place), not a build-time difference.

A change is "breaking" in this sense when it would make an older peer mis-behave against a newer one — including **semantic** changes behind a stable wire (a field whose meaning changes, a role-map value type that shifts), which is the residual risk class that `PROTOCOL_VERSION` alone cannot see.

## Detection strategy (no mechanical CI gate)

A mechanical schema/snapshot gate over the protocol surface was **considered and rejected**: structural wire breaks are already caught by `PROTOCOL_VERSION` discipline (and the dev-build exact-match fallback), while the actual residual risk — semantic breaks behind a stable wire — is invisible to a type snapshot (the types do not change). Such a gate would therefore be redundant where it works, blind where it matters, and would fire mostly as noise on compatible additive field changes (eroding its value through alert fatigue). Instead, detection is layered:

1. **`PROTOCOL_VERSION` stays the structural fatal floor.** Wire-shape breaks must bump it (existing module contract) and remain mandatory.
2. **Human-marked `.breaking.md` changelog fragment for semantic breaks.** The only feasible signal for a same-wire/different-meaning change. `.breaking.md` is a *per-change fragment instance*, not a definition file; the definition above is prose guidance that tells an author *when* to choose that type.
3. **A cross-version manual-test step** in `prd-done` for PRDs touching the daemon / protocol / orchestration / hooks: run the new TUI against a daemon spawned from the *previous* release (or vice versa) and confirm delegate + hooks still flow. This targets the structural blind spot in normal manual testing, which exercises *new-vs-new* and so never reproduces the cross-version failure mode.
4. **(Optional) a non-blocking CI nudge** when a PR touches `src/daemon_protocol.rs`, the event schema, or the role-map types: post a comment reminding the author to confirm the fragment's breaking/compatible classification and consider a cross-version test. It reminds; it does not block (no false-positive friction, nothing to rubber-stamp).

Accept up front that this is not 100% — and note the rejected gate would not have reached 100% either, because the residual risk is semantic. Effort is spent where the risk actually lives.

## Release machinery (semver bump correctness)

The `breaking` towncrier type **already exists end-to-end** — defined in `pyproject.toml`, recognized by `tag-release`'s `analyze.sh` — but is currently **unused and miscalibrated**. `analyze.sh` maps `breaking → major`, so the *first* `.breaking.md` created on `0.29.x` would propose **`v1.0.0`**, accidentally declaring 1.0. The fix (which is correct *generic* 0.x semver behavior, not a dot-agent-deck quirk):

- **While major is 0**: `breaking → minor`, `feature → patch`, `bugfix → patch`.
- **From 1.0 onward**: keep today's mapping (`breaking → major`, `feature → minor`, `bugfix → patch`).

## Versioning-cadence consequence (call it out)

Adopting semver as the compatibility signal means, **from now on while in 0.x, feature-only releases become patch releases** (e.g. `0.29.1 → 0.29.2`); only a protocol-breaking change bumps the minor. The minor digit stops meaning "has new features" and starts meaning "broke compatibility." This is a deliberate, permanent shift in how *every* future release is versioned — not just this feature — and it is what makes compatibility readable straight off the version number.

## Three-repo coordination

Changes span three repos, all vendored as real copies into `.claude/skills/` here:

- **`dot-agent-deck`** (this repo) — the runtime change (the user-facing payoff), plus the vendored skill copies.
- **`prompts`** (source for `dot-ai-tag-release` and `dot-ai-changelog-fragment`) — the `analyze.sh` 0.x recalibration and any generic fragment-guidance wording.
- **`dot-ai`** (source for `dot-ai-prd-done`) — the cross-version manual-test step and the "did this change the TUI↔daemon contract?" prompt.

**Generic vs local boundary**: the `analyze.sh` 0.x recalibration is generically correct and benefits every 0.x project, so it belongs in the shared skill source. The dot-agent-deck-specific bits — the "TUI↔daemon contract" breaking definition, any protocol-surface specifics — stay **local** (this repo's `pyproject.toml` comment / docs), never in the shared skills.

## Scope

### In Scope

- The three-tier runtime model on **both** the local attach path (`src/build_version_handshake.rs`) and the remote connect path (`src/connect.rs`).
- A semver comparison helper using the `semver` crate caret rule, with the clean-release-vs-dev/dirty fallback to exact build-id.
- `build.rs` exposing an "exact release tag" signal (or commits-since-tag) so the fallback can be decided at runtime.
- Carrying `DAD_VERSION` (bare semver) in the local `Hello`/`AttachResponse` handshake as an **additive optional field** (`#[serde(default, skip_serializing_if = "Option::is_none")]`) — no `PROTOCOL_VERSION` bump.
- Reconciling the remote path's existing `VersionMismatch` (always-warn) and `BuildVersionMismatch` (always-fatal) into a single semver-based decision matching the three tiers.
- Reworded, conditional, self-justifying messages: mandatory only on genuine incompatibility, and stating the incompatibility (and naming agents at risk, locally) when it blocks.
- `analyze.sh` 0.x bump recalibration (prompts repo source + vendored copy here).
- Sharpened breaking-definition guidance (local) + the `prd-done` cross-version test step and prompt (dot-ai repo source + copy here).
- Tests (see Milestones Phase 4) and docs (Phase 5), including documenting the new versioning cadence.

### Out of Scope

- **A mechanical CI snapshot/schema gate** over the protocol surface — considered and rejected (see Detection strategy).
- **A separate compatibility epoch** distinct from the version tag — considered and rejected in favor of semver-alone.
- **Auto-killing a daemon hosting live agents**, or auto-restart on incompatibility — preserved from #103's out-of-scope; the user decides, and compatible skew never recycles anyway.
- **Cross-version compatibility shims / negotiation.** The model is classify-and-{suggest|refuse}, not adapt.
- **Retroactively bumping `PROTOCOL_VERSION`** for past internal refactors.
- **Windows.** Unix-socket / SIGTERM semantics assumed (inherited from #93/#103).

## Success Criteria

- A laptop at `0.29.1` connecting to a remote (or local daemon) at `0.29.0` — same `PROTOCOL_VERSION`, compatible semver — **proceeds** with a non-blocking suggestion instead of hard-blocking; local agents are untouched. Verified by tests.
- A `0.29.x ↔ 0.30.x` pair (incompatible minor in 0.x), or any `PROTOCOL_VERSION` mismatch, is **mandatory**, and the message explicitly states the incompatibility. Verified by tests.
- Two **dev/dirty** builds that differ in commit-sha (same nearest tag) still require an exact build-id match (mandatory) — the #103 safety property is preserved during development. Verified by tests.
- `analyze.sh` on `0.29.x`: a `.breaking.md` fragment proposes a **minor** bump (`0.30.0`), not `v1.0.0`; `feature`/`bugfix` fragments propose **patch**. Verified by the existing analyze-script test path.
- `prd-done` prompts for the TUI↔daemon-contract question and includes the cross-version manual-test step for protocol-touching PRDs.
- Docs explain compatible vs incompatible upgrades, the suggest behavior, agent preservation, and the new 0.x versioning cadence.

## Milestones

### Phase 1: Compatibility model in the runtime (the user-facing payoff)

- [ ] **M1.1** — Semver comparison helper: caret-rule compatibility via the `semver` crate (0.x→minor boundary, ≥1.0→major), plus the clean-release-vs-dev/dirty decision. `build.rs` emits an exact-release-tag signal (or commits-since-tag) to support the fallback. Pure-function with a unit matrix (compatible / incompatible / same-tag-different-sha / dirty / untagged).
- [ ] **M1.2** — Carry `DAD_VERSION` in the local `Hello`/`AttachResponse` handshake as an additive optional field; serde round-trip + older-shape-deserializes-to-None tests in `daemon_protocol.rs`.
- [ ] **M1.3** — Local path (`build_version_handshake.rs`): replace the binary match/refuse with the three-tier decision. Build-id-only skew (compatible) → non-blocking suggest, agents untouched, no prompt. `PROTOCOL_VERSION`-or-semver-incompatible → mandatory prompt that states the incompatibility and names live agents. Preserve the non-TTY/CI behavior (clear stderr + non-zero exit) only on the mandatory path.
- [ ] **M1.4** — Remote path (`connect.rs`): reconcile `VersionMismatch` + `BuildVersionMismatch` into one semver-based decision with the same three tiers. Compatible patch/minor-in-range → warn and proceed; incompatible → fatal with an incompatibility-stating message routed to `remote upgrade`.

### Phase 2: Release machinery — semver bump correctness

- [ ] **M2.1** — Recalibrate `analyze.sh` for 0.x (`breaking→minor`, `feature/bugfix→patch`; ≥1.0 unchanged). Change in the `prompts` repo (source) and sync the vendored copy here.
- [ ] **M2.2** — Sharpen the breaking-definition guidance to the TUI↔daemon-contract framing in this repo's `pyproject.toml` comment and docs; keep the shared `changelog-fragment` skill wording generic.

### Phase 3: Detection / discipline

- [ ] **M3.1** — `prd-done`: add the "did this change the TUI↔daemon contract?" prompt and the cross-version manual-test step for protocol/daemon/orchestration/hook PRDs. Change in `dot-ai` (source) and sync the copy here.
- [ ] **M3.2** — (Optional) non-blocking CI nudge on changes to `src/daemon_protocol.rs` / event schema / role-map files.

### Phase 4: Tests

- [ ] **M4.1** — Unit matrix for the comparison helper (Phase 1 M1.1) covering all tiers and the dev/dirty fallback.
- [ ] **M4.2** — Update the existing build-id handshake integration tests: they currently assert mandatory-on-any-mismatch; new expectations are suggest-on-compatible (proceeds, agents alive) and mandatory-on-incompatible. L1 for any TUI banner/render; L2 (`e2e_*`, gated) where the spawned binary / attach protocol is exercised.
- [ ] **M4.3** — Remote connect tests (fake-ssh executor): compatible patch → warn + proceed; incompatible (minor-in-0.x or `PROTOCOL_VERSION`) → fatal, message states incompatibility and points at `remote upgrade`.

### Phase 5: Docs and release

- [ ] **M5.1** — Docs: daemon-lifecycle / upgrade page — compatible vs incompatible upgrades, the suggest behavior, agent preservation, and the new 0.x versioning cadence (features ship as patch releases).
- [ ] **M5.2** — Changelog fragment (the first real exercise of the breaking-vs-not decision for this very PRD).
- [ ] **M5.3** — PR, review, audit, cross-version manual test, merge, close.

## Key Files

- `build.rs` — add an exact-release-tag signal (or commits-since-tag) alongside `DAD_VERSION` / `DAD_BUILD_ID` (M1.1).
- `src/daemon_protocol.rs` — `Hello` / `AttachResponse` gain `DAD_VERSION` as an additive optional field; `PROTOCOL_VERSION` unchanged (M1.2).
- `src/build_version_handshake.rs` — the local three-tier decision; demote build-id-only skew to suggest (M1.3).
- `src/connect.rs` — reconcile `VersionMismatch` + `BuildVersionMismatch` into the semver decision (M1.4); the error enum (`RemoteConnectError`) and the `run_connect` staging.
- A new comparison helper module (or in `build_id.rs`) for the semver caret + dev/dirty logic (M1.1).
- `.claude/skills/dot-ai-tag-release/analyze.sh` — 0.x bump recalibration (M2.1); mirror in the `prompts` repo.
- `pyproject.toml` — the `breaking` type comment, sharpened locally (M2.2).
- `.claude/skills/dot-ai-prd-done/` and `.claude/skills/dot-ai-changelog-fragment/` — guidance + cross-version step; mirror in `dot-ai` / `prompts` (M2.2, M3.1).
- `prds/103-local-daemon-build-version-handshake.md` — parent; this PRD revises its policy.

## Risks and Mitigations

- **Risk**: Relaxing mandatory→suggest **removes the blunt backstop**. A breaking change mis-marked as a patch would let two genuinely-incompatible builds read as "compatible" and only *suggest* an upgrade that was actually mandatory — reintroducing the #103 silent-corruption class.
  - *Mitigation*: `PROTOCOL_VERSION` still catches structural breaks (mandatory); the cross-version manual-test step targets semantic breaks; the dev/dirty fallback preserves exact-match during development. Sequencing guardrail below ensures the relaxation never ships without the signal in place. Residual risk is a rare, recoverable failure (delegate no-ops → `daemon stop`), accepted consciously.
- **Risk (sequencing)**: Shipping the build-id demotion *before* the semver signal is correct would be strictly worse than today (net removed, nothing reliable added).
  - *Mitigation*: Phase 2's `analyze.sh` recalibration and Phase 1's `DAD_VERSION`-in-handshake must land *before or with* the M1.3/M1.4 demotion.
- **Risk (coupling)**: The minor digit now encodes compatibility rather than feature-size; a feature-only release ships as a patch, which can surprise.
  - *Mitigation*: Documented explicitly (cadence section + M5.1). It is a one-time, deliberate policy shift, guarded by the `analyze.sh` mapping.
- **Risk**: `0.0.x` edge — under the caret rule every patch is incompatible. Not our current state (0.29) but worth not mishandling.
  - *Mitigation*: The `semver` crate caret already encodes this; the helper inherits it. Documented in M1.1.
- **Risk**: Three-repo drift — the shared skills are vendored as real copies and consumed by other projects.
  - *Mitigation*: Generic/local split (above); change the source repos, then sync; keep dot-agent-deck specifics out of the shared skills.
- **Risk**: Symmetry — an *older daemon / newer TUI* and *newer daemon / older TUI* must both be classified correctly.
  - *Mitigation*: The caret comparison is symmetric on the version pair; tests cover both directions. (See Open Questions on whether the suggest UX should differ by direction.)

## Open Questions

- **Suggest UX**: where/how does the local "suggest" notice render — a startup line, a dismissible footer/banner, a one-shot message? Does it persist for the session or show once? (Render-layer + L1 test implications.)
- **Remote suggest**: keep the existing stderr warning line, or also surface an in-TUI banner once attached? The remote path prints before handing over the terminal, so a stderr line may be sufficient.
- **Escape hatch**: should a `DOT_AGENT_DECK_FORCE_ATTACH=1` (or similar) exist to attach despite a *mandatory* incompatibility, for power users / debugging? Lean no by default; revisit if asked.
- **Exact-tag signal shape**: `git describe --tags --exact-match` boolean vs a commits-since-tag integer in `build.rs` — which is cleaner for the fallback and for tarball/shallow-clone builds (where git metadata may be absent and both peers come from the same source anyway)?
- **Direction-specific messaging**: when incompatible, should the message differ for "your daemon is older" vs "your daemon is newer" (the remote path already distinguishes upgrade-laptop vs upgrade-remote)?
