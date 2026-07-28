# PRD #126: Agent-driven notifications — dogfood on dot-agent-deck development

**Status**: Planning — narrowed 2026-07-25 to a no-code dogfood (see [Scope decision (2026-07-25)](#scope-decision-2026-07-25))
**Priority**: Medium
**Created**: 2026-05-25
**GitHub Issue**: [#126](https://github.com/vfarcic/dot-agent-deck/issues/126)
**Closes**: [#99](https://github.com/vfarcic/dot-agent-deck/issues/99) (supersedes prior orchestrator-only design)
**Related**: PRD #8 (terminal bell — per-session, in-terminal), PRD #20 (multi-agent support), PRD #82 (orchestrator role reinforcement — see [Interaction with PRD #82](#interaction-with-prd-82)), PRD #201 (Pi integration)

## Validation refresh (2026-06-14)

Re-validated against current code — verdict: **current**. Nothing here has shipped yet (no `src/notifications.rs`, no `[notifications]` config block, no inactivity timer). PRD #99 is correctly closed/superseded by this PRD.

## Scope Decision (2026-06-22)

Rescoped after review. The happy path of this PRD — **agents notify the user via their own tools** — requires *no deck code*: a notification MCP/CLI on the agent (Slack MCP, ntfy, `osascript`, …) plus a notify-on-done/blocked/needs-input instruction injected at spawn. For a single project that instruction can go straight into each role's `prompt_template` in `.dot-agent-deck.toml`; an `agent_notification_hint` config field would only generalize it to ad-hoc panes and scheduled tasks that have no `prompt_template`.

**Deferred until proven needed** — the two deck-side *safety nets*, being the only parts that genuinely cannot be done with MCP + config: the **inactivity nudge** (a timer plus inactivity-fired prompt injection, for a stuck-but-quiet agent that cannot self-notify) and the **no-agent fallback** (a minimal local desktop channel for pre-spawn scheduler failures and silent crashes, where there is no agent to delegate to at all).

## Scope decision (2026-07-25)

Narrowed again. **This PRD's entire deliverable is now: make agent-driven notifications work for `dot-agent-deck`'s own development, changing no deck code.** Deciding whether the deck should be extended is explicitly deferred to a discussion *after* this ships, informed by what the dogfood shows.

Three corrections to the prior scope motivated this:

1. **The "revisit if unreliable" trigger had no source of evidence.** The 2026-06-22 rescope deferred the safety nets pending real usage, but nothing in the PRD produced that usage. This narrows the PRD to the thing that generates it.
2. **The #120 prerequisite is stale.** #126 was listed as a prerequisite for PRD #120 (scheduled issue dispatch). #120 is **closed** — it shipped without this PRD. Nothing downstream is blocked by narrowing.
3. **The "no `Notifier` trait in the deck" line was overtaken by events.** PRD #127 shipped `trait Notifier` / `NotifyEvent` (`src/scheduler.rs:38-45`) with `StderrNotifier` (`:104`) and live call sites in `src/spawn.rs` and `src/issue_dispatch_run.rs`. That seam is scheduler-internal, not a pluggable user-facing channel set, so this PRD's architectural objection to #99 still stands — but the wording was wrong and the seam is not to be touched here.

### The hard constraint

**No changes to `dot-agent-deck` source code.** Not a hook, not a config field, not "a tiny helper". Everything lands in this repo's `.dot-agent-deck.toml`, in `docs/develop/`, and in this PRD's findings section.

**Tripwire:** if implementing this produces the thought "I'll just add a small thing to the deck" — stop, record the thought in the findings, and carry it into the Phase 3 discussion. That thought *is* the output of this PRD. Acting on it mid-flight destroys the evidence.

### What this cannot tell us (stated up front, so the result is not over-read)

The two deferred safety nets cover the cases where **no agent exists to delegate to**: a stuck-but-alive agent that cannot self-report, and a pre-spawn scheduler failure or silent crash where no process remains. Configuring `prompt_template`s produces **zero** data about either. A green dogfood is not evidence that the fallbacks are unnecessary.

Those two must therefore be decided on reasoning rather than on this PRD's data — and for the scheduler-failure case the reasoning already looks settled: unattended scheduling with no human at a terminal has no other delivery path. Phase 3 records the decision either way; it does not pretend the dogfood informed it.

## Problem Statement

Long-running agents and scheduled tasks leave users with no reliable out-of-band signal that they need attention. The terminal bell (PRD #8) only reaches a focused terminal — useless once the user walks away.

The original PRD #99 proposed a pluggable `Notifier` trait plus four channel implementations (desktop, webhook, Slack, email) wired to an orchestrator-completion hook. Three things are wrong with that:

1. **The deck would re-implement what the ecosystem already provides,** and would become a credential holder for third-party services it has no business holding credentials for.
2. **The event source was too narrow** — orchestrator completion only, missing scheduled single-agent tasks, agent-side "blocked / needs input", and scheduler-side failures before any agent spawns.
3. **Only the agent knows what "done" means in context.** A lifecycle hook can fire "orchestration ended"; the agent knows whether it finished, hit a wall, or needs a decision. Inferring that from outside is heuristic; declaring it from inside is explicit.

### Why `dot-agent-deck`'s own development is the right first target

This repo's orchestration has two **explicit** stopping points where the workflow halts and waits for a human — the test-plan approval (step 1) and the merge confirmation (step 7) of the orchestrator `prompt_template` (`.dot-agent-deck.toml:77-124`) — plus a long unattended stretch in step 5, where `release` opens the PR and waits for CI and Greptile to settle before reporting back. Those are exactly the walk-away moments this PRD exists for. If notifications do not pay off here, they will not pay off anywhere.

## Scope

### In Scope

- **A channel decision for this project** — one destination and one delivery mechanism, chosen, verified, and recorded (not left as a menu in user docs).
- **An expectation log** — the instrumentation that makes the experiment falsifiable (see below).
- **Notify instructions in this repo's `.dot-agent-deck.toml`** role `prompt_template`s, anchored to the workflow's real waiting points.
- **A findings record** appended to this PRD.
- **A contributor note** under `docs/develop/` describing the setup, so it is reproducible by anyone working on this repo (per CLAUDE.md rule 11 this is developer-facing and must not be published to the site).

### Out of Scope

- **Any change to `dot-agent-deck` source code.** See [The hard constraint](#the-hard-constraint).
- **The `agent_notification_hint` config field.** It only generalizes the hint to panes and scheduled tasks that have no `prompt_template`; this repo's roles all have one. Revisit in Phase 3.
- **The inactivity nudge and the no-agent desktop fallback.** Unchanged from the 2026-06-22 deferral; see [What this cannot tell us](#what-this-cannot-tell-us-stated-up-front-so-the-result-is-not-over-read).
- **User-facing documentation of the pattern** (`docs/notifications.md`). Do not document a recommended practice before knowing it works. Phase 3 decides whether it graduates from `docs/develop/` to published docs.
- **No `Notifier` trait, dispatcher, pluggable channels, per-event routing, deck-side credentials, agent-callable `notify` CLI, or two-way channels** — the architectural inversion of #99 stands.

### The experiment must be able to fail

**This is the design's load-bearing detail.** The failure mode under test is the *absence* of a signal: when an agent forgets to notify, compacts the instruction away, or dies first, the observable outcome is nothing — indistinguishable from "working correctly, nothing to report yet". Without instrumentation this PRD cannot produce negative evidence, and "it seemed fine" is not a finding.

So every notify instruction is paired with a local append to an expectation log — the agent records *that it reached a notifiable moment* separately from *sending the notification*. Two independent records; the delta is the data.

- Log file: `.dot-agent-deck/notify-log.md` (gitignored), one line per event: timestamp, role, event kind (`gate` / `done` / `blocked`), and whether the send was attempted.
- The gap that matters is **reached-but-never-sent**, and the second gap — **sent-but-never-arrived** — is caught by reconciling the log against what actually showed up in the destination.
- Reconciling is manual and that is fine at this scale; automating it would require deck code.

## Success Criteria

- A channel is chosen and **verified reachable from every agent this repo runs** — the Pi orchestrator *and* the Claude workers — or the failure to reach one of them is recorded as a finding.
- Reaching the test-plan gate (step 1) and the merge gate (step 7) produces a notification that arrives on a device that is not the terminal running the deck.
- The expectation log exists, is written at every notifiable moment, and is reconciled against arrived notifications after each run.
- At least **three** orchestrated PRD runs are observed, **including at least one long enough for the orchestrator to compact** (so the PRD #82 interaction is exercised rather than assumed).
- The findings section records, with counts: moments reached, notifications attempted, notifications arrived, and every gap — plus any "I want to change the deck for this" thoughts the tripwire caught.
- **Zero diff under `src/`.** This is checkable and non-negotiable.
- A Phase 3 decision is recorded for each deferred deck-side item, with its basis (dogfood evidence vs. reasoning) stated explicitly.

## Design notes

### Prefer a CLI over an MCP

This repo does not run one agent. The orchestrator is Pi (`command = "devbox run pi-big"`, `.dot-agent-deck.toml:74`) while the workers are Claude. #126 was originally written against Claude / Codex / Gemini / Aider and never covered Pi (PRD #201).

A shell-callable CLI (`ntfy` / `curl` to a topic, or a two-line script in the repo) works for **any** agent with shell access, uniformly, with no per-agent MCP configuration. An MCP has to be wired up per agent and Pi may not support one at all. So the CLI is the default choice; Slack MCP led the original write-up mostly because it was already installed.

Fallback ladder if the Pi orchestrator cannot reach the chosen channel: (1) a plain CLI it can shell out to; (2) switch the orchestrator to the all-Claude command already sitting commented at `.dot-agent-deck.toml:75`; (3) record "orchestrator cannot notify" as the headline finding — it would be the single most important result this PRD could produce, since the orchestrator owns both user gates.

### Where to fire

| Role | Moment | Kind |
|---|---|---|
| `orchestrator` | test plan posted, waiting for approval (step 1) | `gate` |
| `orchestrator` | merge confirmation reached (step 7) | `gate` |
| `orchestrator` | run finished or abandoned | `done` |
| `coder` / `tester` / `reviewer` / `auditor` | blocked or missing critical context (already surfaced in work-done) | `blocked` |
| `release` | PR checks + Greptile settled after the step 5 wait | `done` |

### Fire-and-forget, always

The instruction must read "send and continue", never "notify and wait for acknowledgment". An orchestrator that blocks on a notification it cannot confirm is a worse failure than no notification at all.

### Interaction with PRD #82

The notify instruction lives in the orchestrator's `prompt_template`, which is delivered as a `Read` tool result and is therefore **compaction-mortal** — the exact mechanism PRD #82 documents. Expect notifications to stop after the first compaction.

Do not misread that as "agents are unreliable at notifying". It is the same root cause, and it makes this PRD a second, independently-observable symptom of #82 — which is useful to both. #82's post-compaction re-assert would restore the notify instruction along with the role, so if the dogfood shows a clean before/after-compaction split, that is evidence *for #82*, not for building notification machinery here.

## Milestones

### Phase 1 — Decide and configure (no deck code)

- [ ] **M1.1** — Choose the destination and the delivery mechanism. Verify reachability from the Pi orchestrator and from a Claude worker before committing to it; walk the fallback ladder if Pi cannot reach it. Record the choice and the verification in this PRD.
- [ ] **M1.2** — Define the expectation-log format and add the "append to `.dot-agent-deck/notify-log.md`" step to the same instructions that trigger a notification. Confirm the path is gitignored.
- [ ] **M1.3** — Extend this repo's `.dot-agent-deck.toml` role `prompt_template`s with the notify + log instructions per the [Where to fire](#where-to-fire) table, phrased fire-and-forget.
- [ ] **M1.4** — Contributor note under `docs/develop/` documenting the setup and how to reproduce it. Not published (CLAUDE.md rule 11); linked from `CONTRIBUTING.md`.

### Phase 2 — Run and observe

- [ ] **M2.1** — Run at least three orchestrated PRDs under the configuration, including one long enough to compact. Do not tune the instructions mid-run; a change resets the sample.
- [ ] **M2.2** — Reconcile the expectation log against arrived notifications after each run. Append a findings section to this PRD with counts and every gap, plus any tripwire thoughts.

### Phase 3 — Decide what, if anything, the deck should do

- [ ] **M3.1** — Review the findings with the maintainer. For each deferred deck-side item (inactivity nudge, no-agent desktop fallback, `agent_notification_hint` field), record a decision **and its basis** — dogfood evidence where the dogfood could speak, explicit reasoning where it structurally could not.
- [ ] **M3.2** — File follow-up issues for whatever Phase 3 approves. Nothing deferred may evaporate silently when this PRD closes.
- [ ] **M3.3** — Decide whether the `docs/develop/` note graduates into published user docs under `docs/`.

## Exit path (decide now, not at `/prd-done`)

This PRD ships **no product change**, which makes the standard flow a poor fit. Expected shape:

- **No changelog fragment** — nothing user-facing changed. **No version bump. No release.**
- **No test-plan gate with tests in it** — there is no deck behavior to cover, so CLAUDE.md rule 4 does not apply. `cargo test-fast` must still pass (it should be untouched).
- **No demo reel** — the branch changes no e2e tests, so the adapter clean-skips by design.
- **No cross-version contract check** (rule 12) — no daemon, protocol, orchestration-code, or hook change.
- Merges as a config + docs commit; closes without a release once Phase 3's follow-ups are filed.

Rules 2 and 10 still apply: `cargo fmt --check` and `cargo clippy -- -D warnings` before any commit, and no hard-wrapped Markdown prose.

## Key Files

- `.dot-agent-deck.toml` — this repo's orchestration roles; the orchestrator `prompt_template` (`:77-124`), its two user gates (`:117`), and the Pi/Claude command split (`:74-75`).
- `docs/develop/` — where the contributor note lands (developer-facing, excluded from the Docusaurus build).
- `.dot-agent-deck/notify-log.md` — the expectation log (gitignored, created at runtime).
- `src/scheduler.rs:38-45,104` — the existing scheduler-internal `Notifier` / `NotifyEvent` / `StderrNotifier` seam. **Referenced so it is not disturbed**; this PRD does not touch it.

## Risks and Mitigations

- **Risk**: the experiment produces no falsifiable result because missed notifications are invisible.
  - *Mitigation*: the expectation log — the whole reason it exists. If M1.2 is skipped, the PRD is worthless; treat it as gating.
- **Risk**: results are read as a verdict on the deferred deck-side items, which the design cannot inform.
  - *Mitigation*: stated up front in the scope decision and re-stated in M3.1, which forces the basis of each decision to be named.
- **Risk**: N=1 project, one maintainer, one workflow — findings may not generalize to users on headless boxes running unattended schedules.
  - *Mitigation*: accepted and recorded. Directional evidence for a decision that is currently being made on none.
- **Risk**: scope leak into deck code once a rough edge appears.
  - *Mitigation*: the tripwire, plus the checkable "zero diff under `src/`" success criterion.
- **Risk**: a badly phrased instruction makes the orchestrator notify constantly, or stall waiting for acknowledgment.
  - *Mitigation*: fire-and-forget phrasing; gates are the primary trigger rather than per-step chatter; the whole change is one revertible TOML commit.
- **Risk**: post-compaction silence gets attributed to notifications rather than to PRD #82.
  - *Mitigation*: at least one run must be long enough to compact, and findings must record before/after-compaction separately.

## Validation Strategy

Manual, per `feedback_validate_pre_pr`: three real orchestrated runs with log reconciliation after each (M2.1/M2.2). No automated tests — there is no deck behavior under test, and adding some would require deck code the constraint forbids. Regression surface is limited to this repo's own orchestration behaving as before, minus the new notifications; `cargo test-fast` stays green because nothing under `src/` moves.

## Findings

_Populated by M2.2. Empty until the dogfood has run._
