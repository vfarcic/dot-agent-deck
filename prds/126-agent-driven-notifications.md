# PRD #126: Agent-driven notifications — dogfood on dot-agent-deck development

**Status**: In progress — Phase 1 (configure) complete 2026-07-25; Phase 2 (run and observe) is next. Narrowed 2026-07-25 to a no-code dogfood (see [Scope decision (2026-07-25)](#scope-decision-2026-07-25))
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

- A channel is chosen and **verified reachable from every agent this repo runs** — all four families: Pi (`orchestrator`, `auditor`), Claude (`coder`, `release`), OpenCode (`reviewer`), and Codex (`tester`) — or the failure to reach one of them is recorded as a finding.
- Reaching the test-plan gate (step 1) and the merge gate (step 7) produces a notification that arrives on a device that is not the terminal running the deck.
- The expectation log exists, is written at every notifiable moment, and is reconciled against arrived notifications after each run.
- At least **three** orchestrated PRD runs are observed, **including at least one long enough for the orchestrator to compact** (so the PRD #82 interaction is exercised rather than assumed).
- The findings section records, with counts: moments reached, notifications attempted, notifications arrived, and every gap — plus any "I want to change the deck for this" thoughts the tripwire caught.
- **Zero diff under `src/`.** This is checkable and non-negotiable.
- A Phase 3 decision is recorded for each deferred deck-side item, with its basis (dogfood evidence vs. reasoning) stated explicitly.

## Design notes

### Prefer a CLI over an MCP

This repo does not run one agent — it runs **four different agent CLIs**, and every one of them carries a notify instruction:

| Family | Roles | Command (`.dot-agent-deck.toml`) |
|---|---|---|
| Pi | `orchestrator`, `auditor` | `devbox run pi-big` |
| Claude | `coder`, `release` | `devbox run agent-orchestrator`, `devbox run agent-release` |
| OpenCode | `reviewer` | `devbox run oc-big` |
| Codex | `tester` | `devbox run codex-big` |

This PRD was originally written against Claude / Codex / Gemini / Aider and never covered Pi (PRD #201) or OpenCode.

A shell-callable CLI (`ntfy` / `curl` to a topic, or a two-line script in the repo) works for **any** agent with shell access, uniformly, with no per-agent MCP configuration. An MCP has to be wired up per agent, and wiring one into four different CLIs — one of which (Pi) may not support MCPs at all — is exactly the per-agent cost the CLI avoids. So the CLI is the default choice; Slack MCP led the original write-up mostly because it was already installed. **This four-family spread is the single strongest argument for the CLI**, and it only became visible once reachability was checked against each launcher rather than assumed from the orchestrator's.

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

- [x] **M1.1** — Choose the destination and the delivery mechanism. Verify reachability from **every** agent family this repo runs — Pi, Claude, OpenCode, and Codex — before committing to it; walk the fallback ladder if Pi cannot reach it, and record any family that cannot self-notify as a finding. Record the choice and the verification in this PRD. → see [M1.1 record](#m11-record-channel-choice-and-verification-2026-07-25).
- [x] **M1.2** — Define the expectation-log format and add the "append to `.dot-agent-deck/notify-log.md`" step to the same instructions that trigger a notification. Confirm the path is gitignored. → see [M1.2 record](#m12-record-expectation-log-format).
- [x] **M1.3** — Extend this repo's `.dot-agent-deck.toml` role `prompt_template`s with the notify + log instructions per the [Where to fire](#where-to-fire) table, phrased fire-and-forget. → see [M1.3 record](#m13-record-where-the-instructions-landed).
- [x] **M1.4** — Contributor note under `docs/develop/` documenting the setup and how to reproduce it. Not published (CLAUDE.md rule 11); linked from `CONTRIBUTING.md`. → `docs/develop/notifications-dogfood.md`, linked from `CONTRIBUTING.md`.

#### M1.1 record — channel choice and verification (2026-07-25)

**Chosen**: a plain `curl` POST to the public ntfy.sh topic **`dot-agent-deck-notify-0c0d15e13936d122`**, wrapped in a repo helper script `scripts/notify.sh` (`scripts/notify.sh <gate|done|blocked> <role> '<message>'`). No MCP, no account, no API key, no OAuth. Overridable per-machine via `DOT_AGENT_DECK_NOTIFY_TOPIC` / `DOT_AGENT_DECK_NOTIFY_SERVER` without touching code or config.

**Verified reachable from all four agent families — the fallback ladder was not needed.** The first pass covered only Pi and Claude; review correctly caught that this repo also runs the `reviewer` through OpenCode and the `tester` through Codex, and that both receive `blocked` notify instructions. All four were then exercised against the real topic, and the Pi and Claude checks were re-run against the corrected script (see [Post-review corrections](#post-review-corrections-2026-07-25)):

| Family | Role | How | Exit | Log rows | HTTP |
|---|---|---|---|---|---|
| — | — | `curl -w '%{http_code}'` POST straight to the topic from a maintainer shell | 0 | n/a | **200**, body `{"id":"8lCxOr4PnHzO","event":"message","topic":"dot-agent-deck-notify-0c0d15e13936d122",…}` |
| **Claude** | `coder` | `./scripts/notify.sh gate coder '…'` from the worker's own shell | **0** | `reached` + `send=ok`, id `inv-6a6541ca-322ccc-50b3` | **200** |
| **Claude** | `release` | prompt piped to `devbox run -- claude -p --model haiku --allowedTools Bash` | **0** | `reached` + `send=ok`, id `inv-6a6542df-3269a3-5d01` | **200** |
| **Pi** | `orchestrator` | `devbox run -- pi -p --model anthropic/claude-haiku-4-5 --approve '…run ./scripts/notify.sh…'` | **0** | `reached` + `send=ok`, id `inv-322b97-0e3f` | **200** |
| **OpenCode** | `reviewer` | `devbox run -- opencode run --model openai/gpt-5.4-mini '…run ./scripts/notify.sh…'` | **0** | `reached` + `send=ok`, ids `inv-322842-79cf` and `inv-3229ba-63d3` | **200** |
| **Codex** | `tester` | `devbox run -- codex exec --model gpt-5.6-sol --sandbox workspace-write -c sandbox_workspace_write.network_access=true '…run ./scripts/notify.sh…'` | **0** | `reached` + `send=ok`, id `inv-2-230f` | **200** |

Each check used the launcher's **non-interactive** form with a cheap model rather than the interactive command the deck spawns (`pi -p`, `claude -p`, `opencode run`, `codex exec`); the bash tool being exercised is the same one, so what is verified is *shell reach*, which is the only thing at issue. The per-agent commands are documented in `docs/develop/notifications-dogfood.md` so this is reproducible rather than a one-off claim.

Two incidental observations from the OpenCode and Codex runs, both already folded back into the script and docs:

- **OpenCode fired the helper twice** — its first invocation printed nothing, so it re-ran the command wrapped in `printf '%s' $?` to read the exit code. Harmless here, but it is a live demonstration of why `reached`/`send` rows need an invocation id: two invocations by the same role, five seconds apart, produce four rows that no timestamp can pair up.
- **Codex reported PID 2** — it runs inside a PID namespace and reliably gets a low PID, so PIDs collide across sandboxed agents. The invocation id therefore mixes epoch + PID + `$RANDOM` rather than relying on the PID alone.
- **Claude needs the prompt on stdin** under `devbox run --`, which swallows a positional prompt and makes `claude -p` exit with "Input must be provided either through stdin or as a prompt argument". Only affects the verification harness, not the deck's interactive spawn; documented so the reproduction steps work as written.

**No agent failed to self-notify.** The success criterion's escape hatch ("or the failure to reach one of them is recorded as a finding") was not needed: all four families have a scriptable non-interactive path and all four reached the topic.

**Failure path verified too** (so the log's negative evidence is trustworthy): pointed at an unreachable server, the helper still wrote the `reached` row, recorded `send=failed http=000 curl_exit=7`, and **exited 0**. With no arguments at all it also exits 0. Fire-and-forget holds by construction, not by convention.

**Not yet verified — arrival on a non-terminal device.** HTTP 200 proves ntfy *accepted* the message; it does not prove it *arrived* anywhere. Closing that requires the maintainer to subscribe a phone or browser to the topic, which is a Phase 2 (M2.1) manual prerequisite and the reason gap #2 below is reconciled by eye.

**Accepted caveat — the topic is public.** ntfy.sh topics are unauthenticated: anyone who knows the name can read *and* publish to it, and the name is committed in a public repo. The random suffix prevents guessing, not lookup. Mitigation is payload discipline, enforced in the script's header comment and the docs: role + event kind + one short sentence, never secrets, tokens, diffs, or file contents. Acceptable for a payload that is literally "a gate was reached"; would not be acceptable for anything else. Noted as a real (if small) argument for a deck-side config field in Phase 3 — see tripwire thought T4.

#### M1.2 record — expectation-log format

File: `.dot-agent-deck/notify-log.md`. **Gitignored** — confirmed by `git check-ignore -v`, which resolves to `.gitignore:6:.dot-agent-deck/` (the pre-existing blanket rule for per-clone dev state). No `.gitignore` change was needed.

Format is a Markdown table — renders on GitHub, greps cleanly, one event per line:

```markdown
| timestamp (UTC) | invocation | role | kind | record | detail |
|---|---|---|---|---|---|
| 2026-07-25T23:07:54Z | inv-6a6541ca-322ccc-50b3 | coder | gate | reached | helper script send path from a Claude worker shell |
| 2026-07-25T23:07:55Z | inv-6a6541ca-322ccc-50b3 | coder | gate | send=ok | http=200 topic=dot-agent-deck-notify-0c0d15e13936d122 |
```

`scripts/notify.sh` appends **two** rows per invocation, which is how "reached a moment" stays independent from "attempted a send":

1. `reached` — written **before** the send is attempted, so it survives a dead network, a missing `curl`, or the agent being killed mid-send.
2. `send=ok` / `send=failed` / `send=skipped` — written **after**, carrying the HTTP status and `curl` exit code.

Both rows carry the same **invocation id**, generated before the first append. Timestamps are second-resolution and roles run concurrently, so id is the only reliable key for pairing a `reached` with its send outcome — the OpenCode check above produced exactly the interleaving case that motivates it.

The message is sanitized (newlines and `|` collapsed) so one event can never become two rows or break the table; the `DOT_AGENT_DECK_NOTIFY_TOPIC` override goes through the same sanitizer, since it lands in both the request URL and the log's detail column.

**Logging is best-effort, and the docs say so.** `append` cannot be made to succeed against a read-only checkout, a bad `DOT_AGENT_DECK_NOTIFY_LOG`, or a full disk, and fire-and-forget forbids failing the caller over it. So it keeps the caller-facing exit 0 but prints `notify.sh: could not …` on **stderr**, plus a closing `N of 2 expectation-log rows were lost for <id>`. That is what keeps "the agent never invoked the helper" distinguishable from "the helper ran but could not log" — the two would otherwise be the same silence. A pre-run check that invokes the helper and confirms both rows appear at the real log path is documented as a prerequisite.

Reconciliation procedure and the three gaps are documented in `docs/develop/notifications-dogfood.md`, including the per-run window (log line range), the definition of *attempted* as `send=ok` + `send=failed` (excluding `send=skipped`), the manual device-arrival tally, and the exact pre/post-compaction table to append to [Findings](#counts-and-gaps). Note that gap #3, *never even reached*, is recorded by **omission** and must be reconstructed from the run's actual shape, because the log cannot report its own absence.

#### M1.3 record — where the instructions landed

All in `.dot-agent-deck.toml` role `prompt_template`s, anchored **inline at each role's real waiting point** rather than collected in a preamble, so each instruction reads at the moment it applies:

| Role | Anchor | Kind |
|---|---|---|
| `orchestrator` | step 1, on the same line as "Surface the plan … and STOP" | `gate` |
| `orchestrator` | step 7, on the same line as "Then pause — the user reviews the PR" | `gate` |
| `orchestrator` | new "**Run finished.**" paragraph after the two-user-gates note | `done` |
| `release` | on the "Once the PR is open, CI is green, and Greptile's review has settled … STOP" line, fired *before* the work-done report | `done` |
| `coder` / `reviewer` / `auditor` | appended to each role's existing "if critical context is missing" sentence | `blocked` |
| `tester` | new final paragraph, covering both "blocked" and the existing out-of-harness-reach report-back | `blocked` |

Phrasing is fire-and-forget throughout — every site says "send and continue", "ignore its result", "never wait for an acknowledgment". No site says notify-and-wait. The orchestrator also carries an explicit **"do not notify on anything else"** line (per-step chatter would make the signal worthless) and an instruction to **tell the user in its next message if it reached a moment but could not run the script at all** — that self-report is the only way the reached-but-not-even-logged case leaves a trace.

#### Post-review corrections (2026-07-25)

Phase 1 was reviewed and security-audited before Phase 2 started. Both passes agreed on one **blocker**, and it is worth recording because it is a genuine finding about this design rather than a typo:

- **`curl --data-binary "$message"` could exfiltrate a local file to the public topic.** `--data-binary` gives a leading `@` the meaning "read this file"; `@-` reads stdin. Agents pass free-form prose, and a message that begins with a path or an @mention is entirely plausible — so `scripts/notify.sh blocked coder '@/etc/passwd'` would have POSTed the file's contents to a world-readable topic, the exact thing the script header promises never happens. Fixed by switching to `--data-raw`, which sends the `@` literally. Verified against a local HTTP sink: `--data-binary "@<file>"` delivered the file's contents, `--data-raw "@<file>"` delivered the literal path, and the helper now delivers the literal path.
  **This is a finding for Phase 3, not just a fix.** The riskiest part of the no-code approach turned out to be neither the agents nor the transport but the *payload plumbing* in the glue script — the one component a deck-side implementation would have owned and gotten right once, instead of once per project that copies this pattern.

Also addressed, all in the glue rather than the deck: the topic override now goes through the same sanitizer as the message (a crafted env var could otherwise inject a spurious log row); both log rows carry an invocation id; and logging failures are reported on stderr instead of being silently swallowed. Zero `src/` diff throughout.

### Phase 2 — Run and observe

- [ ] **M2.1** — **Prerequisite (manual, maintainer):** subscribe a device that is *not* the terminal running the deck to the topic — ntfy phone app or `https://ntfy.sh/dot-agent-deck-notify-0c0d15e13936d122` in a browser — *before* the first run. Phase 1 verified the send path (HTTP 200) but arrival is unverifiable from a shell, and an unsubscribed topic turns every successful send into an apparent miss. Also run the **pre-run log check** (`docs/develop/notifications-dogfood.md`) — invoke the helper once and confirm both rows land at the real log path with no `notify.sh:` stderr note — since logging is best-effort and a silently broken log makes every gap this run produces uninterpretable. Record the log's line count before starting: the reconciliation window is a line range. Then run at least three orchestrated PRDs under the configuration, including one long enough to compact. Do not tune the instructions mid-run; a change resets the sample.
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

- `.dot-agent-deck.toml` — this repo's orchestration roles; the orchestrator `prompt_template`, its two user gates, the Pi/Claude command split, and (as of Phase 1) the inline notify instructions in every role.
- `scripts/notify.sh` — the fire-and-forget helper: `curl` to the ntfy topic plus the two-record append to the expectation log. Always exits 0.
- `docs/develop/notifications-dogfood.md` — the contributor note (developer-facing, excluded from the Docusaurus build, linked from `CONTRIBUTING.md`).
- `.dot-agent-deck/notify-log.md` — the expectation log (gitignored via `.gitignore:6`, created at runtime).
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

### Counts and gaps

_Populated by M2.2. Empty until the dogfood has run._ The reconciliation must record, per run and split before/after any compaction: moments reached, notifications attempted (`send=ok` + `send=failed`, with `send=skipped` counted separately since no request left the machine), notifications arrived on the subscribed device, and each of the three gaps (`reached`-but-not-`send=ok`, identified by invocation id; `send=ok`-but-never-arrived; the moment that produced no row at all). The exact table to append here, the per-run log-line window that bounds the counts, and the commands that produce them are in [`docs/develop/notifications-dogfood.md`](../docs/develop/notifications-dogfood.md#the-record-to-append-to-the-prd) — do not invent a shape at reconciliation time.

### Tripwire thoughts caught during Phase 1 (2026-07-25)

Recorded per [the tripwire](#the-hard-constraint), **not acted on**, and carried into the M3.1 discussion. Each of these was a live "I'll just add a small thing to the deck" impulse while wiring Phase 1:

- **T1 — "The `blocked` notify would be far more reliable as a wrapper around `dot-agent-deck work-done` than as an instruction each worker has to remember."** Every worker already calls `work-done` to finish, and its `--task` text already contains the blocked reason; firing the notification from inside that CLI would make a missed `blocked` notification structurally impossible. **This is precisely the evidence-destroying change the tripwire exists for** — it would move the notification from "the agent chose to do it" to "the deck did it", which is the exact hypothesis under test. Worth noting that it also quietly re-answers PRD #99's question in #99's favour, for the `blocked` event only.
- **T2 — "Reconciliation would be trivial if the deck emitted its lifecycle events into the same log."** The scheduler-internal `Notifier` / `NotifyEvent` seam (`src/scheduler.rs:38-45`) already knows when a run starts, finishes, and fails; teeing those into `.dot-agent-deck/notify-log.md` would give an independent ground truth for "which moments actually occurred", collapsing gap #3 from manual reconstruction to a diff. Deliberately not done — the seam is explicitly off-limits here, and the manual reconstruction is the honest version at N=3 runs.
- **T3 — "`prompt_template` is compaction-mortal, so an `agent_notification_hint` re-injected at each delegate would fix the thing we already predict will break."** This is the deferred config field resurfacing under the guise of a bug fix. It must not be added mid-flight: the predicted post-compaction silence is a *result* of this experiment (and a second independent symptom of PRD #82), and pre-emptively fixing it destroys that result. If the log shows a clean before/after-compaction split, the correct follow-up is #82's re-assert, not this field.
- **T4 — "Having to commit a public topic name into a public repo is a wart a `[notifications]` config block would solve."** A deck-side config field (or an env passthrough the deck owns) would let the topic be private per-machine while keeping zero-setup defaults. Real, but small: the same effect is already available via `DOT_AGENT_DECK_NOTIFY_TOPIC`, at the cost of the maintainer remembering to export it. Recorded so Phase 3 weighs it as an *ergonomics* argument, not a capability one.
- **T5 — non-tripwire, but adjacent.** Nothing in Phase 1 produced a "the deck must do this or it cannot work" thought. Every gap encountered had a no-code workaround, and the two genuinely deck-shaped items are still the pair [the scope decision names as un-informable by this dogfood](#what-this-cannot-tell-us-stated-up-front-so-the-result-is-not-over-read) — the inactivity nudge and the no-agent fallback. Phase 1 neither strengthens nor weakens the case for either, exactly as predicted.

### Zero-diff check

`git diff --stat -- src/` is empty on this branch. Phase 1 touched only `.dot-agent-deck.toml`, `scripts/notify.sh` (new), `docs/develop/notifications-dogfood.md` (new), `CONTRIBUTING.md`, and this PRD. `.gitignore` needed no change (`.dot-agent-deck/` already covers the log).
