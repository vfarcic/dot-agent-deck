# Versioning and the "breaking" definition

> **Developer / maintainer reference.** This page documents release-process and contract discipline. It is intentionally excluded from the published documentation site and renders as plain Markdown here on GitHub.

`dot-agent-deck` ships the TUI and the daemon as the **same binary in two modes**, so the only cross-process contract that matters is the attach protocol between them and the handler semantics behind it. The daemon deliberately outlives the TUI (agents survive detach/sleep/network/machine-switch), which means a long-running daemon can keep serving an *older* contract after you upgrade the binary in place. That runtime skew — a newer TUI meeting an older still-running daemon — is the whole reason the word "breaking" needs a precise, project-specific meaning here.

## What "breaking" means in this project

**Breaking = a change to the TUI↔daemon protocol/handler contract such that an older and a newer build cannot safely interoperate.** This is *not* the same axis as "user-facing breaking change". It is specifically the cross-process contract between the two modes of the binary.

A change is breaking in this sense when it would make an older peer mis-behave against a newer one. That includes the obvious structural cases (a new request variant, a changed field shape) **and** the subtle ones: **semantic breaks behind a stable wire** — a field whose *meaning* changes, or a role-map value type that shifts — where the bytes still deserialize but one side now interprets them wrongly. The classic symptom is a delegate signal that the stale daemon silently no-ops because it doesn't understand the newer shape.

Generic user-facing breakage (a renamed flag, a removed command, a changed default) is a normal product change and is **not** what the `breaking` changelog type is reserved for. Use it only for the cross-process compatibility break described above.

## How a break is detected and marked

Detection is layered — there is deliberately **no** mechanical CI schema/snapshot gate, because a type snapshot catches structural breaks (already covered below) but is blind to the residual risk, which is semantic.

1. **`PROTOCOL_VERSION` is the structural fatal floor.** Any wire-shape break must bump `PROTOCOL_VERSION` in `src/daemon_protocol.rs`; the attach handshake refuses across a `PROTOCOL_VERSION` mismatch. This is mandatory and non-negotiable.
2. **A human-marked `.breaking.md` changelog fragment for semantic breaks.** A same-wire/different-meaning change cannot be detected mechanically, so the author marks it: add a `changelog.d/<issue>.breaking.md` fragment (the `breaking` towncrier type defined in `pyproject.toml`). This is also the signal a future compatibility-classifying handshake would consume.
3. **A cross-version manual test** (see below) for any PR that touches the daemon, the protocol, orchestration, or hooks.

## The 0.x bump policy

While the major version is `0`, the bump rules are deliberately shifted down one level from standard SemVer so the **minor digit tracks compatibility**, not features:

| Change | While `0.x` | From `1.0` onward |
|---|---|---|
| breaking (protocol/handler contract) | **minor** (`0.31.x → 0.32.0`) | major |
| feature (new user-facing functionality) | **patch** (`0.31.1 → 0.31.2`) | minor |
| bugfix | **patch** | patch |

The consequence worth internalizing: while in `0.x`, a feature-only release is a *patch* release, and **only a protocol-breaking change bumps the minor**. So the minor digit stops meaning "has new features" and starts meaning "compatibility broke" — if `0.31.x` becomes `0.32.0`, an older peer can no longer safely talk to a `0.32.x` one. This is already implemented on the release side in the vendored `.claude/skills/dot-ai-tag-release/analyze.sh` (the `breaking → minor`, `feature/bugfix → patch` recalibration); the table above is the policy that script encodes.

## Cross-version manual-test discipline

`PROTOCOL_VERSION` catches structural breaks and the `.breaking.md` fragment records semantic ones the author already knows about — but the failure mode we most want to catch is a semantic break the author *didn't* realize they introduced. The backstop is a manual test, required before merging any PR that touches the daemon / protocol / orchestration / hooks:

1. Build the PR branch's binary.
2. Start a daemon from the **previous release** (the last tagged binary), and start an agent under it.
3. Run the PR-branch **TUI** against that older daemon and confirm the core flows still work end to end: a **delegate** still routes, and **hooks** (work-done, status updates) still arrive.

If delegate or hooks silently stop flowing, the change broke the contract behind a stable wire — bump `PROTOCOL_VERSION` (if the wire shape moved) and/or add a `.breaking.md` fragment so the release is versioned as a compatibility break. This step is enforced in-repo by **CLAUDE.md permanent instruction 12**, which every agent in this project loads and follows; the canonical `dot-ai-prd-done` skill in the `dot-ai` repo carries the same check, and syncing the vendored copy under `.claude/skills/dot-ai-prd-done/` is a separate follow-up.

## Where this lives across repos

- The **0.x recalibration** in `analyze.sh` and the generic changelog-fragment guidance are generically correct and belong in the shared skill **source** (the `prompts` repo); the vendored copy here is kept in sync.
- The **dot-agent-deck-specific** parts — this breaking definition and the protocol-surface specifics — stay local (this doc + the `pyproject.toml` comment).
- The **cross-version manual-test step** and the "did this change the TUI↔daemon contract?" prompt are enforced in-repo by **CLAUDE.md permanent instruction 12** (loaded by every agent, including the `release` role that runs `/prd-done`). The same check lives canonically in the `dot-ai` repo's `dot-ai-prd-done` skill; the copy under `.claude/skills/dot-ai-prd-done/` here is vendored, and folding the check into that vendored copy + its upstream source is a separate follow-up.
