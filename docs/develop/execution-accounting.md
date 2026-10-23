# Execution accounting: signals, record and outcomes

This page is the evidence for PRD #633's first three discovery milestones: which signals each supported agent exposes about a run, a minimal provider-neutral record built from them, and an outcome taxonomy checked against runs that already happened in this repository. It builds on PRD #1741, which shipped the deck's OpenTelemetry run traces ([`telemetry.md`](telemetry.md), the user page [`docs/telemetry.md`](../telemetry.md), `src/telemetry/`), and on #1692's resource measurements, folded into #633. It is a discovery record written on 2026-10-23, not a description of shipped behaviour: nothing on this page is implemented beyond what #1741 shipped.

## How the evidence was gathered

Every number below comes from data already on the maintainer's development machine and from this repository's GitHub history. No agent session was started to produce it.

| Source | What was read | Runs in this repository |
| --- | --- | --- |
| Claude Code transcripts, `~/.claude/projects/<cwd>/<session>.jsonl` and `<session>/subagents/*.jsonl` (Claude Code 2.1.309) | record types, `message.model`, `message.usage`, `message.stop_reason`, `isApiErrorMessage`, `interruptedMessageId`, `isSidechain`, timestamps, `cost-state` and `pr-link` records | 632 sessions, 629 with at least one model request |
| Codex rollouts, `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (codex-cli 0.164.0) | `session_meta` (version, source), `turn_context.model`, `token_count` (`info`, `rate_limits`), `token_usage_record`, `task_started` / `task_complete` / `turn_aborted`, the error kind on `task_complete` | 204 sessions |
| OpenCode database, `~/.local/share/opencode/opencode.db` (OpenCode 1.18.35), read from a copy opened read-only | `session` aggregates (cost, tokens, `summary_additions`, `parent_id`), per-message `modelID`, `providerID`, `tokens`, `cost`, `finish`, error name and HTTP status | 335 sessions, 299 with at least one request |
| Pi sessions, `~/.pi/agent/sessions/<cwd>/*.jsonl` | `message.provider`, `message.model`, `message.usage` (tokens and Pi's `cost`), `stopReason`, tool-result success | 101 sessions |
| Devin, `~/.local/share/devin/cli/` | its bundled documentation (`docs/reference/otel.mdx`, Devin CLI 3000.11.3); the machine holds Devin's own logs but no session transcripts | none |
| GitHub | every pull request of the repository (956), including the 453 on `agent/` branches, and the closing comments of the closed ones and of issues closed without a pull request | — |

The agents' local stores on this machine all start on 2026-09-24, so the sample covers a month (2026-09-24 to 2026-10-23): 1,272 sessions in 244 worktrees of this repository, of which 38 worktrees had runs from more than one agent. Why the stores start there (an agent's own cleanup, or the machine's history) was not checked.

**Privacy.** The extraction scripts whose output this page reports read only the fields named above; they did not read message content, tool inputs or outputs, prompts, file contents or OpenCode's error bodies, and they kept only aggregates. Session ids appear on this page as the first eight hex digits of a SHA-256 of the id or file path. Worktree names appear because they name this repository's issues and pull requests. The scripts were throwaway and are not committed.

**What the deck itself observed for these runs is not in the sample.** #1741's export merged on 2026-10-22, so the deck's own spans (`agent.run`, `agent.status`, `agent.operation`, `agent.ci`) did not exist for most of the sampled runs, and the backend that receives the maintainer's spans was not queried. Where a table below says "deck-observed", it says what the deck records by construction for a run it starts, read from `src/telemetry/` and [`docs/telemetry.md`](../telemetry.md), not a measured value. #1741's dogfooding milestone is where measured deck-side values come from.

## 1. Signal inventory

### Channels

| Channel | What it is | Content exposure |
| --- | --- | --- |
| **Deck state** | what the daemon knows without asking the agent: pane, run start and end and how it ended, orchestration role, status changes, typed input from a client (interventions), shell-command spans, the branch's pull request and its CI rollup | none beyond #1741's program-and-first-word rule for shell commands |
| **Hooks** | the events each integration strategy already delivers to the daemon (`src/hook.rs`, the OpenCode plugin, the Pi extension, Codex's native hooks behind `dot-agent-deck wrap`) | the hook payloads carry tool details and, for Claude Code and Codex, `transcript_path`; the deck's hook handling reads no token counts today |
| **Agent's own OpenTelemetry** | what an agent exports itself, which the deck switches on and tags with `deck.run.id` (#1741) | none with the agents' content switches off, which is how the deck starts them |
| **Local transcript or session store** | the files and database each agent writes for resume and history | the same file holds prompts, code and tool output next to the usage fields; reading usage means opening a file that holds content |
| **Agent CLI** | commands that print usage (for example a `/cost`- or `/status`-style command inside the agent) | interactive; not used here |
| **Forge (GitHub)** | pull request state, checks, closing comments | public repository metadata |

### Lifecycle, process, Git, verification and intervention

These are the same for every agent, because the deck, not the agent, observes them; the per-agent differences are in usage, below.

| Signal | Channel | Provenance | Available when | Notes |
| --- | --- | --- | --- | --- |
| Run start, end, end reason (`exited`, `closed`, `daemon_shutdown`, `unknown`) | deck state (`agent.run`) | measured | at run end | an arbitrary command gets the same, as agent `none` |
| Status time (`thinking`, `working`, `waiting`, `blocked`, …) | deck state (`agent.status`) | measured from agent-reported events | live | an arbitrary command sends no agent events, so its status has no agent-reported source |
| Interventions | deck state (`deck.intervention`) | measured | live | counts only input a person typed or spoke; deck-delivered tasks and messages are excluded |
| Shell commands and their duration | deck state (`agent.operation`) | measured from agent-reported tool events | live | no exit status; for Claude Code a failed command's span can last until the run ends |
| Pull request and CI rollup | deck state (`agent.ci`), forge | measured (polled about once a minute) | after the branch has a pull request | per branch, so per worktree, not per agent |
| Merge, close, closing reason | forge | measured; the reason is maintainer-written prose | after the fact | not observed by the deck today |
| Changed lines | Git in the worktree; Claude Code `cost-state.totalLinesAdded`; OpenCode `session.summary_additions`; Devin `devin.lines_of_code.count` | measured (Git) or agent-reported | at any time (Git) | several agents often share a worktree, so Git attributes to the work item, not the agent ([resource attribution](#resource-attribution-from-1692)) |
| CPU, memory, disk | a per-machine collector (proposed) | measured | sampled | see [resource attribution](#resource-attribution-from-1692) |

### Usage per agent

| | Claude Code (native hooks) | Codex (wrapper) | OpenCode (plugin) | Pi (extension) | Devin (native hooks) | Arbitrary command |
| --- | --- | --- | --- | --- | --- | --- |
| **Model** | per request in the transcript; `model` on its OTel events and metrics | per turn in the rollout (`turn_context.model`) | per message in its database (`providerID`, `modelID`) | per reply in its session file and in the deck extension's `pi.model_usage` | `model` on `api_request` events and `devin.token.usage` (docs) | unavailable |
| **Tokens** | per request in the transcript (`message.usage`: input, output, cache read, cache write split into 5-minute and 1-hour); per session in `cost-state.modelUsage`; OTel `claude_code.token.usage` and `claude_code.api_request` | per turn and running total in `token_count` / `token_usage_record`; OTel trace attributes `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens` on Codex's response span (read in Codex's source on `main`) | per message and per session in its database; **not** in its own OTel export (the user page says so) | per reply in its session file and in `pi.model_usage` | `devin.token.usage` and `api_request` (docs; unverified against a signed-in Devin) | unavailable |
| **Monetary cost** | `cost-state.totalCostUSD` and OTel `cost_usd`, which Anthropic's monitoring docs call approximations ("Cost metrics are approximations. For official billing data, refer to your API provider") | none in the rollout; Codex's source on `main` has a `codex.turn.cost_microusd` metric and a `codex.turn_cost` event with `usage.estimated_usd`, whether 0.164.0 emits them is **unverified** | `cost` per message, **0** for the OpenAI subscription provider used here | Pi's own estimate from its model catalogue (`usage.cost.total`), also for subscription providers | none: the docs say its counters "describe observed agent activity, not a billing statement" | unavailable |
| **Subscription quota** | a `quotaLimits` record only when a limit is hit | `rate_limits` on every `token_count`: plan type, used percent of a window (a 10,080-minute window here), reset time | none found | none found | none documented | unavailable |
| **Errors and interruptions** | `isApiErrorMessage`; `interruptedMessageId` on the user record after an interrupt; the `StopFailure` hook for usage limits (issue #714) | `task_complete.error` (kind only), `turn_aborted` | message `error` (`APIError` with HTTP status, `MessageAbortedError`) | `stopReason: error` | `api_error` event (docs) | exit status (not recorded today) |
| **Child agents** | subagent transcripts beside the parent; the parent's `cost-state` already includes them (below) | none observed | `session.parent_id` (none set in this sample) | none observed | `docs/subagents.mdx` exists; not inspected | a child process is not an agent to the deck |
| **Agent version** | `version` on every transcript record | `session_meta.cli_version` | `session.version` | not in the session file (its `version` is the file format's) | `service.version` on OTel resources (docs) | unavailable |

**Which channel gives tokens without content.** The agents' own OpenTelemetry does, for Claude Code, Codex, Pi and (by its docs) Devin, with their content switches off. OpenCode's default export carries no token counts (its experimental model-call spans can carry prompt text, so the deck leaves them off); its tokens are in its local database, next to the prompts. For runs from before telemetry was on, a local file or database is the channel this page found for every agent, and reading it means parsing a store that also holds prompts and code, even when only the usage fields are kept: it is not interception of the model traffic, but the reader is trusted with content. No signal here required a proxy or gateway in front of the provider.

### Subscription agents

Every agent in the sample ran on a subscription or a plan, as far as the files say: Codex reports plan type `prolite` on all 10,032 `token_count` records; OpenCode's OpenAI provider reports a cost of 0 on all 2,232 of its replies; 36 of the 101 Pi sessions used the `openai-codex` provider, a ChatGPT subscription; and the Claude Code `quotaLimits` record the deck already parses (`src/quota_signals.rs`, whose test fixture is a record observed on this machine) names a five-hour rate-limit window and an organisation spend cap for overage. So Claude Code ran on a plan with included usage, where usage past the limit can be billed as overage; which of its sampled usage, if any, was billed was not checked. No sampled run has a monetary figure known to be the provider's charge for that run.

The record should therefore keep three things apart, and never fill one from another:

- **Tokens**, which every agent but Devin (no local data) reported for every sampled run with at least one successful request.
- **Quota consumed**, the change in a plan's used percentage over the run, where the agent reports it (Codex). For a subscription this is the figure that changes what a user does: it says how much of the week's allowance a run took. In the sample, the 12 Codex runs of one orchestration (`dispatch-prd-421`) took the weekly window from 26% to 45%. Concurrent runs share one window, so a run's delta is an upper bound when others overlap it.
- **Cost**, with provenance `estimated` (list-price equivalent: what the tokens would cost on a pay-as-you-go key) or `provider-reported` (what was actually charged, which only the provider's billing can give), and `unavailable` otherwise. An agent's `0` on a subscription provider is **unavailable**, not zero: OpenCode's zeros would otherwise make its runs look free next to Claude Code's estimates.

## 2. A minimal provider-neutral execution record

### Two units, not one

The sample shows that a session is not the unit a user decides about. The 37 sessions in `dispatch-prd-421` (14 Claude Code, 12 Codex, 5 OpenCode, 6 Pi) produced one pull request, #1813; `dispatch-prd-1542` produced 59 sessions and one closed pull request. An orchestration restarts a role's agent for each delegated task, so one role is many runs. The record has two levels:

- a **run**: one agent process the deck started in a pane, which #1741's `deck.run.id` already identifies;
- a **work item**: the issue or pull request the runs serve, which today is known only by convention (the worktree and branch name) and needs #628's durable work identity to be reliable. 53 of the 244 sampled worktrees had no pull request of their own: most worked on someone else's (verify, rebase, conflict, review findings), so attributing them by their own branch puts their work on nothing.

### Fields

Every field carries a provenance, one of `measured` (the deck or a collector observed it), `provider-reported` (the provider's or agent's own count), `estimated` (computed from other fields, for example tokens times a price list), `agent-asserted` (the agent's claim, for example "work done"), or `unavailable` (the channel does not exist for this agent; the value is absent and the field says why).

| Field | Level | Provenance today | Source |
| --- | --- | --- | --- |
| run id, pane, agent, role, orchestration id, host | run | measured | #1741's `agent.run` |
| start, end, end reason | run | measured | #1741 |
| time per status, interventions, shell-command time | run | measured | #1741 |
| run termination (see [taxonomy](#revised-taxonomy)) | run | measured (end reason) plus provider-reported (the agent's last error or interrupt) | deck plus the agent's own telemetry |
| agent version | run | provider-reported | the agent's telemetry or local store |
| model(s) | run | provider-reported | the agent's telemetry or local store |
| tokens: input, output, cache read, cache write | run, per model | provider-reported | the agent's telemetry; for OpenCode only its local database |
| quota consumed | run | provider-reported, an upper bound when runs overlap | Codex `rate_limits`; unavailable elsewhere |
| cost | run | estimated (list-price equivalent) or provider-reported (API-key billing), else unavailable | the agent's estimate, or tokens times a dated price list |
| parent run | run | measured | the deck's own run tree (an agent started by another agent in a pane carries that pane's `deck.run.id`, #1741) |
| work item | run | measured where the branch names it, else unavailable | #628 |
| disposition, verification | work item | measured | forge |
| changed lines | work item | measured | Git |
| CPU, memory | run | measured | per-machine collector |
| disk | worktree | measured | per-machine collector |

### Aggregation without double counting

- **A run's tokens and cost come from one source per agent, never a sum of two.** Claude Code's session total already includes its subagents: in all 14 sampled sessions with subagent transcripts, `cost-state` output tokens were at least the parent's transcript plus its subagents' (in one, a Haiku subagent's tokens are counted in the parent's Haiku total although the parent made no Haiku request). Adding the subagent transcripts to the parent's total would count them twice.
- **The agent's own total is the source, not a transcript sum.** Claude Code's transcript misses requests its `cost-state` counts: Haiku models appear in `cost-state` and not in the transcript in 539 of the 618 sessions with a `cost-state`, the transcript's output total is lower in 592 of them, and output tokens agree within 1% in 78% of sessions and within 5% in 93%. Tools that sum transcripts undercount by that margin.
- **An agent started inside another agent's pane** (a `codex exec` from Claude Code, which the shim tags with the pane's `deck.run.id`) is a child run. Its tokens belong to it and to the parent's subtree total, never to the parent's own total; the record stores own totals and derives subtree totals.
- **Retries** are separate runs of the same work item. A work item's total is the sum of its runs' own totals, so a retried item costs what both attempts cost, which is what a user wants to see. The same holds for the orchestrations above and for a dispatch to another project: a run is counted once, under its own run id, and rolled up by work item and by orchestration id.
- **Quota percentages do not sum across agents or plans**, and overlapping runs of one plan share a window. Roll quota up per plan and window, not per work item.
- **Runs the deck did not start** are not in the record. On this machine the agents' stores also hold runs started by tests and tools inside a pane, such as what are probably lane-2 test probes (one request on a cheap model, in a worktree where an agent was running tests; their origin was not verified). A run whose environment was cleared loses its `deck.run.id` and should count as unattributed, as #1692 proposed for processes.

### Privacy and redaction

- The record holds counts, identifiers the deck mints, model and agent names, and timestamps. It holds no prompt, response, tool input or output, file content, command line beyond #1741's rule, path or remote URL.
- Reading usage from a local store is the fallback for an agent with no telemetry channel (OpenCode tokens) and for history. The deck already reads two such stores narrowly for usage-limit signals: a bounded tail of the Claude Code transcript its own hook names (`read_claude_transcript_tail` in `src/quota_signals.rs`) and the Codex rollout (`src/codex_rollout_tail.rs`), comparing JSON keys only. A usage reader that does the same must select fields by name and drop the rest of each record before it leaves the function that parsed it, as the scripts behind this page did. A store's error bodies can hold more than an error: OpenCode's stored HTTP errors carry the provider's response headers, cookies included, so an error is recorded as its kind and status code only.
- A work item's title and a closing reason are maintainer-written prose. The record links to them; it does not copy them.

### Retention and storage

The agents' own stores are not a retention policy: they are each agent's, they can be cleaned up by the agent, and their formats are not documented as an interface, so an agent upgrade can change them without notice. The deck's record is small (one row per run, about twenty numeric or short fields) and should live in the daemon's state directory with a bounded age, defaulting to private and local, with OpenTelemetry export as the optional copy #1741 already makes. The bound itself is a decision for the storage PRD; nothing here measured how long users want to look back.

### Missing data

A value that is not known is absent with a reason (`unavailable: no channel`, `unavailable: telemetry off`, `unavailable: run ended before the first request`), never zero and never guessed, matching #1741's rule that "a value the deck cannot determine is left out". A 0 is reserved for a measured zero: a run that made no request has zero tokens; a subscription reply has unavailable cost.

### OpenTelemetry semantic conventions

#1741 uses `service.name` and `service.version` (stable), `vcs.repository.name` (release candidate) and `host.name` (development), and `deck.*` for everything else. Status as of 2026-10-23, read from the model files of `open-telemetry/semantic-conventions` (v1.44.0) and of `open-telemetry/semantic-conventions-genai` (`main`, last commit 2026-10-21), where the GenAI conventions now live:

| Convention | Stability | Recommendation |
| --- | --- | --- |
| `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens`, `gen_ai.usage.reasoning.output_tokens` | development | adopt for the token fields: Codex already emits these names, and no GenAI convention is stable. Pin the semantic-conventions version the record uses, since development attributes can change |
| `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.provider.name` | development | adopt for model and provider, for the same reason |
| `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id`, `gen_ai.operation.name` (`invoke_agent`, `invoke_workflow`) | development | do not adopt yet: `deck.run.id`, `deck.agent` and `deck.orchestration.id` say what the deck means, and these overlap without a settled meaning |
| all 160 entries in the GenAI model files | development | none is stable; nothing under `gen_ai.*` is a commitment yet |
| `error.type` | stable | adopt for the kind of a run's failure |
| `vcs.ref.head.name`, `vcs.change.id`, `cicd.pipeline.run.id`, `process.pid`, `process.parent_pid` | release candidate | `vcs.change.id` fits the pull request number of a work item; the others only if a later record needs them |
| `session.id` | development | do not use for the deck's run: agents already use it for their own session (Claude Code's and Devin's metrics carry it) |

## 3. Outcome taxonomy against real runs

### Sampled runs

Twenty-one runs were classified by hand, across four integration strategies (native hooks: Claude Code; wrapper: Codex; plugin: OpenCode; extension: Pi), covering success, failure, retry, interruption, orchestration, child agents and providers whose monetary cost is unavailable. Tokens are as the agent reported them (input includes cache reads and writes); "est." costs are list-price equivalents.

| # | Agent | Run | Work item | Elapsed | Requests | Tokens in / out | Cost | Run termination | Work disposition | Candidate taxonomy |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | Claude Code | `2fd90a52` | issue-1383 | 85 min | 91 | 17.7 M / 80 k | est. $7.12 (Claude Code) | completed, after one interruption | merged #1414 | "merged" hides the interruption |
| 2 | Claude Code | `6ec0fcb2` | issue-1383 | 23 h | 287 | 85.0 M / 208 k | est. $41.38 | completed, after one API error retried in-session | merged #1414 | "merged"; the retry is invisible |
| 3 | Claude Code | `a75c61fb` | issue-1400 | 54 min | 113 | 19.5 M / 54 k, one subagent | est. $8.99, subagent included | completed | merged #1431 | fits |
| 4 | Claude Code | `bb24a7fd` | issue-240 | 23 min | 32 | 3.2 M / 10 k | est. $1.65 | completed | no PR; issue closed "no longer reproduces" | **no class fits**: a successful result with no change |
| 5 | Claude Code | `081ee6ec` | issue-754 | 18 h | 15 | 1.5 M / 9 k | est. $1.72 | completed | issue closed as not planned | "abandoned" fits loosely; it was a decision to defer |
| 6 | Claude Code | `09c871f3` | prd-1692 | 9 h | 69 | 10.2 M / 70 k | est. $6.12 | completed | PR #1807 closed after a change of direction; its measurements kept in a decision record | "rejected" and "superseded" both half-fit; the knowledge was used |
| 7 | Claude Code | `38d53763` | verify of PR #1259 | 5 min | 14 | 1.4 M / 8 k | est. $1.07 | completed | contributed a review to another branch's PR (merged) | **no class fits** the run's own work |
| 8 | Claude Code | `744e112b` | prd-1542, 4-agent orchestration, 59 sessions | 15.5 h | 145 | 33.5 M / 105 k | est. $13.77 | completed | PR #1561 closed, replaced by PRD #1631, branch kept as its reference | superseded |
| 9 | Codex | `82775b21` | prd-1542 | 1 min | 2 turns | 0.04 M / <1 k | unavailable; quota 9% → 9% | interrupted (turn aborted) | superseded (as 8) | the run and the item disagree |
| 10 | Codex | `4a404637` | issue-1678 | 19 min | 1 turn | 5.4 M / 24 k | unavailable; quota 4% → 5% | failed (`server_overloaded`) | merged #1688 | the run failed, the item merged |
| 11 | Codex | `5e40f0fc` | prd-421, 4-agent orchestration | 28 min | 1 turn | 10.0 M / 28 k | unavailable; quota 34% → 36%; est. $1.55 | completed | merged #1813 | fits |
| 12 | OpenCode | `ff01c9b7` | issue-1243 worktree | 6 s | 1 | 0 / 0 | reported $0 (subscription) | failed (HTTP 400 on the first request) | merged #1331, but the run is probably a test probe, not part of the work | **misattributed** by worktree |
| 13 | OpenCode | `858926bc` | main checkout | 44 min | 40 | 4.1 M / 8 k | reported $0; est. $1.32 | interrupted (two aborted replies) | none | no work item to attach to |
| 14 | OpenCode | `eb377882` | prd-421 | 4 min | 11 | 0.7 M / 3 k | reported $0; est. $0.24 | completed | merged #1813 | fits, but "$0" is wrong |
| 15 | Pi | `b4c912c4` | prd-421 | 2 min | 16 | 1.3 M / 5 k | Pi est. $0.42 on a subscription provider | completed | merged #1813 | fits; the estimate is not spend |
| 16 | Pi | `b0aa9410` | issue-1635 | 21 min | 33 | 3.2 M / 20 k | Pi est. $2.86 | completed, one tool error | merged #1658 | fits |
| 17 | Pi | `89914d61` | main checkout | — | 3 | 0 / 0 | none | failed (all three replies `error`) | none | fits "failed" |
| 18 | Claude Code | `7b75c2e1` | issue-1676, first dispatch | 31 min | 53 | 6.6 M / 32 k | est. $3.08 | completed | no PR from this worktree | the attempt's outcome is only known from 19 |
| 19 | Claude Code | `4af4fc50` | issue-1676, second dispatch (`-desktop`) | 85 min | 152 | 33.3 M / 86 k | est. $10.75 | completed | merged #1682; issue closed | a retry, visible only through the issue number |
| 20 | Claude Code | `e374fc12` | rebase of PR #1438 | 79 min | 78 | 10.0 M / 30 k | est. $3.67 | completed | contributed to #1438 (merged from another branch) | **no class fits** the run's own work |
| 21 | Claude Code | `f858ff1f` | prd-421 | 5 min | 29 | 3.4 M / 26 k | **missing**: no `cost-state` record | completed | merged #1813 | fits; cost missing without a reason |

Rows 12 to 14 and 17 have no Devin counterpart: there is no Devin session data on this machine, so Devin is not covered by the sample.

### The whole sample

Run termination, classified mechanically from each agent's store (a session's last reply, error and interrupt records); "ended mid-turn" means the store ends inside a turn, which is how a run the deck stopped or whose pane was closed looks from the agent's side:

| Agent | Sessions | Completed | Failed | Interrupted | Ended mid-turn | No request |
| --- | --- | --- | --- | --- | --- | --- |
| Claude Code | 629 | 604 | 4 | 3 | 18 | — |
| Codex | 204 | 192 | 2 | 1 | 9 | — |
| OpenCode | 335 | 255 | 40 | 4 | 1 | 35 |
| Pi | 101 | 100 | 1 | 0 | 0 | — |

Sessions with at least one interruption are more common than sessions that ended on one: 29 Claude Code sessions had an interrupt and carried on. 31 Claude Code sessions had an API error and only 4 ended on it.

Work disposition, per worktree (244), joined to GitHub by branch name: 189 merged, 2 with a closed pull request, 53 with no pull request of their own. Across the whole repository, 449 of the 453 pull requests from `agent/` branches merged and 4 closed, and all four closings say the work was carried on elsewhere: replaced by a PRD (#1561), folded into a combined pull request (#884), a duplicate of a merged one (#495), and a change of direction with the findings kept (#1807). The 53 without a pull request split into work on another pull request (verify, rebase, conflict, findings, review, fix: about two-thirds), issues closed with no change because the defect no longer reproduced (#240, #415), an issue closed as not planned (#754), work still open (#1708, #1820, #906, #221, this PRD), cleanup and Renovate units, scratch directories and the main checkout.

### Field presence and disagreement

Presence is over the sessions where the field applies (a session with at least one request, for usage fields). "Clears" means present or honestly unavailable, with explicit provenance, in at least 90% of sampled runs, read without inspecting prompt or source content.

| Field | Claude Code | Codex | OpenCode | Pi | Disagreement | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| model | 629/629 | 203/204 | 299/299 | 101/101 | — | clears |
| tokens | 629/629 | 201/204, the rest made no request | 259/299; the other 40 failed before any usage, an honest zero | 101/101 | Codex: per-turn sums equal the session total in 199/203, within 1% in 201/203. OpenCode: session aggregates equal message sums in 293/293. Claude Code: transcript sums vs Claude Code's own total, output within 1% in 78% and within 5% in 93%, input within 5% in 70%, the transcript lower in 592/618 | clears, from the agent's own total |
| agent version | 629/629 | 204/204 | 299/299 | 0/101, unavailable (no channel) | — | clears, with Pi marked unavailable |
| cost | 618/629 estimated by Claude Code; 11 missing with no reason | 0/204, unavailable (subscription) | 296/299 report 0 on a subscription; 3 priced | 100/101 estimated by Pi | list price times Claude Code's own token totals vs its `costUSD`: median −12% for Opus (as low as −37%), −24% for Sonnet 5.5, 0% for Haiku. 76% of Opus cache-write tokens were 1-hour writes, which cost more than 5-minute ones, and the session total does not split them, so the estimate cannot be reproduced from the total. Pi's own cost matched list price within 1% in 100/100, since it computes from the same catalogue | clears **only** as `estimated` with subscription zeros mapped to unavailable; as provider-reported spend, 0% of sampled runs |
| quota consumed | rare (only on a limit) | 201/204 | none | none | — | clears as `unavailable` for three agents; informative only for Codex |
| interruptions (agent-side) | recorded | recorded | recorded | not distinguishable | Claude Code counts deck-delivered tasks as typed prompts (`promptSource: typed`), so agent-side input counts cannot tell a person from the deck | use the deck's intervention count, which can |
| pull request link (agent-side) | `pr-link` in 240/548 sessions whose worktree has a PR (44%) | none | none | none | — | fails per agent; take it from the deck (`deck.pr.number`) and the forge |
| child agents | 14 sessions with subagents, all included in the parent's total | none observed | none observed | none observed | — | clears for Claude Code; too little data elsewhere |
| work item | by naming convention only: 191/244 worktrees map to a PR by branch | | | | — | fails until #628 |

### Revised taxonomy

The candidate list mixes three different questions, and the sample breaks it where they disagree: a failed run in a merged item (row 10), an interrupted run in a superseded item (row 9), a merged item whose run was interrupted (row 1), and runs whose work is someone else's pull request (rows 7 and 20). Splitting it into three fields gave each of the 21 runs, and each mechanically classified session, a run termination, and each of the 19 hand-classified runs that served a work item a disposition (rows 13 and 17 served none):

- **Run termination** (per run; deck-observed end reason plus the agent's last error or interrupt): `completed`, `failed`, `interrupted` (by a person, or by the deck when it stops or restarts a run: the deck's end reason says which), `timed_out`, `unknown`. No sampled run timed out; the deck sets no run time limit today, so `timed_out` is reserved for when one exists. "Ended mid-turn" in the table above becomes `interrupted` or `unknown` once the deck's end reason is joined to it.
- **Work disposition** (per work item; forge): `merged`, `superseded` (closed with a successor named; all four closed agent pull requests here), `rejected` (closed without a successor; observed 0 times in 453), `resolved_without_change` (new; #240 and #415), `deferred` (closed as not planned; replaces "abandoned" where a decision was made), `abandoned` (no activity and no decision; not observable without a time rule), `open`.
- **Verification** (per work item, a property rather than an outcome): `verified` when required checks passed on the merged head; `failing`, `unverified`. On this repository branch protection makes every merge verified, so as an outcome it carried no information here; as a property it still separates a merged item from an unmerged one whose checks passed. #1741's `agent.ci` already measures it.

Two more things the sample needs that are not outcomes: a run's **contribution target** (the work item it served, which is not its own branch for verify, rebase and review runs) and its **retry link** (a later attempt at the same work item, rows 18 and 19), both of which depend on #628.

## Resource attribution (from #1692)

#1692's measurements are in [its decision record](https://github.com/vfarcic/dot-agent-deck/issues/1692#issuecomment-6332111458) and summarised on [#633](https://github.com/vfarcic/dot-agent-deck/issues/633#issuecomment-6340717563); nothing was re-measured here. They fit the record as follows:

- **CPU and memory per run**, provenance `measured`, from a per-machine collector outside the deck that groups processes by the run identity they inherit in their environment (`DOT_AGENT_DECK_PANE_ID`, or `deck.run.id` in `OTEL_RESOURCE_ATTRIBUTES`), sampled every 15–30 seconds and sent over OTLP to the same backend so it joins on `deck.run.id`. Memory as PSS, since summed RSS read about twice the real peak. CPU of short-lived processes stays a known undercount unless the collector reads cumulative counters that include reaped children, a cgroup per run, or exits via eBPF.
- **Disk per worktree**, not per run: 12 of 17 panes on the busiest measured deck shared a worktree. It joins the record at the work-item level.
- **Unattributed** resource use (a process whose environment was cleared) is reported as such, the same rule as for runs above.
- macOS was not measured.

**Can a stock scraper group by environment variable? No, for both checked, as of 2026-10-23.** The OpenTelemetry Collector's `hostmetrics` process scraper (beta for metrics) matches processes by name only (`include` / `exclude` take `names` with a match type, in its `config.go`), and its resource attributes are the pid, parent pid, executable name and path, command, command line, owner and cgroup, with no environment. Prometheus `process-exporter` matches on `comm`, `exe` and `cmdline` and names groups from `{{.Comm}}`, `{{.ExeBase}}`, `{{.ExeFull}}`, `{{.Username}}`, `{{.Matches}}`, `{{.PID}}`, `{{.StartTime}}` and `{{.Cgroups}}`, again no environment; its `-children` option adds a process to its nearest matching ancestor's group, which is the parent-walk #1692 showed loses double-forked processes. Both can read the cgroup, so if the deck ever starts each run in its own cgroup (#1692's "what would change it"), either could group by it with no custom code. Until then, grouping by environment needs a small custom receiver or exporter.

## What this means for the prototype and the decision

### Prototype recommendation (milestone 4)

- **Metric set:** per run, the agent's own token totals by model (input, output, cache read, cache write), cost with provenance (`estimated`, or `unavailable` for subscription zeros and agents with none), quota consumed where reported, and the run termination; per work item, the disposition and verification fields above. Elapsed time, status time and interventions are already in #1741's spans.
- **Agents:** Claude Code (its OTel `claude_code.api_request` events already reach the backend tagged with `deck.run.id`), Codex (its traces carry `gen_ai.usage.*` according to its source on `main`, to be confirmed against the installed version; its quota is only in the rollout file today) and Pi (the deck's own extension already sends `pi.model_usage`). Together they are three integration strategies. OpenCode has no token channel but its database, so it is the one agent that would need a local reader; leave it out of the first prototype and record it as unavailable. Devin needs a signed-in session to verify its docs before it is in.
- **Where:** a backend-side derivation first, with no deck code: the three agents' usage already arrives in the same backend as `agent.run`, joined by `deck.run.id`, so a query can produce the record and show what is missing before anything is built. If the deck needs the values itself (for the dashboard or budgets), the place is a new module under `src/telemetry/` that consumes usage events the daemon already receives; token counts in hook payloads, or a reader of `transcript_path`, would change hook handling, which is a separate PRD and is out of bounds for this run (the hook files are owned by another orchestration until PRD #1666 part 3 merges). Overhead to measure: the extraction behind this page parsed 1.2 GB of Claude Code transcripts in 35 s and 403 MB of Codex rollouts in 10 s at a load average around 46, so a reader that re-parses whole files is not acceptable on a busy deck; an incremental tail (the way `src/codex_rollout_tail.rs` already tails Codex rollouts for usage-limit errors) is the shape to measure.

### Preliminary read: Revise, leaning Go

- **Tokens are trustworthy** for the three agents that report them through telemetry, and for OpenCode from its database: present in every sampled run that made a request, and internally consistent where the agent keeps two counts (98% exact for Codex, 100% for OpenCode). The caveat is the source: transcript sums undercount Claude Code.
- **Monetary cost is not**: no sampled run had a provider-reported charge, the estimates disagree with each other by 12–37% for the most-used model, and one agent reports zeros for a paid plan. It is usable only as a labelled list-price equivalent.
- **Metrics that would change a decision in this sample** (an inference from the data, since nobody was shown these values while the runs happened):
  1. **Run termination by model and agent**: 49 OpenCode sessions on this machine (40 of them in this repository's worktrees) failed on their first request with HTTP 400, on 7 different days in 17 directories, on models the subscription evidently does not serve (`gpt-5.4-mini` in 38 of the 49). Seeing that as a failure rate would make someone fix a configuration or a test's model choice.
  2. **Quota consumed per work item**: one orchestration's Codex runs took 19 points of a weekly allowance. That decides which agent takes the next task and whether to stop.
  3. **Interventions and interruptions per run**: 29 interrupted Claude Code sessions, and the deck's own intervention count, separate a run that needed help from one that did not, which is the "which roles need you most" question #1741's spans already answer.
  4. **Work-item roll-up across agents**: `dispatch-prd-421` cost Claude Code's estimate of $30 plus 19 points of Codex quota plus Pi's estimate of $2 for one pull request, which is the figure for comparing workflows.
- **Supersession, not rejection, is how agent work fails here**: 0 of 453 agent pull requests were closed without a successor. A record that counts only merged versus rejected would report a 99% success rate and miss the retries and superseded attempts that cost the most (rows 8, 18 and 19).

That is enough to argue against Stop, and against the narrowest Revise (outcome and interventions only), because tokens and quota are reliable and the record works across four materially different integrations without a proxy. It is not yet enough for Go: the PRD's bar asks for metrics that changed a real decision, and this evidence only shows that they would have. The prototype above, run against #1741's dogfooding, is what can show it.

## Not covered and open

- **Devin**: no local session data; its usage fields are from its documentation only.
- **Timed out**: no sampled run timed out, and the deck has no run time limit to time out against.
- **Deck-observed values**: not measured for these runs (see the top of the page); #1741's dogfooding supplies them.
- **Codex's cost estimate**: present in Codex's source on `main`; whether codex-cli 0.164.0 emits `codex.turn.cost_microusd` was not checked.
- **The 18 Claude Code and 9 Codex sessions that ended mid-turn**: assumed to be runs the deck stopped or whose pane closed; confirming needs the deck's end reason.
- **The Claude Code requests a transcript misses**: Haiku requests explain part of the gap to Claude Code's own total; the rest (for example 65 k of 94 k output tokens in one session with a subagent) was not explained.
- **Retention**: why every agent's store on this machine starts on 2026-09-24 was not checked, and how far back users want to look was not asked.
