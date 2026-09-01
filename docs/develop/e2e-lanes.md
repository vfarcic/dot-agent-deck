# The e2e lanes

Issue #502 split the L2 e2e suite in two by whether a test reaches a **real agent**. **CLAUDE.md rule 5 is the policy** — the three tiers of obligation, and why there is no longer a pre-PR obligation to run the tier in full. This page is the operational half: how to actually drive each lane, and what a green run does and does not prove.

## What each lane is

| | lane 1 | lane 2 |
| --- | --- | --- |
| command | `cargo test-e2e` | `cargo test-e2e-live` |
| cargo features | `e2e` | `e2e,e2e-live` |
| files | the 47 `tests/e2e_*.rs` that reach no real agent | **all 71** — lane 1 *plus* the 24 that do |
| where | the `e2e-deterministic` job in `.github/workflows/ci.yml` | **your machine, and nowhere else** |
| when | every PR | when you touch what it covers |
| credentials | none in the job's environment at all | your own |

`cargo test-e2e-live` is a **superset** run, not a live-only run. The 24 credentialed files open with `#![cfg(all(feature = "e2e", feature = "e2e-live"))]`, so `e2e-live` without `e2e` compiles every e2e file to an empty crate; there is deliberately no live-only alias. Filter it (`cargo test-e2e-live claude_001`) rather than running it whole.

Lane 1 is not a required status check. The required set is still `build`, `build-macos`, `build-windows` and `security`; rule 8 records why it is deliberately advisory.

## Why lane 2 is not in CI

**No test that reaches a real agent runs in CI, and no agent credential is registered on this repository.** That is a decision with two reasons, and both have to be re-argued before anyone puts a key back.

**Frequency is not a mitigation.** The earlier design ran the credentialed lane per-merge on `main` rather than per-PR, behind a `live-e2e` GitHub Environment with a required reviewer and a `run-live-e2e` label. Every one of those controls reduces the *number of exposures*; none of them reduces the *existence of the vulnerability*. Once an API key is a secret on a public repository, it is reachable by whatever can make that workflow run — and the whole point of a pre-merge validation lane is that branch code, including the workflow definitions themselves, executes with it. Running that less often is risk theatre. The fix actually available is to not put the key there.

**The boundary is "reaches a real agent", not "reads a key".** This is the part that makes the classification worth keeping even with no CI job behind it. On a developer's machine the agent CLIs are *pre-authenticated* — `~/.claude/.credentials.json`, the macOS Keychain, `~/.codex/auth.json` — so a test that spawns `claude` and never touches `ANTHROPIC_API_KEY` still spends a credential and still bills someone. Splitting on "does this test read a secret" would have left those in lane 1. The `e2e-live` cargo feature draws the wider line across exactly 24 files, and that is why the feature, the `test-e2e-live` alias, `bacon.toml`'s job and all 24 `#![cfg(all(…))]` attributes stay.

**Where the deleted workflow went.** `.github/workflows/e2e-live.yml` was removed rather than parked as `workflow_dispatch`-only: an unwired credentialed workflow sitting on `main` is a trap for whoever next registers a secret for an unrelated reason. Git history holds the working implementation — commit `edaa2b4` on `main` (the version merged by PR #800) and `62ba861` on branch `agent/e2e-live-credential-sinks` (the final version, with the credential purge and the inheritance-based containment). Read those if the decision is ever revisited; do not restore them without re-arguing the two reasons above.

**What CI still does for lane 2.** Exactly one thing, and it is load-bearing: `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` in the `build` job **type-checks** all 24 files. That is the only CI-side coverage they have. CLAUDE.md rule 2 says the same thing at more length, because "simplify the feature list" is a plausible-looking edit that silently removes a third of the tier from every gate.

**The verification gap is an accepted trade, not an oversight.** Nothing can force a contributor to run lane 2. The only thing that could is CI, and CI has no credentials by design. So real-agent regressions surface when someone happens to run them, not on any schedule. That cost was weighed against holding an API key in a public repository's CI and judged the cheaper side.

## Running lane 2

```sh
cargo test-e2e-live claude_001                              # one test
cargo test-e2e-live chain_smoke                             # a module
DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test-e2e-live pi_    # and fail rather than skip
```

You need the agent CLIs the filtered tests drive (`claude`, `opencode`, `codex`, `pi`) installed and authenticated, or a usable `ANTHROPIC_API_KEY` — see the next section for which preflights accept which. Locally the devbox lacks Tauri's GTK/WebKit deps (issue #771), so a `--workspace` run may need `--exclude dot-agent-deck-desktop`.

### A skip is a pass, so set the flag

Real-agent tests open with `skip_unless!(check_<agent>_available())`. That macro prints `SKIP: [e2e] <reason>` and **returns normally**, so nextest counts the test as **passed**. An absent or unusable credential therefore removes coverage silently rather than reddening the run — and because `cargo test-e2e-live` is a superset run, lane 1's 47 deterministic files carry the result green on their own. "It passed" and "no agent ran" look identical.

`DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` (`REQUIRE_REAL_E2E_ENV` in `tests/common/mod.rs`) turns every runtime skip into a **panic**. Set it whenever you want "cannot run here" to read as UNVERIFIED rather than as green. Without it, read the `SKIP:` lines rather than the colour:

```sh
cargo test-e2e-live --success-output=final <filter> 2>&1 | grep -E '^\s*SKIP: \[e2e\] '
```

Two details of that grep are load-bearing. The leading `\s*` is there because nextest indents captured output by four spaces, so a `^SKIP: ` anchored at column 0 matches nothing on a run that really did skip (issues #452, #490). The `[e2e]` marker is there because `SKIP: ` alone does not identify the e2e harness — the aliases carry `--workspace`, so a run also selects `xtask/` and the root package's unit tests, several of which print their own `SKIP:` lines when `python3`, `node`, `jq` or `bash` is missing. `_skip_if_err` in `tests/common/mod.rs` prints the marker; drop it from the pattern and you are counting those too. `--success-output=final` is needed at all because a runtime skip is a *successful* test, and nextest suppresses a successful test's output by default.

### What an API key does and does not unlock

Issue #502/#785 chose an **API key** over the owner's OAuth credential set: scopable, spend-cappable, independently revocable, and revoking it logs nobody out of anything. That decision is now moot for CI — there is no CI credential — but it still governs a local run on a machine with no agent CLI logged in, which is the case for a fresh checkout or a container.

`check_claude_available` accepts a non-empty `ANTHROPIC_API_KEY` as a **third path**, consulted *after* `~/.claude/.credentials.json` and the macOS Keychain — so a developer with a real credential set authenticates exactly as before, and the key is an addition rather than a replacement. `check_opencode_available` gained the same third path, offered only for an `anthropic/…` test model (the harness forwards that key and no other, so opening the gate for an `openai/…` model would turn a clean skip into a failure deep in a PTY wait). `check_pi_available` always worked this way. `check_codex_available` is deliberately **unchanged**: codex reads its credential from a file, so a key-only host provisions `~/.codex/auth.json` with `printenv OPENAI_API_KEY | codex login --with-api-key` and the gate's live model probe stays as the real proof of reachability.

Three harness changes ride along with the claude one and are **not optional** — the import and the seeding sit inside `launch_with_fixture`, which panics rather than skips, so widening the gate alone would have converted 22 silent skips into 22 hard panics:

- `import_claude_credentials` writes **no** credentials file when the key is what authorises the run, instead of hard-failing on the absent host file.
- `seed_claude_project_trust` pre-answers Claude Code's *"Detected a custom API key in your environment"* prompt in the per-test `~/.claude.json`. That prompt defaults to **No**, so without the seed an unattended interactive agent stalls forever. It is recorded under the key's **last 20 characters**, and one rule decides which answer gets written: **the harness never silently moves a run onto metered API billing.** It may move a run off it, and it may move a run onto it when something explicit authorises that — never on its own. The two branches below are that rule applied.
- `INHERIT_PASS` lets `ANTHROPIC_API_KEY` cross the harness's `env_clear` into the spawned deck, so the daemon-spawned agent actually receives it.

**OAuth usable — the ordinary developer machine.** The key is recorded **rejected**, and an inherited `approved` entry is *revoked* rather than preserved. The revocation is the part that needed measuring, because it only matters if Claude Code actually prefers an approved key over a credential file it already has. It does. Measured on claude 2.1.252 against a real credential set, three isolated HOMEs differing in nothing but this field:

| host `~/.claude.json` says | ambient key | interactive header |
| --- | --- | --- |
| `approved` | exported | `Haiku 4.5 · API Usage Billing`, plus *"Both claude.ai and ANTHROPIC_API_KEY set · auth may not work as expected"* |
| `rejected` | exported | `Haiku 4.5 · Claude Team` |
| neither | absent | `Haiku 4.5 · Claude Team` |

So an approved key **wins** over a usable OAuth file, and the rejected case is byte-identical in outcome to having no key at all. That host approval was recorded for the developer's own interactive sessions, at a time when `INHERIT_PASS` was PATH-only and the key never reached a test agent; honouring it here would newly bill them for a test run they did not ask to pay for, while overriding it inside the isolated HOME costs them nothing, since the OAuth set the harness just imported is what runs instead. A developer who *wants* metered local runs reaches for the same lever a key-only host has — no usable OAuth credential.

**OAuth unusable — a key-only host.** The key is recorded **approved**, because it is the only way in — *unless* the host config already records a **rejection** for this exact key and nothing authorises overriding it. Then the refusal stands, and `check_claude_available` refuses the run naming the reason rather than letting it proceed to a silent bill. `DOT_AGENT_DECK_REQUIRE_REAL_E2E` is the authorisation: set it and the refusal is overridden, because setting it is an explicit statement that this run must reach a real agent. This replaced an unconditional approve-and-drop-the-refusal whose stated justification — that a CI runner would otherwise inherit a developer's refusal — no longer describes anything real, since there is no runner. What is left is the one place the stored "No" is a deliberate human decision: a local key-only host. Ambient key presence is not consent either; this repository's dev environment loads API keys automatically.

Measured across the 24 live files, by preflight and by test function:

| preflight | test functions | with a real credential set | key-only host |
| --- | --- | --- | --- |
| `check_claude_available` | 22 | run | **run** |
| `check_codex_available` | 5 | run | **run**, once `~/.codex/auth.json` is provisioned — subject to the model question below |
| `check_pi_available` | 5 | run | **run** |
| `check_opencode_available` | 2 | run | **run** (`anthropic/…` test model only) |
| `check_devin_available` | 1 | run | **cannot** — no API-key path exists |

Those rows sum to 35 but cover **33 distinct tests**, because two are gated *twice*: `pi_live_002_native_seeded_orchestration_delegates_live` and `chain_smoke_pi_001_orchestrator_delegates_to_real_worker` each call both `check_pi_available()` **and** `check_claude_available()`.

Two are worth knowing about before you run a filter that selects them:

- **`devin_live_001_…`** — `devin auth login` offers only a browser redirect or a manual paste-a-token flow, so there is no API-key path at all, and Devin bills every inference call. Revisit only if Devin ships a non-interactive, scopable credential.
- **`dispatch_013_orchestration_surfaces_and_delegates`** additionally requires a non-empty `GITHUB_TOKEN` and drives a **live GitHub fixture repository** — clone, per-issue worktree, remote-write leak assertions. It is runnable locally with your own token; it was excluded by name from the credentialed CI job that no longer exists.

**One thing is genuinely undetermined:** whether an `OPENAI_API_KEY`-derived `auth.json` can reach `gpt-5.1-codex-mini` (`CODEX_TEST_MODEL_DEFAULT`). The dev box this was written on holds a ChatGPT-subscription `auth.json`, which that model family refuses outright, and nothing has since exercised `codex login --with-api-key` end to end. If the probe fails on your machine, the remedy is `DOT_AGENT_DECK_CODEX_TEST_MODEL` — not dropping the preflight.

## Where a rendered credential can go, and what stops it

Lane 2 hands a real credential to a process that draws a terminal, and everything that terminal draws is captured somewhere. The #785 audit walked those sinks; one was open, and the fixes below close it and its two neighbours.

**The reason this matters is now LOCAL artifacts, and that is a stronger reason than CI ever was.** While lane 2 ran in CI the story was about a job log and an uploaded JUnit report — behind a GitHub Environment, with GitHub's secret masking in front of rendered logs. None of that exists any more, and what replaced it has *no* gate at all:

- **`.cast` recordings are uploaded to YouTube and the link is published.** A PTY-attached test writes `full-stream.cast` under `.dot-agent-deck/recordings/<test>/` when it fails or under `DOT_AGENT_DECK_RECORD=1`. `.claude/skills/demo-reel-adapter` selects those casts, `.claude/skills/demo-reel` stitches them into an MP4 and `--publish` uploads it **unlisted to YouTube**, and the URL goes into the PR body *and* the changelog fragment, which flows into the **public release notes** (PRD #180/#20, [`demo-reel.md`](demo-reel.md)). Unlisted means unsearchable, not private: anyone with the link can watch. And this path selects for exactly the risky recordings — a clip is only eligible if it spins up a **real agent**, which is to say if it is a lane-2 test. There is no environment gate, no reviewer and no secret masking anywhere along it.
- **JUnit reports carry failed and retried tests' stdout and stderr.** nextest's defaults store both, and `target/nextest/default/junit.xml` is written by the cargo process holding the credential. It is exactly the file someone attaches to an issue or pastes into a PR when a live test fails. Run `scripts/junit-strip-output.py` over it first — it rebuilds the document from a per-element attribute whitelist, so the result cannot carry free text at all. `python3 scripts/junit-strip-output.py --self-test` proves it.
- **Panic output goes wherever a developer pastes it.** A harness timeout interpolates a raw grid into its message; a failing assertion interpolates `deck.snapshot_grid()`. That text lands in a terminal, in a transcript, in an issue, in a message to another agent.

So the redaction below is not a leftover from the CI design. It is the only thing standing between a credential and a published video.

**Diagnostics, not just recordings — and it is one sink, not two.** The harness already redacted every *persisted* recording: `final-grid.txt`, its SVG, `fixture.toml`, and the cross-event `full-stream.cast` (which handles a credential split across two PTY reads). It did not redact **captured test output**. `snapshot_grid()` and `stream_text()` return raw terminal content, harness timeouts interpolate a raw grid into their panic messages, and dozens of live tests interpolate `deck.snapshot_grid()` straight into an `assert!`.

What makes that a blocker rather than a theoretical gap is the correlation: the condition that renders the key's suffix on screen (API-key approval seeding absent, stale, or incompatible with a future Claude Code release) is *the same* condition that then hangs the agent on the prompt and kills the test at a PTY wait. The leak path and the panic path are one path.

The fix is a process-global **redacting panic hook** (`install_credential_redaction` in `tests/common/mod.rs`), not a redaction on the accessors. Three properties are worth knowing before touching it:

- **Raw stays raw.** `snapshot_grid()` and `stream_text()` still return real content, so assertions keep matching. Redaction happens at the **output seam** — the one place every panic message in the process passes through on its way to stderr.
- **It reaches the paths no per-deck object can.** `e2e_pi_orchestrator.rs`, `e2e_delegate_work_done_chain.rs` and `e2e_codex_worker.rs` drive an *in-process* daemon and read pane bytes off `daemon.registry.snapshot()`. They have no `TuiDeck`, so they have no `recording_redactions` and never will — the audit named them as having no redaction of any kind. A process-global store reaches them for free.
- **Installation is an invariant, not a convention.** Every harness entry that can obtain terminal content or handle a credential installs the hook first: `TuiDeckBuilder::launch`, every importer, every `check_*_available`, `seed_claude_worker_home`, `snapshot_grid`, `stream_text`. So "a grid exists in this process" implies "the hook is installed", whatever route the test took. `set_hook` is process-global, so spawned threads are covered too.

**A credential the terminal WRAPPED is matched too, and the arithmetic is why this is not a footnote.** An Anthropic key is 108 characters; the harness renders at 120 columns; an `ANTHROPIC_API_KEY=` line therefore starts the value at column 19 and breaks it **102 / 6**, which splits the key's 20-character response id **14 / 6**. Byte-exact matching found neither piece, so the entire credential was reconstructable out of a panic grid while both registered patterns matched nothing — and that grid goes into whatever the developer pastes. It is not only the grid: the same value is split the same way in `full-stream.cast`, one layer lower, where a row change is the deck's own cursor-position escape rather than a newline — and `full-stream.cast` is the file the reel publishes.

`credential_redaction_ranges` now bridges **row transitions**: after a `\n`, a `\r` or an `ESC`, a value may resume anywhere within 4 KiB, provided each hop reproduces at least 8 more bytes of it exactly. The eight-byte floor is the safety bound — without it, "match, skip, match again" is a subsequence search, and almost any text contains almost any string as a subsequence. The bytes *between* fragments are deliberately left in place: in a grid they are the newline, the pane border and the neighbouring column, and in a `.cast` they are the escapes that keep it replayable. Arbitrary bytes rather than a whitelist of frame characters, because the deck's usual layout puts a sidebar between two rows of a pane, not padding. A contiguous occurrence still produces exactly one replacement, so this is a strict superset of what it replaced, and all four sinks inherit it from that one function.

**Left in place is a statement about the artifact, not about what the scan read — and the first version of this conflated the two.** It jumped the scan cursor to the *end* of a fragmented match, so every byte it had preserved was skipped, and preserved bytes are arbitrary rendered content that on a real grid holds whatever else was on screen. That includes the OTHER registered value: `api_key_recording_redactions` always registers the key **and** its response id together, and the id is a *suffix* of the key — so the six-byte run that resumes the key on the next row also occurs, fourteen characters earlier, inside a response id rendered on that same row. The matcher chained through that earlier occurrence, advanced past the whole match, and never looked at the fourteen characters of the id it had stepped over, nor at the real six-byte continuation further along. Both survived; the complete 20-character derivative was reconstructable while every contiguous assertion passed. Measured by the security audit of PR #805 with the exact registered pair, in the real geometry.

Two changes close it, and each is load-bearing on its own half. The scan now resumes at the end of a match's **first** fragment rather than past its last, so a preserved gap is read like any other bytes — that covers the 102 / 6 case above. And `fragment_candidates` returns **every** place a hop could have resumed, not just the first: the chain still follows the first and still never backtracks, but the alternates reproduce the same credential bytes exactly and are redacted too. That second half is what covers a pane, where 110 inner columns break the key **92 / 16** and the run left behind by the coincidence is sixteen characters of key material rather than six. `two_registered_values_interleaved_across_rows_are_both_redacted` pins both geometries, and each half was confirmed non-vacuous by disabling it and watching that test name the surviving fragment. Ranges can now overlap before `merge_redaction_ranges` coalesces them, which is why they are sorted rather than assumed ascending.

Cost stays linear in the input for a fixed credential set. An alternate-collecting hop pays the full `MAX_WRAP_GAP × MIN_WRAP_FRAGMENT` window that a candidate-free gap already paid, and only when the remaining tail is a full eight bytes; a short tail still stops at the first candidate. Measured over a synthetic stream of the wrapped pair plus filler: 2.1 MB in 10.6 ms before, 11.1 ms after, both scaling 1:1 with size. The adversarial shape the audit named — a stream packed with exact eight-byte credential prefixes that never chain — goes from 699 ms to 895 ms per 1.6 MB, still linear, still a constant factor rather than a curve.

**What this deliberately does not claim.** `MIN_WRAP_FRAGMENT = 8` is a floor on evidence, so a run *shorter* than it can survive: when a coincidence takes the chain, the real six-byte remainder of a 102 / 6 break stays in the artifact. Six bytes is below the bar this matcher states for evidence everywhere else, which is the reason the floor sits where it does rather than an exception to it. In the other direction the floor over-redacts, and `collect_credential_values` widens that surface by registering **every** auth-document string of 16 characters or more, sensitive field name or not — so human-readable values (a URL, an account label, a model name) are registered too and genuinely can share an eight-byte prefix with unrelated rendered text. It is kept at eight deliberately: because the matcher replaces matching runs and leaves the layout between them, a false positive obscures a fragment of a panic rather than destroying it, whereas raising the floor misses a 102 / 6 tail, which is the leak this seam exists to stop. Narrowing what `collect_credential_values` registers is the fix if this ever bites, and it is deliberately not made on a guess — a value that stops being registered stops being redacted.

One measured subtlety worth keeping: `vt100::Screen::contents()` suppresses the row separator for a row the *terminal* auto-wrapped, so a test that writes a long line and lets the terminal wrap gets the value back **contiguous** and proves nothing. ratatui never auto-wraps — it positions the cursor per row — so the deck's grid always takes the separator path. The regression tests paint their rows explicitly for exactly that reason.

Its honest limits: a test that `println!`s terminal content bypasses the hook entirely (none does, and `no_test_prints_terminal_content_outside_the_redacting_panic_seam` keeps it that way, naming `redact_credentials_for_output` as the remedy for the day one legitimately needs to); and it can only redact values the harness knows about, which is why every importer registers what it copied. That print scanner now also follows a binding — `let grid = deck.snapshot_grid(); eprintln!("{grid}")` used to walk straight past it — and the panic-hook scanner now matches the *call* rather than a `panic::`-qualified path, so an imported `set_hook` no longer slips through.

**Codex credentials are registered like the others.** `CredentialImport::Codex` used to return no redactions at all, which was survivable while `~/.codex/auth.json` was only ever a developer's own ChatGPT session. It stopped being survivable once that file could be *produced* from a raw `OPENAI_API_KEY` on a key-only host: `import_codex_credentials` copies those bytes verbatim into every isolated Codex HOME. It now returns targeted redactions through the same `collect_credential_values` policy the OpenCode and Devin importers use, and registers them for diagnostic redaction itself — so the in-process `e2e_codex_worker.rs` route is covered without having to remember to.

## Maintenance notes

- **Nothing here is exercised by CI, so it rots quietly.** Lane 2's harness paths — the preflights, the importers, the trust seeding, the redaction seam — are compiled by rule 2's clippy gate and *run* by nobody unless a developer runs them. When you touch any of them, run the tests that cover them and say which ones in your report.
- **Lane 1's `timeout-minutes: 60` is derived, not measured.** Nothing had run the e2e tier on a GitHub runner before that job existed, so the figure comes from the tier's own declared kill windows (`lifecycle/version/001` alone carries a 120s × 10 window and pays a cold nested dependency build). Re-tune it once honest runs exist; do not raise it to paper over a hang.
- **The agent CLI versions are whatever you have installed.** The credentialed workflow used to pin `@anthropic-ai/claude-code`, `@earendil-works/pi-coding-agent`, `opencode-ai` and `@openai/codex` to exact versions, and Renovate never tracked those pins. With the workflow gone there is no pinned set at all — a lane-2 failure after an agent CLI update is a real possibility, and checking the CLI version is a reasonable first move when a previously-passing real-agent test starts failing on its own.
- **`check-pin-lockstep.sh` no longer counts this workflow.** It scans `.github/workflows/` dynamically rather than against a fixed site count, so the deletion needed no change there; `pin_lockstep.rs` runs it inside `cargo test-fast` as before.
