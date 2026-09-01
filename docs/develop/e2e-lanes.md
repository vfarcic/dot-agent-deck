# The e2e lanes

Issue #502 split the L2 e2e suite into two CI lanes by whether a test needs a working agent credential. **CLAUDE.md rule 5 is the policy** — which lane runs where, and why there is no longer a pre-PR obligation to run the tier in full. This page is the operational half: how to actually drive them, and what a green run does and does not prove.

## What each lane is

| | lane 1 | lane 2 |
| --- | --- | --- |
| command | `cargo test-e2e` | `cargo test-e2e-live` |
| cargo features | `e2e` | `e2e,e2e-live` |
| files | the 47 `tests/e2e_*.rs` that need no agent credential | **all 71** — lane 1 *plus* the 24 that do |
| where | the `e2e-deterministic` job in `.github/workflows/ci.yml` | the `live` job in `.github/workflows/e2e-live.yml` |
| when | every PR | per-merge on `main`, `workflow_dispatch`, and a `run-live-e2e`-labelled PR |
| secrets | none in the job's environment at all | `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`, environment-scoped |
| timeout | 60 min | 90 min |

`cargo test-e2e-live` is a **superset** run, not a live-only run. The 24 credentialed files open with `#![cfg(all(feature = "e2e", feature = "e2e-live"))]`, so `e2e-live` without `e2e` compiles every e2e file to an empty crate; there is deliberately no live-only alias. That is also why rule 2's clippy gate names both features — with only `--features e2e` those 24 files are type-checked by nothing.

Neither lane is a required status check. The required set is still `build`, `build-macos`, `build-windows` and `security`; rule 8 records why the two new jobs are deliberately advisory.

## Running the live lane on a PR

```sh
gh pr edit <n> --add-label run-live-e2e
```

The label exists already (`run-live-e2e`, "Run the credentialed live e2e lane on this PR"). `labeled` is in the workflow's `pull_request` trigger list, so applying it to an already-open PR starts a run immediately; `synchronize` keeps a labelled PR re-running as it is pushed to.

To run it on `main` without waiting for a merge:

```sh
gh workflow run e2e-live.yml
```

Then read the result — and read the SKIP lines, not just the colour. See below.

```sh
gh run list --workflow=e2e-live.yml --limit 5
gh run view <run-id> --log | grep -E '^\s*SKIP: \[e2e\] '
```

The run's **summary page** carries the *count* without a log download: an *e2e runtime skips* section saying how many e2e tests skipped, how many unrelated `SKIP:` lines the rest of the `--workspace` run produced, and nothing else. **The reasons are deliberately not reproduced there** — a matching line is ordinary output from a successful test, so nothing downstream may treat it as trustworthy metadata; the summary reports a number and points back at the masked job log, which is what the grep above reads. That count exists at all only because the run step passes `--success-output=final`: a runtime skip is a *successful* test, so under nextest's defaults its `SKIP:` line is suppressed from the log entirely and nothing downstream could count it. On lane 2 that count is now 0 on any passing run, because `DOT_AGENT_DECK_REQUIRE_REAL_E2E` panics before the line is printed — see below for what it is still good for. On a **local** run without the flag it is the number that matters.

Two details of that grep are load-bearing. The leading `\s*` is there for the same reason `checks.sh` needs it: nextest indents captured output by four spaces, so a `^SKIP: ` anchored at column 0 matches nothing on a run that really did skip (issues #452, #490). The `[e2e]` marker is there because `SKIP: ` alone does not identify the e2e harness — both aliases carry `--workspace`, so a run also selects `xtask/` and the root package's unit tests, several of which print their own `SKIP:` lines when `python3`, `node`, `jq` or `bash` is missing. `_skip_if_err` in `tests/common/mod.rs` prints the marker; drop it from the pattern and you are counting those too. (`.claude/skills/verify-pr/checks.sh` deliberately keeps the broader marker-less pattern — it runs against branches that predate the marker, and its output is a local file rather than a job summary.)

The job also uploads a JUnit report as `nextest-e2e-live-attempt-<n>` (issue #564), which is what survives a job-level re-run overwriting the conclusion. It carries test names, outcomes, per-test wall clock and the suite counts. Two things it does **not** carry. It has never carried the ran-versus-skipped distinction, for the reason above — read the run summary for that, not the artifact. And since issue #785 it carries no test output at all: `scripts/junit-strip-output.py` rebuilds the report from a per-element attribute whitelist before upload, so `<system-out>`, `<system-err>`, every `<failure>`/`<error>` message and body, and every text node are gone by construction. That is because nextest's JUnit defaults store stdout **and** stderr for failed and retried tests, the file is written by the cargo process holding `ANTHROPIC_API_KEY`, and GitHub's secret masking covers log rendering but not files copied out by `actions/upload-artifact` — which on this public repository any logged-in reader can download. Run `python3 scripts/junit-strip-output.py --self-test` to see that proved.

## The `live-e2e` environment

The job declares `environment: live-e2e`. That is the control that makes running a credentialed job on freshly-merged code acceptable at all (issue #785 decision 2): with a **required reviewer** on the environment, a merge *queues* the run and waits for a human click, so merged code never auto-executes with the key. Environments and their protection rules are free on public repositories.

Two properties of it are deliberate and must not be tidied up:

- **No deployment-branch restriction.** Limiting the environment to `main` would stop a branch PR exercising the credentialed path, which is exactly the validate-before-merge property the label trigger exists for.
- **The required reviewer is set on the environment by hand**, by the repository owner. A workflow file cannot declare it, and it is the one control that addresses the threat directly — an environment with no reviewer is a repository secret wearing an environment's name.

Lane 2 needs **two** environment secrets: `ANTHROPIC_API_KEY` (claude, opencode and pi — 26 of the 31 selected gated tests) and `OPENAI_API_KEY` (codex — the other 5, via the provisioning step). Both already exist as *repository* secrets; #785 decision 2 wants them scoped to this environment instead.

One ordering note worth getting right, because it is cheap to get right and annoying to undo: **add the required reviewer last.** A required reviewer blocks *every* job that declares `environment: live-e2e`, on *every* run, before any step — the job does not start until a human clicks Approve, there is no per-branch pre-approval, and a re-run re-queues. Since the `pull_request` trigger includes `synchronize`, a `run-live-e2e`-labelled PR then costs one click per push. Prove lane 2 works first, then add the reviewer before merging. #785 decision 2 is about *merged* code auto-executing with the key; nothing in it requires the reviewer to exist during pre-merge validation on a branch. If the reviewer is already on, remove the label while iterating and re-add it when a run is actually wanted.

Check the current state with:

```sh
gh api repos/vfarcic/dot-agent-deck/environments --jq '.environments[] | {name, protection_rules, deployment_branch_policy}'
```

## Three things that decide whether a green lane-2 run means anything

### 1. A skip is a pass — which is why lane 2 forces every skip to fail

Real-agent tests open with `skip_unless!(check_<agent>_available())`. That macro prints `SKIP: [e2e] <reason>` and **returns normally**, so nextest counts the test as **passed**. An absent or unusable credential therefore removes coverage silently instead of reddening the job. Left alone, "the job passed" and "no agent ran" would be the same observation, because `cargo test-e2e-live` is a superset run and lane 1's 47 deterministic files carry the result green on their own.

So the `live` job sets **`DOT_AGENT_DECK_REQUIRE_REAL_E2E=1`** on its run step (`REQUIRE_REAL_E2E_ENV` in `tests/common/mod.rs`), which turns every runtime skip into a **panic**. That is what makes a green lane-2 run *proof* rather than an absence of evidence. It is set unconditionally and there is deliberately **no off-switch** — no `if:`, no per-trigger branch, no repository variable — because an off-switch is precisely the mechanism by which the vacuous green comes back, and it would come back invisibly. What the flag needs instead is a *named* exclusion for the tests that genuinely cannot run here, which the run step's `-E` filterset provides: exactly `devin_live_001_real_interactive_turn_drives_the_card_live` and `dispatch_013_orchestration_surfaces_and_delegates`, each justified below. An exclusion list is reviewable in the diff; a global off-switch names nothing.

One consequence to know before reading a run: under this flag the *e2e runtime skips* count in the run summary is **0 by construction** on any run that passes, because `_skip_if_err` panics before it prints. It is a consistency check on the flag, not independent evidence — a non-zero count on a green run means the flag stopped reaching the tests. The evidence that agents ran is now the job's colour.

Both keys are also checked for emptiness by a guard step placed immediately before the credentialed steps. That is no longer the thing standing between the job and a vacuous green — the flag is — but it is still worth keeping: without it a missing secret surfaces as 26 separate preflight failures deep in a 90-minute run, and with it the job stops in seconds naming the secret and where to set it. It sits **last rather than first**, because CLAUDE.md rule 8's reason for step 1 is that a late failure leaves a half-done release and a test job has no half-done state; placing it late lets a keyless run exercise the checkout, the toolchain, the agent CLIs and the whole build before it stops. It tests emptiness **after trimming**, matching `check_pi_available`'s `.trim()` and `common::anthropic_api_key`'s. Its honest limit is unchanged: it catches unset, empty and whitespace-only, not a key that is expired, revoked or over its spend cap. Note the consequence for forks — GitHub withholds secrets from a fork `pull_request`, so labelling a fork PR produces a *red* advisory job saying lane 2 cannot run there, which is the intended reading rather than a green tick standing in for coverage that never happened.

Locally the flag is opt-in, and worth setting whenever you want "cannot run" to read as UNVERIFIED rather than green:

```sh
DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test-e2e-live <filter>
```

### 2. What an API key does and does not unlock

Issue #502/#785 chose an **API key** over the owner's OAuth credential set: scopable, spend-cappable, independently revocable, and revoking it logs nobody out of anything. The cost was that most preflights did not accept one. Three of them now do.

`check_claude_available` accepts a non-empty `ANTHROPIC_API_KEY` as a **third path**, consulted *after* `~/.claude/.credentials.json` and the macOS Keychain — so a developer with a real credential set authenticates exactly as before, and the key is an addition rather than a replacement. `check_opencode_available` gained the same third path, offered only for an `anthropic/…` test model (the harness forwards that key and no other, so opening the gate for an `openai/…` model would turn a clean skip into a failure deep in a PTY wait). `check_pi_available` always worked this way. `check_codex_available` is deliberately **unchanged**: codex reads its credential from a file, so the workflow provisions `~/.codex/auth.json` from `OPENAI_API_KEY` in one non-interactive command (`printenv OPENAI_API_KEY | codex login --with-api-key`) and the gate's live model probe stays as the real proof of reachability.

Three harness changes ride along with the claude one and are **not optional** — the import and the seeding sit inside `launch_with_fixture`, which panics rather than skips, so widening the gate alone would have converted 22 silent skips into 22 hard panics:

- `import_claude_credentials` writes **no** credentials file when the key is what authorises the run, instead of hard-failing on the absent host file.
- `seed_claude_project_trust` pre-answers Claude Code's *"Detected a custom API key in your environment"* prompt in the per-test `~/.claude.json`. That prompt defaults to **No**, so without the seed an unattended interactive agent stalls forever. It is recorded under the key's **last 20 characters**, and one rule decides which answer gets written: **the harness never silently moves a run onto metered API billing.** It may move a run off it, and it may move a run onto it when something explicit authorises that — never on its own. The two branches below are that rule applied.
- `inherit_pass` lets `ANTHROPIC_API_KEY` cross the harness's `env_clear` into the spawned deck, so the daemon-spawned agent actually receives it.

**OAuth usable — a developer's machine.** The key is recorded **rejected**, and an inherited `approved` entry is *revoked* rather than preserved. The revocation is the part that needed measuring, because it only matters if Claude Code actually prefers an approved key over a credential file it already has. It does. Measured on claude 2.1.252 against a real credential set, three isolated HOMEs differing in nothing but this field:

| host `~/.claude.json` says | ambient key | interactive header |
| --- | --- | --- |
| `approved` | exported | `Haiku 4.5 · API Usage Billing`, plus *"Both claude.ai and ANTHROPIC_API_KEY set · auth may not work as expected"* |
| `rejected` | exported | `Haiku 4.5 · Claude Team` |
| neither | absent | `Haiku 4.5 · Claude Team` |

So an approved key **wins** over a usable OAuth file, and the rejected case is byte-identical in outcome to having no key at all. That host approval was recorded for the developer's own interactive sessions, at a time when `inherit_pass` was PATH-only and the key never reached a test agent; honouring it here would newly bill them for a test run they did not ask to pay for, while overriding it inside the isolated HOME costs them nothing, since the OAuth set the harness just imported is what runs instead. A developer who *wants* metered local runs reaches for the same lever CI has — no usable OAuth credential.

**OAuth unusable — a CI runner, or a key-only developer host.** The key is recorded **approved**, because it is the only way in — *unless* the host config already records a **rejection** for this exact key and nothing authorises overriding it. Then the refusal stands, and `check_claude_available` refuses the run naming the reason rather than letting it proceed to a silent bill. `DOT_AGENT_DECK_REQUIRE_REAL_E2E` is the authorisation, and `e2e-live.yml` sets it unconditionally on the run step, so CI overrides exactly as it did before. This replaced an unconditional approve-and-drop-the-refusal whose stated justification — that a runner would otherwise inherit a developer's refusal — does not survive checking: `ubuntu-latest` starts with a **fresh HOME** and nothing copies `~/.claude.json` onto it, so the removal was a no-op in CI and took effect only on a local key-only host, which is the one place the stored "No" is a real, deliberate human decision. Ambient key presence is not consent either; this repository's dev environment loads API keys automatically.

Measured across the 24 live files, by preflight and by test function:

| preflight | test functions | before | now |
| --- | --- | --- | --- |
| `check_claude_available` | 22 | skip | **run** (21 selected; `dispatch_013` excluded, below) |
| `check_codex_available` | 5 | skip | **run**, once `~/.codex/auth.json` is provisioned — subject to the model question below |
| `check_pi_available` | 5 | 3 run, 2 skip | **run** (all 5) |
| `check_opencode_available` | 2 | skip | **run** |
| `check_devin_available` | 1 | skip | excluded by name — permanent, below |

Those rows sum to 35 but cover **33 distinct tests**, because two are gated *twice*: `pi_live_002_native_seeded_orchestration_delegates_live` and `chain_smoke_pi_001_orchestrator_delegates_to_real_worker` each call both `check_pi_available()` **and** `check_claude_available()`. That is why the "before" column reads 3 of the 5 pi tests rather than 5 — a fact this page previously got wrong, claiming *5 of 35*. The honest before-figure was **3 of 33**.

After the change, **31 of the 33 gated tests are selected and required to run**: 26 authorised by `ANTHROPIC_API_KEY` and 5 by the provisioned codex credential. The two that are not:

- **`devin_live_001_…` — a deliberate PERMANENT skip.** `devin auth login` offers only a browser redirect or a manual paste-a-token flow, so there is no API-key path at all; the credential would be a personal Cognition account session, on a public repository. Devin also bills every inference call. `devin` is not installed on the runner and the test is excluded by name. Revisit only if Devin ships a non-interactive, scopable credential.
- **`dispatch_013_orchestration_surfaces_and_delegates`** additionally requires a non-empty `GITHUB_TOKEN` and drives a **live GitHub fixture repository** — clone, per-issue worktree, remote-write leak assertions. Handing a credentialed job a repository token is a #785 decision in its own right and has not been taken, so including it would fail the job on a secret nobody deliberately granted. Enabling it is a separate, deliberate act.

**One thing is genuinely undetermined and only a CI run can settle it:** whether this repository's `OPENAI_API_KEY` can reach `gpt-5.1-codex-mini` (`CODEX_TEST_MODEL_DEFAULT`). Confirming it off a runner would mean writing the real key into an `auth.json`, and the dev box this was written on holds a ChatGPT-subscription `auth.json`, which that model family refuses outright. The five codex tests are therefore **not** excluded: if the probe fails they go red naming it, which is how the question gets answered. The remedy is then `DOT_AGENT_DECK_CODEX_TEST_MODEL` on the run step — not dropping the flag.

### 3. The four npm pins are not tracked by Renovate

`e2e-live.yml` pins `@anthropic-ai/claude-code`, `@earendil-works/pi-coding-agent`, `opencode-ai` and `@openai/codex` to exact versions. `renovate.json`'s customManagers cover the `toolchain:` pins and the `cargo-nextest` pin under `.github/workflows/`, not npm packages, so **nothing bumps these four**. They need a deliberate manual bump, and agent CLIs move fast. The claude and opencode pins are the versions the API-key path was measured against; the codex pin is the version `codex login --with-api-key` was measured against, deliberately not the newer release, because nothing has yet exercised that command on a runner.

## Where a rendered credential can go, and what stops it

Lane 2 hands a real API key to a process that draws a terminal, and everything that terminal draws is captured somewhere. The #785 audit walked those sinks; one was open, and the three fixes below close it and its two neighbours.

**Diagnostics, not just recordings — and it is one sink, not two.** The harness already redacted every *persisted* recording: `final-grid.txt`, its SVG, `fixture.toml`, and the cross-event `full-stream.cast` (which handles a credential split across two PTY reads). It did not redact **captured test output**. `snapshot_grid()` and `stream_text()` return raw terminal content, harness timeouts interpolate a raw grid into their panic messages, and dozens of live tests interpolate `deck.snapshot_grid()` straight into an `assert!`. nextest writes those panics into the run log and into the raw JUnit report, and GitHub masks a registered secret's exact value in a rendered log — never a 20-character substring of it.

What makes that a blocker rather than a theoretical gap is the correlation: the condition that renders the key's suffix on screen (API-key approval seeding absent, stale, or incompatible with a future Claude Code release) is *the same* condition that then hangs the agent on the prompt and kills the test at a PTY wait. The leak path and the panic path are one path.

The fix is a process-global **redacting panic hook** (`install_credential_redaction` in `tests/common/mod.rs`), not a redaction on the accessors. Three properties are worth knowing before touching it:

- **Raw stays raw.** `snapshot_grid()` and `stream_text()` still return real content, so assertions keep matching. Redaction happens at the **output seam** — the one place every panic message in the process passes through on its way to stderr.
- **It reaches the paths no per-deck object can.** `e2e_pi_orchestrator.rs`, `e2e_delegate_work_done_chain.rs` and `e2e_codex_worker.rs` drive an *in-process* daemon and read pane bytes off `daemon.registry.snapshot()`. They have no `TuiDeck`, so they have no `recording_redactions` and never will — the audit named them as having no redaction of any kind. A process-global store reaches them for free.
- **Installation is an invariant, not a convention.** Every harness entry that can obtain terminal content or handle a credential installs the hook first: `TuiDeckBuilder::launch`, every importer, every `check_*_available`, `seed_claude_worker_home`, `snapshot_grid`, `stream_text`. So "a grid exists in this process" implies "the hook is installed", whatever route the test took. `set_hook` is process-global, so spawned threads are covered too.

Its honest limits: a test that `println!`s terminal content bypasses the hook entirely (none does, and `no_test_prints_terminal_content_outside_the_redacting_panic_seam` keeps it that way, naming `redact_credentials_for_output` as the remedy for the day one legitimately needs to); it can only redact values the harness knows about, which is why every importer registers what it copied; and a credential the vt100 grid wrapped across a line boundary is two substrings, not one — the same limit the recording redaction has always had.

**Codex credentials are registered like the others.** `CredentialImport::Codex` used to return no redactions at all, which was survivable while `~/.codex/auth.json` was only ever a developer's own ChatGPT session. It stopped being survivable when this workflow began *producing* that file from the raw `OPENAI_API_KEY`: `import_codex_credentials` copies those bytes verbatim into every isolated Codex HOME. It now returns targeted redactions through the same `collect_credential_values` policy the OpenCode and Devin importers use, and registers them for diagnostic redaction itself — so the in-process `e2e_codex_worker.rs` route is covered without having to remember to.

**Neither credential outlives its step.** A `Purge the run's credentials from the runner` step sits immediately after the test command, `if: always()`, before every post-processing step, before `actions/upload-artifact`, and before the `Swatinem/rust-cache` post-action — all of which run as the same user. Position is the whole point; a cleanup after those is worth much less. It removes `~/.codex`, stops the harness daemons and agents (whose *environments* hold `ANTHROPIC_API_KEY`, and which `TuiDeck::drop` explicitly cannot kill because the daemon `setsid`s into its own session), and removes the harness temp roots, whose isolated HOMEs each hold a copy of `auth.json` and are only self-cleaning when a test *passes*. It deliberately does not touch `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` — CLAUDE.md rule 14 documents why that number is load-bearing for the temp-dir reaper. An auth file that survives its own removal fails the job loudly; everything else is best-effort, because a run that left no daemon behind is the expected outcome.

The runner is ephemeral and no step in this file consumes either sink today, so all three are defence in depth rather than an incident fix. What they buy is that "step-level `env:`" stops being a misleading description of a credential's lifetime, and that a step added *below* the purge cannot silently inherit one.

## Maintenance notes

- **Both timeouts are derived, not measured.** Nothing had ever run the e2e tier on a GitHub runner before these jobs existed, so lane 1's 60 minutes comes from the tier's own declared kill windows (`lifecycle/version/001` alone carries a 120s x 10 window and pays a cold nested dependency build) and lane 2's 90 from that plus the real-agent files, several of which sit at 300-540s. Lane 2's figure is now doing more work than when it was written: it was derived while 3 of 33 gated tests could run, and 31 of them now do. Re-tune both once honest runs exist; do not raise either to paper over a hang.
- **The first lane-2 runs are the measurement, so read them as such.** Nothing in the credential path has ever executed on a runner. The claude, opencode and pi halves were measured directly on a dev box with a relocated HOME carrying no credential file at all — an interactive, tool-using Haiku agent, API-key authenticated — but a runner is still a new variable, and the codex model question above is genuinely open. Expect the first run to teach you something; do not respond to it by loosening `DOT_AGENT_DECK_REQUIRE_REAL_E2E`.
- **`check-pin-lockstep.sh` now covers this workflow.** `e2e-live.yml` carries both a `toolchain:` and a `cargo-nextest` pin, so the script's site counts went from 7 and 4 to **9 and 5**. `pin_lockstep.rs` runs it inside `cargo test-fast`, so a drifted pin here goes red on the per-task gate.
- **The `live` job's `if:` is an ALLOWLIST of event names, so adding a trigger takes two edits.** Putting a `workflow_run`, a `schedule` or a `repository_dispatch` in the `on:` block is not enough — the job also has to name it in the condition, or it silently never runs. That is deliberate for the one job in the repository that holds an agent credential: a new trigger that does not run is a missing line and an obvious one to add, while a new trigger that runs *with the key* would be a security decision nobody made. Fail closed, not open.
- **Lane 2 has a repository-wide concurrency group with `cancel-in-progress: false`.** These runs spend real tokens against one account, so two overlapping runs race for the same rate limit and can fail each other for reasons unrelated to the code. A run that is halfway through has already spent the money; let it finish and queue the next.
- **The build and the run are separate steps on purpose.** `cargo nextest list` builds the test binaries with **no secret in the environment**, because `build.rs` is first on #785's list of surfaces a credentialed job exposes — arbitrary code at compile time, ahead of any test. The run step then finds everything fresh. Be precise about what that buys: it is a freshness guarantee, not a hermetic seal. `cargo nextest archive` plus `--archive-file` would be one, at the cost of a workspace-remap round trip this lane cannot validate until the secret exists.
