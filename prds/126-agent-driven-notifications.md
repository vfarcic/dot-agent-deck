# PRD #126: Agent-driven notifications + idle-worker detection

**Status**: In progress — **rescoped 2026-07-27 from a no-code dogfood into a real, shipped feature** (see [Scope decision (2026-07-27, rescope to a real feature)](#scope-decision-2026-07-27-rescope-to-a-real-feature)). Phase 1 dogfood (config-only) completed and informed this rescope; its results are preserved in [Background](#background-the-dogfood-that-led-here). M1 (detector + hardening + tests), M2 (recipe), M3 (docs) and M4 (dogfood retirement) all landed 2026-07-28. Fast tier **1173/1173**, scoped e2e **15/15**. Outstanding before merge: the rule-12 cross-version check (M1.5), the full e2e gate with recording, `/prd-done`, and the demo reel. M5 (validate on real runs) is post-merge and additionally blocked on `TELEGRAM_CHAT_ID` being provisioned.
**Priority**: Medium
**Created**: 2026-05-25
**GitHub Issue**: [#126](https://github.com/vfarcic/dot-agent-deck/issues/126)
**Closes**: [#99](https://github.com/vfarcic/dot-agent-deck/issues/99) (supersedes prior orchestrator-only `Notifier` design)
**Related**: PRD #8 (terminal bell), PRD #20 (multi-agent), PRD #82 (orchestrator role reinforcement — see [Interaction with PRD #82](#interaction-with-prd-82)), PRD #201 (Pi integration)

## Scope decision (2026-07-27, rescope to a real feature)

This PRD began as a **no-code dogfood** — wire agent notifications for this repo's own development with zero deck code, and let the experience decide whether the deck should be extended. That dogfood ran (config-only, ntfy over `curl`) and did its job: it confirmed the human-gate notifications can be driven from the orchestrator prompt, and it surfaced — immediately and concretely — the **one thing config alone cannot do: detect a worker that was delegated to and then went silent.** An LLM orchestrator, once it delegates and waits, gets no execution turns until the worker responds, so it cannot wake itself to notice a stuck/dead worker. That needs a wall-clock timer, which only the daemon can own.

The maintainer decided to **build that as a real, shipped product feature** (not a dev-only crutch), because it is generally useful to any orchestration, and because this release already carries other compatibility-affecting features. This PRD is therefore rescoped from "dogfood, no deck code" to:

1. **A daemon-side idle-worker detector** — an *agnostic* capability: the daemon tracks each outstanding delegation and, after a configurable timeout with no response, **injects a self-describing prompt into the orchestrator**. The daemon does **not** send notifications; it reports the fact, and the orchestrator decides what to do (notify, re-delegate, abandon, surface in the TUI). This is the deferred "inactivity nudge" from #99/#126, done as a neutral primitive rather than a notifier.
2. **A notification recipe** — orchestrator-only Telegram messages (via MCP), fired at the workflow's pause-for-human moments *and* on the daemon's idle-worker event.
3. **Published user documentation** — one `docs/` page that (a) documents the idle-worker feature and its config knob and (b) shows an *example* recipe wiring those moments to a chat channel (Telegram as the worked example).
4. **Retiring the dogfood artifacts** — remove the ntfy `scripts/notify.sh` glue and its per-role prompt instructions, replaced by the productized design above.

### Decisions locked in this rescope

- **Idle detection lives in the daemon**, exposed as an agnostic event (orchestrator decides the action) — not a notification feature baked into the daemon.
- **Timeout is configurable** via a new key in `.dot-agent-deck.toml`, **default 120 minutes**. A rare false positive on a legitimately long task is an acceptable, discardable notification; v1 fires on **elapsed time since delegation**, not on a liveness/activity signal (that refinement is a later improvement if false positives prove annoying).
- **Non-breaking is the target.** The idle prompt is delivered over the **existing prompt/input-injection path** (the same mechanism the scheduler and delegation already use to put text into a session), so **no new wire message type is introduced** and an older TUI simply never receives idle prompts. Rule 12's cross-version manual test is the arbiter; classified as a **feature** (non-breaking) unless implementation forces otherwise.
- **Notifications are orchestrator-only.** The orchestrator is the only agent the user talks to and the only one that ever waits on the user. Workers never notify and never block on the user — a blocked/needs-input worker returns the question via `work-done`, and the orchestrator escalates. The per-worker `blocked` pings from the dogfood are **removed**.
- **No experimental flag** (rule 9 answered "no") — the feature ships visible by default.
- **User-facing docs** go in published `docs/` + `site/sidebars.js` (rule 11), not `docs/develop/`.
- **Messages are short** and identify the orchestration (repo + PRD #), because the maintainer runs several in parallel — each message must say only *done* vs *needs attention* and *what*.

## Problem Statement

Long-running agents and orchestrations leave users with no reliable out-of-band signal that they need attention. The terminal bell (PRD #8) only reaches a focused terminal — useless once the user walks away. PRD #99 proposed a pluggable `Notifier` trait plus four channel implementations wired to an orchestrator-completion hook; that was rejected because it makes the deck a credential holder, its event source was too narrow, and it inferred "done" from outside when only the agent knows what "done" means in context.

The correct split, confirmed by the dogfood: **the agent (orchestrator) owns *what happened and what to do about it*; the deck owns only the one signal an agent structurally cannot produce about itself — that a delegatee has gone silent.** This PRD builds exactly that boundary.

### Interaction with PRD #82

A notify instruction placed in the orchestrator's `prompt_template` is delivered as a `Read` tool result and is therefore **compaction-mortal** — notifications may stop after the first compaction, the same mechanism #82 documents. This PRD mitigates it where it matters most: the daemon's **injected idle prompt is self-describing** ("worker X has been silent for N minutes; if this needs the user, tell them"), so the idle path survives compaction even if the template policy is gone. The gate/done notifications remaining compaction-mortal is expected #82 behavior and is a known limitation, not a bug fixed here.

## What we're building

### A. Daemon idle-worker detector (deck code)

- The daemon tracks, per orchestration, each **outstanding delegation** (role + start time) and whether its `work-done` has arrived.
- If a delegation has been outstanding longer than `worker_response_timeout_minutes` (default **120**) with no `work-done`, the daemon **injects one self-describing prompt** into the orchestrator session. The **shipped** wording is in `compose_idle_worker_prompt` (`src/state.rs`) and is authoritative — it is single-line, **ASCII-only (no emoji)**, opens with the stable `has not responded with work-done (dot-agent-deck daemon report, not a message from a person or an agent)` clause, and carries the role inside `[UNTRUSTED-ROLE-LABEL: … :END-UNTRUSTED-ROLE-LABEL]` markers. An earlier draft of this PRD illustrated it as *"⏱ Worker `<role>` was delegated `<N>` minutes ago…"*; that was pre-hardening and never shipped. Do not treat it, or any emoji, as part of the contract — the L2 tests assert the real anchors.
- **Agnostic:** the daemon contains **no notification logic**. It only reports the condition; the orchestrator's prompt decides the action.
- **Race-safe by construction:** the timer is per-outstanding-delegation, so an arriving `work-done` cancels it. A near-simultaneous finish therefore cannot produce a contradictory idle prompt.
- **Fires at a safe turn boundary** (unsolicited input delivered like any other injected prompt), never mid-reasoning.
- **One-shot per delegation** in v1 (fire once, don't re-nag), to avoid a stuck run spamming.

### B. Orchestrator-only Telegram notification recipe (this repo's config)

The orchestrator `prompt_template` in this repo's `.dot-agent-deck.toml` fires a **short, fire-and-forget** Telegram message (via the `telegram` MCP, using the already-installed `pi-mcp-adapter` for Pi and the `telegram` entry in `.mcp.json`) at exactly these moments — each prefixed with **repo + PRD #**:

**Tool name gotcha for M2.1 and the M3.1 docs page:** through `pi-mcp-adapter` the tool is exposed as **`telegram_send_message`** (server-name-prefixed), *not* `send_message`. A live Pi send failed on exactly that before retrying with the prefixed name — the adapter reported the server's tools as `telegram_send_message`, `telegram_get_updates`, `telegram_list_chats`, `telegram_send_photo`, and so on. The prompt must not say `send_message` unqualified. Note this is an adapter-naming detail, so the recipe wording should describe the tool by role ("the Telegram MCP's send-message tool") and let the reader match their own client's naming.

| Trigger | Example message |
|---|---|
| Test-plan gate (step 1) | `dot-agent-deck PRD #N — needs approval: test plan ready` |
| Escalation (a worker returned a needs-input question via `work-done`) | `dot-agent-deck PRD #N — needs input: <one line>` |
| Merge gate (step 7) | `dot-agent-deck PRD #N — needs go-ahead: merge PR #<pr>` |
| Full run done/abandoned (only when *fully* done) | `dot-agent-deck PRD #N — DONE: merged & closed` |
| Daemon idle-worker event | `dot-agent-deck PRD #N — STUCK: <role> silent >N min` |

Fire-and-forget throughout ("send and continue", never wait for ack). Per-worker `blocked` pings are removed.

### C. Published user documentation

One page under `docs/` (added to `site/sidebars.js`) with **two clearly separated parts**:

1. **Feature: idle-worker detection.** What it does (agnostic event), the `worker_response_timeout_minutes` knob and its default, and that the daemon reports — it does not itself notify.
2. **Example recipe: turn moments into messages.** A worked example wiring the orchestrator prompt to a chat channel, with **placeholders** (never the maintainer's real bot/chat), showing the Telegram MCP + `pi-mcp-adapter` setup as *one* channel while making clear the channel is the reader's choice. Framed as an example that works for us, not a guarantee.

### D. Retire the dogfood artifacts

Remove `scripts/notify.sh` and the per-role ntfy notify instructions from `.dot-agent-deck.toml`; discard the interrupted ntfy Greptile-fix edits; retire/convert `docs/develop/notifications-dogfood.md`. Reconcile PR #223 so it reflects the productized design end to end.

## Scope

### In scope

- Daemon idle-worker detector (A) + its `.dot-agent-deck.toml` config knob.
- Automated tests for the daemon feature (rule 4).
- Orchestrator-only Telegram recipe in this repo's config (B).
- Published user docs (C).
- Removal of ntfy dogfood artifacts (D).
- A minimal orchestrator-side expectation log kept for falsifiability (detect "reached a moment but did not send", e.g. after compaction). Lightweight; MCP's returned `message_id` already closes the "sent-but-never-arrived" gap.
- Changelog fragment (feature) and cross-version contract check (rule 12).

### Out of scope

- **The no-agent / dead-orchestrator fallback** (a pre-spawn scheduler failure or a crashed orchestrator, where there is nobody to inject into or notify). Accepted **known limitation**; notifications are best-effort, not guaranteed. Candidate for a follow-up.
- **Liveness/activity-based idle detection** (distinguishing "working but silent" from "hung"). v1 uses elapsed time with a long default; refine later only if false positives are annoying.
- **Per-role / per-task timeouts.** v1 is a single configurable value.
- **A pluggable `Notifier` trait, deck-side channels, deck-held credentials, or an agent-callable `notify` CLI.** The inversion of #99 stands — the deck reports idleness; the agent notifies.
- **Re-nagging / repeated idle prompts** for the same delegation (one-shot in v1).

## Success Criteria

- Delegating to a worker that never responds causes the daemon to inject exactly one self-describing idle prompt into the orchestrator after the configured timeout; a `work-done` arriving first cancels it (no false idle prompt). Covered by automated tests.
- `worker_response_timeout_minutes` in `.dot-agent-deck.toml` is honored (default 120).
- The change is **non-breaking**: a branch daemon interoperates with a previous-release TUI (and vice versa) — delegate and hooks still flow; an old TUI simply never receives idle prompts. Confirmed by the rule-12 cross-version manual test.
- This repo's orchestrator sends a Telegram message at each pause-for-human moment and on the idle event; messages are short and identify repo + PRD #. Workers send nothing.
- The published docs page exists, separates the feature from the example recipe, uses placeholders, and is in `site/sidebars.js`.
- The ntfy artifacts are gone; `cargo test-fast` (and, pre-PR, `cargo test-e2e`) pass.
- A changelog **feature** fragment exists; version bump follows the release's policy.

## Design notes

### Delivery of the idle prompt (the key implementation question)

Before implementing, confirm two things in `src/daemon_protocol.rs` and the daemon: (1) there is an existing message that injects text/input into a *running* session (the scheduler's injected `prompt` and delegation task delivery both put text into a session — reuse that path), and (2) how the protocol handles unknown message variants. Reusing the existing injection path introduces **no new wire shape**, which is what keeps the feature non-breaking. Only if a genuinely new daemon→TUI event variant is unavoidable does the compatibility classification need revisiting.

### Fire-and-forget, orchestrator-only

The orchestrator is the sole notifier and the sole agent that waits on the user. Workers escalate via `work-done`; the orchestrator turns an unanswerable-without-the-user question into one notification and pauses. This removes the dogfood's per-worker `blocked` pings, which pinged the user about things they could not act on and risked a worker blocking on a reply that (by the topology) can never reach it.

## Milestones

### M1 — Daemon idle-worker detector

- [x] **M1.1** — Design verification (coder): confirm the reusable injection path and the protocol's unknown-variant handling; record the chosen approach and the non-breaking classification. → see [M1.1 record](#m11-record-design-verification-2026-07-28).
- [x] **M1.2** — Implement daemon tracking of outstanding delegations + one-shot, race-safe, self-describing idle-prompt injection after the timeout. → commit `d34fbd4`.
- [x] **M1.3** — `worker_response_timeout_minutes` config in `.dot-agent-deck.toml` (default 120), read by the daemon. → commit `d34fbd4`.
- [x] **M1.4** — Automated tests (rule 4): L2 synthetic (delegate → advance clock → idle prompt injected; `work-done` before timeout → no injection; config value honored) plus at least one PTY-attached L2 exercising the user-visible pane behavior. → `idle_worker_001`–`006` (fast) + PTY `011` stand-in (`d34fbd4`), the real-agent reel-marked `012` (`4685fb9`), and the post-hardening regression set `007`–`010`/`013` (`2fa4e81`). A follow-up audit of that regression set added the natural-exit `014`, strengthened `013`, and bound the `check_claude_available` expiry checks to their own tokens (`tests/real_agent_preflight.rs`). **Fast tier 1173/1173; scoped e2e 15/15.** Every regression test was proven capable of failing by disabling the guard it targets and restoring it (see the hardening record).
- [ ] **M1.5** — Rule 12 cross-version manual test + a `changelog.d/126.feature.md` fragment. *Fragment done* (`d10db23`); the cross-version manual test is outstanding.

### M2 — Orchestrator-only Telegram recipe (this repo's config)

- [x] **M2.1** — Rewrite the orchestrator `prompt_template` to fire Telegram (the MCP's send-message tool, `telegram_send_message` under `pi-mcp-adapter`) on the five triggers above; short messages prefixed repo + PRD #; fire-and-forget. → `7b19356`. The tool is described by role rather than hard-coded, and `chat_id` is **mandatory** with the reason stated (see the transport security record); reading `telegram_get_updates` is explicitly banned.
- [x] **M2.2** — Remove per-worker `blocked` pings; workers escalate via `work-done`. → `7b19356`; verified no `notify.sh`/ntfy reference survives in `.dot-agent-deck.toml`.
- [x] **M2.3** — Keep a minimal orchestrator-side expectation log for falsifiability. → `7b19356`: the orchestrator itself appends `timestamp | moment | message_id` (or `send=failed`/`send=skipped`) to the gitignored `.dot-agent-deck/notify-log.md`. No helper script exists any more.

**Not yet operational:** `TELEGRAM_CHAT_ID` is **not provisioned** anywhere — absent from the environment and from `.env.vals.yaml` (which carries only `TELEGRAM_BOT_TOKEN`). Its absence deliberately **fails safe**: the orchestrator skips the send, logs `send=skipped`, and tells the user in-band, rather than omitting `chat_id` — which would fall back to the most recently active chat and be interceptable by any sender (see the transport security record). So no notification actually fires until the maintainer sets it. Note `.env.vals.yaml` is a **tracked** file whose credentials are `vals` references but which already carries `SLACK_TEAM_ID`/`SLACK_CHANNEL_IDS` as committed plaintext, so a plaintext chat id there would follow existing precedent *and* publish the id in a public repo; a `ref+gcpsecrets://` line or a gitignored `.env` avoids that. Maintainer's call, and a prerequisite for M5.

### M3 — Published user docs

- [x] **M3.1** — New `docs/` page (in `site/sidebars.js`) separating the idle-worker feature from the example recipe; placeholders only; recipe framed as an example. → `74ba03e`: `docs/idle-workers-and-notifications.md` (`sidebar_position` 5.6, between orchestration and scheduled-tasks). Placeholders verified — no real chat id or bot handle appears anywhere in tracked files. The test-only `DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS` seam is deliberately **not** documented, so a published page cannot invite production use of it. `npm ci && npm run build` in `site/` passes with no broken links. Same commit adds an explicit `worker_response_timeout_minutes = 120` above the first table header in this repo's own config, so the file-parsing path is exercised and correct placement is demonstrated by example.

### M4 — Retire dogfood artifacts

- [x] **M4.1** — Remove `scripts/notify.sh` + ntfy prompt instructions; discard the interrupted Greptile-fix edits; retire/convert `docs/develop/notifications-dogfood.md`; reconcile PR #223. → `7b19356`: script deleted (its half-finished ntfy edits go with it), all ntfy instructions gone from every role, and the dogfood note rewritten as a short historical pointer so the `CONTRIBUTING.md` link stays live. **PR #223 reconciliation is still outstanding** — its title and body still describe the abandoned "Phase 1 dogfood (no deck code)" scope; that happens at `/prd-done`.

### M5 — Validate on real runs (post-merge)

- [ ] **M5.1** — Run a couple of real orchestrated PRDs, including one long enough to compact; confirm the gate/escalation/done notifications and the idle event fire as intended; record any refinements.

## M1.1 record — design verification (2026-07-28)

Verified against the code before implementing; this is what fixed the approach and the non-breaking classification.

**Delivery path (no new wire shape).** The idle prompt is injected via `AgentPtyRegistry::write_to_pane_and_submit(orchestrator_pane_id, text)` — already the exact primitive and target that `handle_work_done` uses to feed the orchestrator, and that the scheduler and delegation use elsewhere. The daemon calls the registry directly, so the bytes never become a protocol frame. The sibling `write_to_pane_notice` was rejected: it writes a visible-but-unsubmitted line, and we need the orchestrator to actually act.

**Non-breaking, `PROTOCOL_VERSION` stays 6.** Unknown-variant handling is a **hard error on every wire enum** — `AttachRequest` is `#[serde(tag = "op")]` with no `#[serde(other)]`, so an unknown op yields a malformed-request reply and a closed connection; `BroadcastMsg` is `#[serde(tag = "kind")]`, so an unknown kind kills the event stream (this is precisely why PRD #120 bumped 3→4). Only additive `#[serde(default, skip_serializing_if)]` struct fields are tolerated. The handshake is an **exact-match refusal**, so a needless bump would *itself* break interop. Because the idle prompt never crosses the wire, the cross-version matrix is better than this PRD originally assumed: a new daemon with an **old TUI still shows the idle prompt** (ordinary stream-out scrollback), and an old daemon with a new TUI simply has no detector. Classified **feature** → `changelog.d/126.feature.md`, patch bump. It would only become breaking if a `BroadcastMsg`/`KIND_*`/`AttachRequest` variant were added, or an existing field's meaning changed.

**Tracking, arming, cancelling.** Arm in `handle_delegate`'s target loop — it has the orchestrator pane id, target role, worker pane id and timestamp, and arming there (rather than inside dispatch) avoids clock skew from the `clear = true` SessionStart wait while still covering an early-bailing respawn failure. Cancel at the **very top** of `handle_work_done`, above all of its early returns. State lives on `AgentPtyRegistry` as interior-mutability side-maps (the established precedent), **not** on `AppState`: both handlers run under a read guard, and moving to `write()` would widen a hot lock.

**Three races, two of which this PRD had not identified.** (1) The finish-line race is handled for free by making the timer's first act on wake an **atomic take** under the registry mutex — work-done-first means the take returns `None` and the timer is a silent no-op, which also delivers one-shot behavior. (2) **Re-delegation to the same worker pane** overwrites the record, so delegation #1's still-sleeping timer would otherwise wake and consume delegation #2's record, firing a premature prompt — fixed with a monotonic **seq** in each record that each timer captures and the take checks conditionally. (3) A **deliberately closed worker pane** (`StopAgent` → `unregister_pane`) must also cancel, or closing a stuck worker still nags two hours later.

**Config.** `worker_response_timeout_minutes` on `ProjectConfig` with `#[serde(default)] = 120`. No `deny_unknown_fields` anywhere in `src/`, so adding the key is forward- and backward-compatible by construction. Resolution order is **env > file > default**, read from the *orchestration* cwd first and the worker cwd second (they diverge for PRD #120 issue-dispatch clones). **TOML gotcha that must be carried into the M3.1 docs page:** a top-level scalar key has to appear *above* the first table header — appending it to the end of a config whose tables start early silently makes it a key of the last table, where it is ignored.

**Test seam.** There is no injectable or virtual clock anywhere in `src/` or `tests/`, and a multi-thread tokio test with real PTYs could not use one anyway. The established pattern is an env override read at use time, so the detector reads `DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS` (milliseconds), letting tests run in ~1–2s. Observation works by making the orchestrator pane a `cat` stub that echoes injected bytes into the snapshot.

**Known limitation.** A daemon **restart** loses all in-memory outstanding records and silently disarms every pending timer. Worth stating on the docs page.

## Orchestration finding — the Codex tester cannot commit in a worktree

Recorded because it cost two delegation round-trips on this PRD and will recur on every worktree run until it is addressed.

The `tester` role runs Codex with `--sandbox workspace-write`. In a **git worktree**, `.git` is a *file* pointing at `<parent-repo>/.git/worktrees/<name>/`, so the real index lives **outside** the sandbox's writable root. Every `git add`/`commit` therefore fails, and Codex surfaces it as a "read-only index" — which reads like a permissions bug but is not: the index file is mode `rw-rw-r--` and owned by the user, and the orchestrator can commit the very same files without issue. The same role would commit fine in a plain branch checkout, where `.git` sits inside the workspace.

Consequences and the workaround used here: the tester's files must be committed by someone outside the sandbox. Having the **coder** stage them (the earlier approach) works but puts the tester's tests in reach of the role explicitly forbidden from editing them, so the safer route is for the **orchestrator** to commit them directly after verifying `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` itself, since rule 2 binds whoever commits.

Real fixes, in preference order: grant the tester's sandbox write access to the parent repo's `.git/worktrees/` path; or run the tester unsandboxed for worktree PRDs; or stop using worktrees for runs that involve the Codex tester. This belongs in `CLAUDE.md` or `docs/develop/` once a fix is chosen — it is a standing property of the role configuration, not a fact about this PRD.

## M1 hardening record — review + audit resolution (2026-07-28)

A reviewer and a security auditor examined `d34fbd4`/`4685fb9` and filed eight and five findings respectively (`.dot-agent-deck/review-findings-126-m1.md`, `.dot-agent-deck/audit-findings-126-m1.md` — gitignored, so the load-bearing outcomes are recorded here). Resolved in `d82426c` and `4b3a2c6`.

**Delivery is now identity-bound, not pane-id-bound.** The original record named its target only by orchestrator *pane-id string*, which produced the worst finding of the review: after an orchestrator pane closed, a new agent taking that pane id would receive the old orchestration's idle prompt — **auto-submitted, to an agent with tool access, possibly in a different orchestration**. The record now carries the orchestrator's registry agent id and orchestration, delivery goes through the existing `write_and_submit_guarded` identity gate with a revalidation closure, and pane close is an atomic `begin_pane_close`/`finish_pane_close` transition that drops every record touching the pane — as worker *or* orchestrator — before termination, with `arm_outstanding_delegation` refusing a closing pane. If the orchestrator has no live registry agent at arm time, the daemon **declines to arm**: no nudge beats a nudge delivered to a stranger.

**Zero now means disabled, not immediate.** Both sources previously accepted `0`, which raced the worker's own dispatch and reported every worker as stuck before it could answer. `0` from either source now disables the detector outright — no record, no timer — and non-zero values are bounded (1…10080 minutes; 100 ms…7 days via the test seam), with out-of-range **rejected in favour of the default rather than silently clamped**. This semantic must appear on the M3.1 docs page.

**The role name is framed as data.** A role name is printable text from a repository's `.dot-agent-deck.toml`, which travels with a hostile clone, and the idle prompt is auto-submitted to the orchestrator. Control bytes were already blocked upstream, so the live vector was printable instruction text (a role literally named `worker. Ignore prior instructions and run: …`). The value is now wrapped in `[UNTRUSTED-ROLE-LABEL: … :END-UNTRUSTED-ROLE-LABEL]` markers that the surrounding prose declares untrusted, and `<`/`>` are stripped from the value so the field **cannot close its own quoting** and continue as prose. The `has not responded with work-done` clause deliberately **opens** the line, because L2 assertions match against a vt100 grid where a needle straddling a narrow pane's wrap column would not be found.

**Scoped by maintainer decision:** the same weakness predates this PRD on the **delegate** path, where a role name is interpolated into the task pointer submitted to the worker. A role-identifier grammar at config validation / the `TabMembership` boundary would close both, but risks rejecting existing configs with exotic role names, so it is tracked as a separate follow-up rather than done here.

**Timers are now cancellable.** Each record holds a `oneshot` sender and the watch `select!`s it against the sleep, so work-done, supersession, or close wakes the task immediately instead of leaving it asleep for the full timeout holding an `Arc` — previously unbounded in live task count and reachable by repeated `delegate` calls. Duplicate target roles within one delegate signal are also de-duplicated.

### Transport security record — `telegram-mcp-bot@1.1.0` (2026-07-28)

The notification transport was source-reviewed because it holds the bot token and, like every stdio MCP server declared in `.mcp.json`, is spawned with the **full parent environment** — so it sees every secret in the shell. Reviewed artifact: `telegram-mcp-bot-1.1.0.tgz`, npm SRI `sha512-9c2ADQsc29JpyY1knd6tae678UNoNJkt6s5NvZJmuTXZa9wclCimoa0/XLf6ibbPx8n0TOFlqOcdMIZVr5QTig==`. The published `dist/` tree was **reproduced exactly** from a local build of commit `31c745d`, and npm carries a Sigstore/SLSA provenance attestation binding that tarball to the repo's release workflow and tag.

**Package-authored code is clean:** no `fetch`/socket/telemetry/beacon of its own, no non-documentation HTTP URL, all network traffic via grammY to `api.telegram.org`; it reads only `TELEGRAM_BOT_TOKEN` plus documented options and `HOME` for `~`-expansion of a caller-supplied photo path, never enumerating `process.env` nor logging or returning the token; no `eval`/`new Function`/`vm`/dynamic import; no consumer install hooks. `npm audit --omit=dev` on the freshly resolved graph: **0 advisories at every severity**. Weak maturity signals (single maintainer, young repo, unsigned tag) are counterbalanced by the reproducible build plus provenance.

Two findings shaped the recipe and must survive into the M3.1 docs page:

- **The bot has no allowed-user or allowed-chat check (high).** Every inbound message updates that chat's `lastMessageAt`, and both send tools fall back to the **most recently active chat** when `chat_id` is omitted. Anyone who learns the bot username can therefore message it just before a notification and receive that notification instead. Mitigation adopted: **always pass an explicit configured `chat_id`**, never rely on the default. Relatedly, `get_updates` is an unauthenticated inbound channel and thus a prompt-injection path, so the orchestrator is instructed never to read it. Server-side enforcement would require a fork, a patch, or a different server — `1.1.0` exposes no allowlist option.
- **An exact top-level pin does not pin the dependency graph.** `telegram-mcp-bot@1.1.0` declares `@modelcontextprotocol/sdk ^1.29.0`, `grammy ^1.41.1`, `zod ^4.3.6` and ships no shrinkwrap, so `npx -y telegram-mcp-bot@1.1.0` still resolves mutable, unreviewed transitives that execute in-process with the inherited environment (~102 production entries, none declaring install scripts at review time). Fully closing this needs a repo-managed install with a committed lockfile invoked via `npm ci --omit=dev --ignore-scripts`. **Deferred by maintainer decision**, because `.mcp.json` already runs `coderabbitai-mcp@latest` and `@modelcontextprotocol/server-slack` the same way: it is a pre-existing repo-wide pattern, not something this PRD introduced, and fixing it properly touches all three servers. The top-level pin is applied here as a strict improvement.

### Verification record — the regression tests were proven capable of failing

Because the hardening's two blockers were fixed on the strength of an argument, each regression test was validated by disabling the specific guard it targets, observing the failure, and restoring the guard (`2fa4e81`). This matters more than usual here: the tester pane had stopped consuming delegations, so by maintainer decision the tests were written by the same role that wrote the implementation, and a demonstrated failure is the only thing separating real coverage from a test that agrees with its author.

- `idle_worker_009` (SIGTERM grace overlap) — suppressing `begin_pane_close` reproduced the nudge inside the grace window; the test also asserts the overlap itself occurred, so it cannot pass for the wrong reason.
- `idle_worker_010` (close-vs-delegate barrier) — removing the `closing_panes` check in `arm_outstanding_delegation` alone fails it.
- `idle_worker_013` (re-delegation + late first `work-done`) — removing the superseded branch in `retire_outstanding_delegation` makes delegation #2 go silent. **Strengthened after a second audit:** the original form also passed when `retire_outstanding_delegation` was stubbed to a **complete no-op** (verified), because a watch that survives a late completion proves nothing about *which* record was consumed. It now carries a twice-delegated, twice-**completed** control worker whose second completion must retire what the first deliberately left armed, so a no-op `work-done` shows up as an extra prompt. Both mutations — the no-op and the removed superseded branch — now fail it.
- `idle_worker_007` (bounds) — clamping instead of falling back to the default fails it from either source.
- `idle_worker_008` (closed orchestrator + pane-id reuse) — **does not isolate a single guard**, and cannot: on the `StopAgent` path the `begin_pane_close` sweep drops the record before any timer can wake, so the identity gate is never reached. Both layers must be removed before a stray submit appears there. This was originally accepted as defense in depth; a second audit **overturned that reasoning**, because a **natural orchestrator exit** (the process simply ending, no `StopAgent`) runs no sweep at all, leaving the identity gate as the only guard — so a single-layer regression there is a genuine misdelivery into a tool-enabled agent, possibly in another orchestration.
- `idle_worker_014` (natural orchestrator exit + pane-id reuse) — the isolating test for that gate, added in response. The orchestrator stub ends its own process on a flag file, an unrelated agent takes the freed pane id, and dropping **only** the expected agent id from the guarded send (every sweep intact) reproduces the stray auto-submit: the successor's PTY receives the dead orchestration's full idle prompt. `008` stays green under that same mutation, which is the measured proof that `014` covers what `008` cannot.

Two honest gaps recorded rather than papered over: a **file-level** `0` cannot be proven decisive behaviorally, since no config value below one minute exists to contrast it against, so `idle_worker_003` records that as a *does-not-assert* and `007` covers it at the resolution level; and `check_claude_available` is offline only (regular-file check, JSON parse, OAuth token present, and each expiry bound to the presence of **its own non-empty token** — so an expired sole access token with no refresh token, and the converse, are both rejected, while an expired access token with a *live* refresh token still passes, matching Claude Code's own refresh), deliberately with **no probe request**, because a live round trip would spend tokens on every e2e run. Revoked credentials and network failures therefore remain an accepted false-positive class. The token-bound form replaced a first cut that evaluated both expiries independently of both tokens, where an absent expiry ("no expiry information") silently voted *live* for a token that was not there; `tests/real_agent_preflight.rs` pins every accepted and rejected shape, and the three that the binding closes were verified to fail against the unbound form.

### Pre-existing, unrelated: `linkage-check` is red on this branch and on `main`

`cargo xtask linkage-check` reports 4 *forbidden sleep call (Decision 21)* failures in `tests/e2e_delegate_work_done_chain.rs` (lines 114/169/183/210). That file is untouched by this PRD and last changed in `fe929d2`, which is on `main` — the identical violations reproduce there, so this is not a regression from this work. It also does not gate the PR: **CI does not run `linkage-check`** (the `build`, `build-windows`, `build-macos`, and `security` jobs run fmt, clippy, build, `cargo test`/`nextest`, and `cargo audit`, with no `xtask` step), despite a stale comment at `.github/workflows/ci.yml:59` claiming it stays on the Linux job and CLAUDE.md rule 7 asserting CI fails the build on it. Worth reconciling the claim with the workflow separately from this PRD.

### Accepted limitations

Recorded deliberately; all three fail toward a discardable extra notification rather than silence, except where noted.

1. **Out-of-order completion credits the wrong delegation.** A `work-done` retires the *oldest* outstanding delegation for a pane. If the newest reports while an older one never does, the older is credited and the newest record stays armed, producing one spurious — discardable — nudge.
2. **A consumed-then-re-delegated record can be retired by a late completion.** If delegation #1's record was already consumed by its own timer (reported idle) and the orchestrator then re-delegates, a late `work-done` for #1 retires #2 outright, so #2 can go silent with no nudge. This is the one residual that fails toward silence. Both this and #1 are consequences of rejecting the alternative fix — correlating completion via a token echoed back by the agent — which would have made a safety mechanism depend on an **LLM faithfully round-tripping a string**.
3. **A failed pane close does not restore swept records.** Close un-marks the pane but leaves the dropped records dropped, so a genuinely stuck worker whose close *failed* is not reported afterward. Chosen as the fail-safe direction, on the grounds that a deliberate close means the operator is already handling that worker.
4. **Package pinning is exact for fresh clones only.** `.pi/settings.json` pins `npm:pi-mcp-adapter@2.15.0` (Pi does accept the `@version` suffix — verified), but Pi writes its own generated manifest as `^2.15.0` regardless, and `save-exact` does not change it. Because that manifest and lock are gitignored and regenerated from `settings.json`, a fresh clone still resolves exactly 2.15.0; only a `pi update` inside an existing `.pi/npm` could drift within 2.x.

## Exit path

This now ships a real feature, so the normal flow applies (reversing the dogfood's exit path):

- **Rule 4 applies** — the daemon feature needs L2 tests, including a PTY-attached one (M1.4).
- **Changelog fragment** — a `changelog.d/126.feature.md` (feature; non-breaking target). Include the demo-reel link if one is produced.
- **Version bump** — per the release's policy; the feature rides this release's shared version.
- **Rule 12** — cross-version manual test to confirm non-breaking (M1.5).
- Rules 2 and 10 still apply (fmt/clippy before commit; no hard-wrapped Markdown prose).

## Background: the dogfood that led here

The config-only Phase 1 (ntfy over `curl`, wrapped in `scripts/notify.sh`, now being retired) verified the channel was reachable and fire-and-forget, and produced the findings that justify this rescope:

- **Telegram reachability is proven from Pi and Claude.** After the rescope to Telegram/MCP, live sends succeeded from the **Pi orchestrator** (via the `pi-mcp-adapter`, message_id 17) and from **Claude** (native `.mcp.json` discovery, message_id 18) to the maintainer's chat. `pi-mcp-adapter@2.15.0` (MIT) was source-reviewed clean and installed project-local (`.pi/settings.json`); `telegram` was added to `.mcp.json` with an env-placeholder token wired via `vals`.
- **"One `.mcp.json` for every agent" is a myth** — only Claude reads it natively; OpenCode uses `opencode.json`, Codex uses `~/.codex/config.toml`. This is why **orchestrator-only** notifications are the right design: only the orchestrator (Pi, which now has the adapter) needs to send, so the other agents' MCP-config divergence is irrelevant.
- **The felt need for the idle detector** was the decisive tripwire: the maintainer immediately wanted "notify me if a worker goes silent for too long", which config provably cannot deliver — the origin of milestone M1.
- Earlier dogfood tripwire thoughts (T1–T5) — wrapping `blocked` inside `work-done`, teeing the scheduler `Notifier` seam into the log, an `agent_notification_hint` field, a `[notifications]` config block for a private topic — remain recorded as design inputs; several are now moot (orchestrator-only + MCP) or superseded by the idle-detector decision.

Two security notes carried forward: every stdio MCP server declared in `.mcp.json` is spawned with the full parent environment (standard MCP-host behavior — all secrets visible to each server); and the MCP SDK carries 4 moderate, Windows-only transitive advisories. Both accepted.

## Key Files

- `src/daemon_protocol.rs`, the daemon, and the session prompt/input-injection path — where the idle-worker detector and its delivery live.
- `.dot-agent-deck.toml` — the new `worker_response_timeout_minutes` knob and this repo's rewritten orchestrator `prompt_template`.
- `.mcp.json` — the `telegram` MCP server (added); `.pi/settings.json` — the `pi-mcp-adapter` install.
- `docs/` + `site/sidebars.js` — the published feature/recipe page.
- `scripts/notify.sh`, `docs/develop/notifications-dogfood.md` — dogfood artifacts to remove/retire.

## Risks and Mitigations

- **False-positive idle alerts on legitimately long tasks.** *Mitigation:* 120-min default makes it rare; a false alert is a discardable message; one-shot avoids spam. Liveness-based detection deferred.
- **The change accidentally breaks TUI↔daemon compatibility.** *Mitigation:* reuse the existing injection path (no new wire shape); rule-12 cross-version manual test gates it.
- **Idle prompt injected mid-turn or racing a finish.** *Mitigation:* deliver only at a turn boundary; per-delegation timer cancelled by `work-done`.
- **Post-compaction silence of the recipe notifications.** *Mitigation:* the daemon's injected idle prompt is self-describing (survives compaction); gate/done notifications' compaction-mortality is a documented known limitation (#82).
- **Publishing the recipe before it is battle-tested.** *Mitigation:* frame it as an example, and validate on real runs (M5) including a compacting one.
- **The no-agent / dead-orchestrator gap.** *Mitigation:* explicitly out of scope and documented as a known limitation.
