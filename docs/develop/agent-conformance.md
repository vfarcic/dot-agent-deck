# Agent conformance and compatibility evidence

> **Developer / maintainer reference.** This page is not published. It records the discovery done for [PRD #221](https://github.com/vfarcic/dot-agent-deck/issues/221): what each supported agent integration claims, which test can disprove each claim, how a reduced real-agent interop matrix behaved when it was run, and the decision that came out of it. Every count and run on this page carries the date it was taken; a count of this shape rots, so re-measure before citing one.

[Agent adapters](agent-adapters.md) is the contract for *adding* an agent. This page is about *knowing what the shipped ones support*, and how well that knowledge is backed.

## Capability dimensions

A dimension is something a user can rely on or not, per agent. "Deck-level" means the deck does the same thing whatever agent is in the pane, so the agent axis adds no information there; "agent-specific" means what happens depends on what the agent reports or how it draws its screen.

| # | Dimension | What a user relies on | Agent-specific? | Where the deck declares it today |
| --- | --- | --- | --- | --- |
| D1 | Identity and launch | the pane is recognised as that agent, including behind a launcher script | yes (detection), deck-level (launcher declaration) | registry entry: detection basenames, strategy |
| D2 | Lifecycle status | Thinking / Working / Idle follow the agent's turn | yes | the integration strategy (hooks, plugin, extension, wrapper) |
| D3 | Readiness | the deck does not type into an agent that cannot take input yet | yes | registry: screen recognisers for Claude Code and Codex; `SessionStart` for the others |
| D4 | Waiting for the user | **Needs Input** for a permission prompt or a question, and the options known to the deck | yes | registry: per-version question tables (`verified_from` / `verified_on`) |
| D5 | Failure status | **Error** for a failed provider turn, **Blocked** for a spent limit | yes | hook ingestion only; no declaration |
| D6 | Prompt metadata | the submitted prompt is known to the deck | yes | hook ingestion only |
| D7 | Tool activity | **Working** names the tool and its detail | yes | hook ingestion only |
| D8 | Background work | the `Background:` line counts commands and subagents | commands: deck-level; subagents: yes | hook ingestion only |
| D9 | Message receipt | a deck message wakes the agent, and its receipt is acknowledged by the agent rather than by a PTY write | yes | registry and adapter code; the user table is in `docs/orchestration.md` |
| D10 | Delegation as worker | a delegated task is received, worked and reported with `work-done` | yes, through D3 and D9 | none beyond D3/D9 |
| D11 | Delegation as orchestrator | the agent runs `delegate`, and receives worker reports | mostly deck-level: any agent with a shell tool can run the CLI; receiving reports is D9 | none |
| D12 | Prompt keys | voice/desktop interrupt, clear and delete reach the agent | yes | registry: `prompt_keys`, measured per version |
| D13 | Restoration | after the deck restarts, the workspace and its commands come back | deck-level: the layout and command are restored, not the agent's conversation | none needed |
| D14 | Replacement | a `clear = true` delegate replaces the agent and the new one receives the task | yes, through D3 | none beyond D3 |
| D15 | Remote operation | the same on a remote host | deck-level, plus the per-agent hook install on that host | the startup installer per agent |
| D16 | Platform | macOS and Linux; Windows is not shipped | partly: Devin's config resolver and Codex's home resolution are Unix-shaped | per-adapter code |
| D17 | Agent's own telemetry joined to the deck run | the agent's OTLP data carries the deck's run id | yes | per-agent environment and configuration the deck writes |
| D18 | Execution posture | permission mode, sandbox, network, credentials, tool authority | yes | **not declared**: the deck observes and answers permission prompts (D4) and enforces none of these |

D18 is the PRD's question about permission, filesystem, network, sandbox, credential and tool-authority capabilities. The finding is that the deck has no model of them at all: it reports a permission prompt when the agent raises one and can answer it, and the posture itself is whatever the user launched the agent with (`--allowedTools` for Claude Code, `--sandbox` and `--ask-for-approval` for Codex, `--permission-mode` for Devin, `--approve` for Pi). The one place posture changes deck behaviour is Codex: with untrusted hooks the doorbell asks the model to run `dot-agent-deck inbox fetch` itself, which needs `network_access = true` in its sandbox. Declaring posture would be new product scope, not a conformance question, so it is left out of the levels below.

## Capability levels

A claim's level is set by the strongest evidence that can disprove it, and every claim names that evidence.

| Level | Meaning | What can disprove it | Where it runs |
| --- | --- | --- | --- |
| **live** | a test drives the real agent, with a model turn where the dimension needs one, and asserts the claim | a lane-2 test, or a lane-1 test that runs a real agent binary with no model call | lane 2: a developer's machine, by hand. The no-model lane-1 tests skip in CI, which installs no agent CLI |
| **contract** | a deterministic test feeds the deck payloads shaped like the agent's and asserts the deck's side | a fast-tier or lane-1 test with recorded or synthesized payloads | every PR |
| **documented** | taken from the vendor's documentation, never measured | nothing in the tree | — |
| **degraded** | works in a named reduced form, which the user docs describe | the test of the fallback path | as the test |
| **unavailable** | the agent exposes no channel for it today (CLAUDE.md rule 20), and the user docs name the gap | nothing to run | — |
| **unknown** | neither measured nor documented | — | — |

Two properties matter more than the level, and neither is recorded anywhere today:

- **Recency.** A **live** claim is only as good as the last time someone ran its test. Lane 2 runs on no runner ([e2e lanes](e2e-lanes.md#why-lane-2-is-not-in-ci)), and a run leaves its evidence in that developer's `.dot-agent-deck/recordings/<test>/` and nowhere shared. So no claim on this page can say "verified within the last N days" from data; it can only say that a test exists.
- **The agent version.** The registry's question tables carry the agent versions they were measured on as data (`verified_from`, `verified_on`), and the prompt keys name theirs in comments. Elsewhere a version appears as a feature gate (Claude Code 2.1.78 for **Error**, for example) or in prose, not as a record of when a claim was last seen to hold, and no test result records the agent version it ran against. So an upstream release that breaks a claim surfaces when someone happens to run the test.

## Inventory

Measured 2026-10-23 against `tests/CATALOG.md` and `src/agent_registry.rs` at `4e653fea`. Cells give the level and the catalog id of the test that can disprove the claim; `L2` marks a lane-2 test. "Arbitrary command" is a pane running something the registry does not recognise.

| | Claude Code | Codex | OpenCode | Pi | Devin | Arbitrary command |
| --- | --- | --- | --- | --- | --- | --- |
| Strategy | native hooks | wrapper + native hooks | plugin | extension | native hooks | none |
| D2 lifecycle status | live: `chain-smoke/claude/001` L2 | live: `codex/live/001`, `codex/hooks/001` L2; degraded without trusted hooks (quiet-output fallback) | live: `telemetry/otlp/007` L2 | live: `pi/live/001` L2 | live but quarantined: `devin/live/001` (#1348); contract: the `devin_hook_ingestion` tests | unavailable by default: background commands only (D8). Launched under `dot-agent-deck wrap`, the wrapper's generic rules report output activity (contract: the classifier unit tests in `src/wrap.rs`) |
| D3 readiness | live: screen recogniser, `orchestration/delegate/014` L2 | live: composer recogniser, `inbox/codex/010` L2 | live: `orchestration/delegate/046` L2 | live: `chain-smoke/pi/002` L2 (boot delay held) | documented: its `SessionStart` timing is unmeasured (registry comment) | — |
| D4 waiting, options known | live: `question/live/001`–`003` L2 (2.1.136+) | live: `question/live/004` L2 | live: `question/live/005` L2 | live, no model: `question/live/006` (other extensions' dialogs only; Pi asks none of its own) | documented: permission table built from Devin's docs, untested (`question/detect/009` is contract) | unavailable |
| D5 error / blocked | live: `status/blocked/022` L2 (2.1.78+) | live: `status/blocked/026` L2 | live: `status/blocked/023` L2 | unavailable | unavailable | unavailable |
| D6 / D7 prompt and tool detail | live: `chain-smoke/claude/001` L2 | live: `codex/hooks/001` L2 | contract: `hooks/delivery/003`; the live entry `chain-smoke/opencode/001` has no test (below) | live: `pi/live/001` L2 (#622) | live but quarantined: `devin/live/001` | unavailable |
| D8 background subagents | live: `status/background/008` L2 | contract: `status/subagent/006` | contract: `status/background/010`, `011` | unavailable (extension reports none) | unavailable | — |
| D9 message receipt | live, native wake from 2.1.296: `inbox/claude/011`–`014` L2 | live, doorbell when idle and hooks at turn end: `inbox/codex/009`–`011` L2 | live, native through the TUI plugin: `inbox/opencode/015`–`022` L2 | live, native: `inbox/pi/001`–`022` L2 | degraded by design (doorbell only); contract: `inbox/doorbell/002`; no live test | unavailable |
| D10 worker | live: `orchestration/delegate/014` L2 | live: `codex/worker/001`, `orchestration/delegate/009` L2 | live: `orchestration/delegate/015`, `046` L2 | live: `chain-smoke/pi/002` L2 | **unknown**: no test | — |
| D11 orchestrator | live: `scheduler/dispatch/013`, `orchestration/dispatch/002`, `scheduler/idle-worker/022` L2 | **unknown**: no test | **unknown**: no test | live: `chain-smoke/pi/001`, `pi/live/002` L2 | **unknown**: no test | — |
| D12 prompt keys | live: `prompt/voice-keys/001`–`003` L2 | live: `prompt/voice-keys/004` L2 | live: `prompt/voice-keys/005` L2 | measured by hand on Pi 0.87.1 (registry), no test | unavailable: unmeasured, `prompt_keys` is `None` | unavailable |
| D17 own telemetry | live: `telemetry/otlp/005` L2 | live: `telemetry/otlp/018` L2 | live: `telemetry/otlp/007` L2 | live: `telemetry/otlp/019` L2 | live per its catalog entry, `telemetry/otlp/020` L2; the user docs still say "not yet confirmed against a signed-in Devin" | — |

D13 (restoration), D15 (remote operation) and D16 (platform) are left out of the table because the agent column adds nothing to them: the deck restores the layout and command for every agent and no conversation for any ([Resuming Sessions](../session-management.md#resuming-sessions)); the remote tests that reach a real agent use Claude Code only (`remote/upgrade/008` L2); and the live tests above declare Unix or macOS-and-Linux coverage, none Windows. D14 (replacement) is D3 plus D10, and the D10 cells for Claude Code, Codex, OpenCode and Pi are already `clear = true` replacements.

On 2026-10-23, 41 test files were gated on `e2e-live`, carrying 129 distinct `#[spec]` ids. Most of them drive Claude Code; Devin appears in two (`devin/live/001` and `telemetry/otlp/020`).

### What the inventory shows

- **The claims already live in six user pages, by hand.** The per-agent tables in `docs/session-management.md` (two), `docs/orchestration.md`, `docs/telemetry.md` (two), `docs/desktop/voice.md`, `docs/installation.md` and `docs/troubleshooting.md` are the only published statement of what each agent supports, and nothing ties a cell to a test. One has already drifted from its evidence: the telemetry table says Devin's join is "not yet confirmed against a signed-in Devin", while `telemetry/otlp/020` claims a lane-2 test that does exactly that. Either the test has never been run green against a signed-in Devin or the page was not updated, and nothing in the tree can say which.
- **The matrix's orchestrator axis is mostly empty, and mostly needn't be filled.** Real-agent evidence for an orchestrator exists for Claude Code and Pi only. But what an orchestrator does that is agent-specific is receive messages (D9), which every worker cell already exercises; issuing `delegate` is a shell command any agent with a shell tool runs. So an orchestrator cell per agent buys little over the worker cells, and an N×N grid buys less.
- **The worker cells do not assert the same contract.** `chain-smoke/pi/002` and `codex/worker/001` assert the `work-done` leg; `orchestration/delegate/014`, `015` and `009` say in their catalog entries that they do not. A conformance suite needs every worker cell to assert the same three things: receipt acknowledged by the agent, the work done, and `work-done` reported.
- **The catalog can name a live test that does not exist.** `chain-smoke/opencode/001` describes a real OpenCode run whose card shows the OpenCode tool name, but no test in the tree carries that id, so the one live claim for OpenCode's tool detail is a catalog line. A conformance check has to resolve each named id to a test, not to a catalog entry.
- **Devin is the thinnest column.** Its one model-backed test is quarantined (#1348) because no maintainer has a Devin account to run it, `devin auth login` has no API-key path ([e2e lanes](e2e-lanes.md#what-an-api-key-does-and-does-not-unlock)), and it has no delegation test at all. Apart from its telemetry test, every Devin claim in the table rests on that quarantined test, its documentation or contract tests.
- **Some real-agent coverage needs no credential.** `question/live/006` runs the real Pi binary without a model call, in lane 1, and skips in CI only because CI installs no agent CLI. `pi/live/003` also makes no model call and needs no credential, but sits in a lane-2 file. Whether a pinned agent CLI in CI, with no credential, falls inside CLAUDE.md rule 5's "no e2e test reaches a real agent in CI" is a policy question this discovery does not settle; it is listed as a follow-up below because it is the only route this discovery found to catching an upstream CLI change on a schedule.

## The reduced interop matrix

The original #221 proposal was a parametrised orchestrator-by-worker matrix, five agents by five. From the inventory, the routing between any two panes is the deck's and is agent-agnostic, so the agent-specific part of a pair is the worker's readiness and receipt (D3, D9, D10) and the orchestrator's receipt of the report (D9 again). The reduced default set is therefore:

- **One worker cell per agent**, under a scripted orchestrator that runs the real `delegate` CLI: the cell is receipt acknowledged by the agent, the work, and `work-done`.
- **One real-to-real cross-agent pair**, so that a real orchestrator reading a real worker's report is exercised at least once: today that is Pi orchestrating Claude Code.
- **The full N×N grid stays opt-in**, as a filter someone asks for, never a default.

Retries are isolated per cell: a cell that fails is rerun alone, and a pass on the rerun is recorded as a flake of that cell, never folded into a green total. nextest's `retries` with its FLAKY report already does this per test, and `.config/nextest.toml` uses it that way for `dispatch_015`.

The trial set, all existing lane-2 tests, so the trial needed no harness change:

| Cell | Test | Asserts `work-done` |
| --- | --- | --- |
| Claude Code worker | `orchestration/delegate/014` (`delegate_014_real_claude_worker_acts_on_clear_true_delegate`) | no |
| OpenCode worker | `orchestration/delegate/015` (`delegate_015_real_opencode_worker_acts_on_clear_true_delegate`) | no |
| Codex worker | `codex/worker/001` (`codex_worker_001_real_worker_receives_delegate_and_signals_work_done`) | yes |
| Pi worker | `chain-smoke/pi/002` (`chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done`) | yes |
| Pi orchestrator → Claude Code worker | `chain-smoke/pi/001` (`chain_smoke_pi_001_orchestrator_delegates_to_real_worker`) | yes |
| Devin worker | none exists; and Devin was logged out on the trial host | — |

### The trial, 2026-10-23

Twenty consecutive runs of the five cells together, one nextest invocation per run, on the 16-core development box while other agents' work shared it (1-minute load average between 8 and 72 at the end of a run). Each run set `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1`, so a preflight that could not reach its agent would have failed rather than skipped and been counted as a pass. nextest retries were off, so no failure could have been absorbed. Agents: Claude Code 2.1.309 and OpenCode 1.18.35 on Haiku, Codex 0.163.0 on `gpt-5.6-luna` through its ChatGPT login, and Pi 1.3.0 on `openai-codex/gpt-5.6-luna`, all at the harness's default test models. Devin 3000.11.3 was installed but logged out.

| Cell | Runs | Pass | Fail | Skip | Median | 90th percentile | Max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Claude Code worker (`delegate/014`) | 20 | 20 | 0 | 0 | 13.6 s | 19.8 s | 22.2 s |
| Pi worker (`chain-smoke/pi/002`) | 20 | 20 | 0 | 0 | 14.6 s | 19.9 s | 23.4 s |
| Codex worker (`codex/worker/001`) | 20 | 20 | 0 | 0 | 20.4 s | 26.4 s | 30.2 s |
| Pi → Claude Code (`chain-smoke/pi/001`) | 20 | 20 | 0 | 0 | 32.8 s | 39.5 s | 40.2 s |
| OpenCode worker (`delegate/015`) | 20 | 20 | 0 | 0 | 80.5 s | 109.4 s | 154.6 s |

A run's wall clock is set by its slowest cell, OpenCode, which nextest flags as slow every time: 64–155 s per run for the tests themselves, and 70–257 s including the incremental build check. A one-off build of the `e2e,e2e-live` test binaries before the first run took about 12 minutes. Model spend was not metered by the trial: per run it is three Haiku conversations (the Claude Code worker, the OpenCode worker and the Claude Code worker under Pi), two GPT conversations on a ChatGPT subscription (Codex and Pi), and the preflight probes the Codex, OpenCode and Pi gates make.

What the numbers support, and what they do not:

- **No false failure in 100 cell runs.** That is consistent with the criterion of under 5 % per cell, but 20 runs cannot establish it: with zero failures in 20, the 95 % upper bound on a cell's failure rate is about 14 % (`1 − 0.05^(1/20)`). Showing under 5 % with that confidence takes about 60 clean runs per cell.
- **No genuine compatibility failure either**, so the trial did not test whether a failing cell would be diagnosable from its output. The tests' own failure output and the [load context](e2e-lanes.md#reading-a-red-run-the-load-context) are what a developer would read.
- **The cost suits an opt-in local command, not a mandatory tier.** About 90 seconds per run is cheap enough to run before a change to an adapter, delegation or the inbox. Since #502 there is no pre-PR tier to put it in, so "acceptable for the documented pre-PR tier" now means a command a developer chooses to run, which is what the decision below keeps.
- **The cells measure a mixed contract.** Two of the five passes say nothing about `work-done` (above), so 20 passes of the Claude Code and OpenCode cells say nothing about that leg for those agents.
- **Devin is not in it.** Its column stays at **documented** or **unknown** for every delegation dimension until someone with a Devin account runs a Devin cell.

The trial needed no change to the harness or the registry; the command was a loop over `cargo test-e2e-live -E '<the five test names>'`, recording each run's nextest output and junit.

## Keeping declarations, output, docs and versions in step

The question was whether an agent's declared capabilities should be fixed at build time, generated, or discovered by tests, and how the docs, upstream versions and #502's constraints stay in step with them.

**Declarations stay in the registry, at build time.** The registry already declares the agent-specific parts that change behaviour: the strategy, the readiness recognisers, the question tables and the prompt keys. Discovering capabilities at runtime would mean probing an agent the user is about to use, which costs a model turn or a guess, and the registry's design rules out runtime extension anyway ([Agent adapters](agent-adapters.md#design-philosophy-a-curated-registry-not-a-plugin-system)). What is missing is not a place to declare but a link from each declaration to its evidence.

**The link should be a manifest that a check reads, not generated prose.** A small checked-in table, one row per agent and dimension, giving the level and the catalog ids that can disprove the claim. `xtask/linkage-check` already parses `tests/CATALOG.md` and resolves `#[spec]` ids; a rule over that table would fail the build when a named id has no test (the `chain-smoke/opencode/001` case), when the test's catalog entry does not name that agent, or when a **live** claim names only a contract test. Generating the user-doc tables from it was considered and not recommended: those tables carry conditions a level cannot hold ("Yes, 2.1.78 or newer", "Yes, for dialogs other Pi extensions show"), and CLAUDE.md rule 21 wants them in the user's words. A check that each user-table cell agrees with the manifest's level is the lighter tie, and is part of the follow-up's design question rather than decided here.

**Upstream versions are recorded where they are measured.** The question tables and prompt keys already carry the versions they were measured on. The same field belongs on a **live** claim: the agent version the claim was last seen to hold on. Where it comes from is the open part, because of the next point.

**#502 decides what can be published.** Lane 2 runs on a developer's machine and nowhere else, by a decision CLAUDE.md rule 5 records as a trade rather than a gap. So there is no schedule, and the evidence of a run stays on the machine that ran it. A claim can therefore carry "a test exists" and, at best, "a maintainer ran it green on date D against version V" if someone records that by hand; it cannot carry "recent" in any sense a reader could rely on. That is the condition the PRD's success criteria set for stopping public reporting.

## Decision

**Revise**, recorded 2026-10-23 on [#221](https://github.com/vfarcic/dot-agent-deck/issues/221): keep the reduced interop matrix and an internal compatibility record, and publish no compatibility matrix or reliability score.

Against the PRD's criteria:

- **Every claim names a test that can disprove it** — met for the inventory above, with the gaps named as **documented** or **unknown** rather than filled in.
- **The reduced set run at least twenty times** — met for the four agents that could run (100 cell runs, no failure, no skip). Devin could not run.
- **The default matrix is viable below 5 % false failures per cell and at an acceptable cost** — the cost is acceptable for an opt-in local command; the rate is consistent with the bar but not established by 20 runs, so the matrix stays opt-in, as the criterion says it must when the rate is unproven.
- **Public reporting is `Go` only with recent evidence for every published cell** — not met, and not meetable while lane 2 runs on no schedule: "recent" has no source. So public reporting stops, as the criteria set out for that case, and the internal contract stays because the inventory shows it would help: claims already drift across six user pages, the catalog can name a test that does not exist, and the worker cells assert different contracts.

Follow-ups:

- [#1852](https://github.com/vfarcic/dot-agent-deck/issues/1852) — PRD: an agent capability record that the build checks against the catalog and the tests, seeded from the inventory above, with the user pages' tables tied to it.
- [#1853](https://github.com/vfarcic/dot-agent-deck/issues/1853) — PRD: the reduced matrix as one opt-in command with a uniform worker contract, isolated retries and a local record of results and agent versions. It touches the shared e2e harness, so it starts after PRD #1666 part 3 has merged.
- [#1854](https://github.com/vfarcic/dot-agent-deck/issues/1854) — a policy question, not a plan: whether pinned agent CLIs with no credential may run in CI for offline real-binary tests, the only route found to catch an upstream CLI change on a schedule.

Not pursued:

- **Third-party or runtime agent integrations.** The registry's non-goal stands ([Agent adapters](agent-adapters.md#design-philosophy-a-curated-registry-not-a-plugin-system)); nothing in the inventory argues for reopening it before the capability record exists to say what an integration would have to prove.
- **[ACP](https://agentclientprotocol.com/overview/introduction) as an integration path.** ACP connects an editor to an agent it runs as a subprocess over JSON-RPC on stdio, and assumes the user is working in the editor. The deck's model is the opposite: the agent's own interface runs in a pane the user sees, and the deck observes it. Driving an agent over ACP would mean the deck drawing the conversation itself, which is a different product rather than a better adapter. Worth revisiting only for an agent that has no hooks or plugin system but does speak ACP; none of the five shipped agents is in that position.
