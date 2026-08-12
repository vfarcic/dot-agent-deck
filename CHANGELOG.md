# Changelog

## [0.36.0] - 2026-08-11

### Changed

- **Attach-protocol bump: shell-activity event types (`PROTOCOL_VERSION` 6 → 7)**
  `EventType` gains two daemon-synthesized, wire-serialized variants, `ShellBusy` and `ShellIdle` — the signal that a pane's agent has a foreground shell command running even though the agent itself has gone quiet (a backgrounded build, a long test suite). Both ride `AgentEvent.event_type` in the daemon→TUI `KIND_EVENT` broadcast, a payload a peer decodes as a whole frame. A build that predates this change has neither variant nor a `#[serde(other)]` fallback for `EventType`, so a `KIND_EVENT` frame carrying either one fails the entire frame decode, not just that one event.
  Classification: this is a cross-version compatibility break for a **pre-existing reader** — an older TUI (or any other client that decodes `KIND_EVENT`) attached to a newer daemon that emits `ShellBusy`/`ShellIdle` — not generic user-facing breakage, so per this project's own rule (`docs/develop/versioning.md`) it is classified `breaking` and `PROTOCOL_VERSION` is bumped from 6 to 7. The attach handshake's `probe_remote_protocol` refuses a mismatched pairing with a clean `ProtocolMismatch` at connect time instead of a mid-session decode failure.
  Mitigation: `EventType` also gains a `#[serde(other)]` catch-all (mirroring `AgentType`'s earlier retrofit) that decodes any future/unrecognized `event_type` string to a neutral `Unknown` variant instead of erroring, so this build and every build after it degrade gracefully — a further event-type addition needs no further bump. Already-released binaries predate that fallback, which is exactly why the `PROTOCOL_VERSION` bump is still needed as the guard for them.
  **Action required: upgrade the TUI and the daemon together.** An old TUI cannot safely attach to a daemon built from this change — `probe_remote_protocol` refuses the connection with `ProtocolMismatch` rather than risk a mid-session frame-decode failure. Pair an upgraded daemon with an upgraded TUI (and vice versa) rather than upgrading one side only.

### Added

- **A Pane Reads Working While Its Agent's Shell Command Runs**
  A pane's status used to come entirely from agent-emitted hooks, so a role whose agent shelled out to something long — `cargo build`, a test suite, a release script — sat on a stale "Idle" while the command was visibly still running. The worst case was the common one: Claude Code moves a command that outlives its ~2-minute limit into a background shell and ends the agent's turn right there, so the pane flipped to "Idle" about two minutes in and stayed wrong for the rest of a ten-minute build.
  The daemon now watches each pane's own process tree for a shell command the agent has launched, and the pane reads Working for as long as one is running — past the point where the command is backgrounded and the agent's turn ends, and equally for a plain foreground command that finishes well inside the limit. The pane returns to Idle when the command actually finishes. This never overrides a more specific status like Thinking, WaitingForInput, or Error; it only fills the gap where nothing more specific is known. A pane sitting at an idle agent prompt is unaffected, including one with MCP servers and other long-lived helpers running underneath it.
  The behaviour is proven against a real agent running a real command — both that a busy pane reads Working across the backgrounding point, and that an idle pane does not.

### Fixed

- **A stuck `ps` can no longer stall the daemon or wrongly mark every pane idle**
  The shell-activity poll ran its process-table sample synchronously on the daemon's async runtime, with no time limit. Two consequences, both now fixed.
  The sample occupied a runtime worker thread for its whole duration, so everything else the daemon does queued behind it — hook ingestion, client requests, even shutdown. Ordinarily that was ~49ms every 500ms; a `ps` wedged in uninterruptible state on a stuck filesystem (a hung NFS mount, a failing disk) stalled the daemon indefinitely, and the 500ms cadence made that a recurring exposure rather than a one-off. The sample is now awaited rather than blocked on, so the worker stays free while `ps` runs, and a 2-second deadline bounds how long any one poll waits for an answer.
  The deadline bounds the wait, not the `ps` process itself, and deliberately so: a process stuck in uninterruptible state cannot be killed until it comes unstuck, so abandoning it and starting a fresh one on the next poll would accumulate unkillable `ps` processes at roughly 24 a minute. A poll that overruns instead keeps waiting on the same sample, so at most one `ps` exists at a time however long the underlying filesystem stays wedged. The overrun is logged once, not once per poll.
  Because that lets a sample answer late, an answer is only acted on while it still describes the current machine — a reading more than 3 seconds old is discarded rather than applied — and only about the panes that were already open when it was taken. Otherwise a filesystem that unwedges after the panes it was asked about have been closed or replaced would have its old reading attributed to whichever panes are open now, which on a machine recycling process ids can mean one pane inheriting another's activity.
  A sample that times out is treated as *no information*, never as "no pane is busy". This distinction is the reason the fix is not simply a shorter wait: a wedged `ps` says nothing about the panes, which are exactly as busy as they were a moment earlier. Reading a blown deadline as "not busy" would have marked every running pane `Idle` — reintroducing the stale-`Idle` badge this signal exists to prevent, with a new trigger and nothing in the log to trace it by. Statuses are instead left untouched until a sample answers, and a timeout is logged as a warning.
- **Bounded Frame Length on the TUI's Synchronous Daemon Client**
  The TUI's one-shot daemon queries now reject an over-long response frame instead of allocating from the length prefix on the wire. `read_frame` has always refused a frame larger than 16 MiB precisely so a forged or corrupted length cannot make the process allocate gigabytes; the synchronous client the TUI uses for its blocking queries read the same 5-byte header without that check and sized its buffer straight off the `u32`. Since the allocation happened before the body was read, a 5-byte header claiming 4 GiB was enough — no payload required.
  This ran on the `Ctrl+n` new-pane key path, where the deck asks the daemon which directories already host a live orchestration, so the visible failure was the TUI ballooning or being killed on a keystroke. Both sides now read the same bound and return the same `InvalidData` error, and every caller already treated a failed request as best-effort, so an over-long frame degrades to "no hint" or a `Run-now failed: …` status line rather than taking the TUI down.
  The wire format is unchanged. A daemon can never have sent a frame this affects — the writer has always enforced the same cap — so builds across versions interoperate exactly as before.
- **The shell-activity poll no longer scans the process table when no pane needs it**
  A running deck stops paying for a process-table scan it has nothing to do with. The shell-activity monitor — the 2Hz poll that spots a pane whose agent is quietly running a long shell command — sampled the whole machine *before* checking whether any pane was open to classify. A daemon with zero panes therefore forked `ps -A` twice a second, each sample also issuing a `getsid(2)` for every process row, to classify nobody. Nothing bounded it: the daemon's idle shutdown requires no clients **and** no agents, so a dashboard sitting attached with nothing open polled for as long as it was left open.
  The cost that buys is now measured rather than assumed: roughly 49ms of wall time per sample on an idle 16-core Linux box with ~620 processes, i.e. about 10% of one core at the 2Hz cadence — of which only ~1.4ms is the deck's own CPU, the rest being the `ps` child and the wait on it. The monitor now resolves which panes exist first and skips the sample entirely when the answer is none, so an idle deck spends nothing here.

### Miscellaneous

  Fixed the e2e test harness allocating its per-process temp root on `/tmp`, which on some machines is a RAM-backed tmpfs ([#322](https://github.com/vfarcic/dot-agent-deck/issues/322)). Interrupted runs leak those roots, and because tmpfs is memory the leaks are resident RAM, not disk — one cleanup freed 5.8 GB and took swap from 5 MiB free to 3.8 GiB free on the machine that reported this. The visible symptom was a healthy `cargo test-e2e` collapsing into a wall of failures that read like real product regressions (`Disk quota exceeded`, `git init` failing, daemons never booting) when the tmpfs simply ran out of room.
  Containment is scoped to what was measured rather than asserted broadly. Live `/tmp` sampling during a recorded full run found three allocations still escaping, ~184 KiB in total: the harness's own Codex-auth pre-flight (a `NamedTempFile`, which `linkage-check` rule 8 did not match — the rule now covers file constructors as well as directory ones), `src/dispatch.rs`'s e2e-gated unit test (which clones a repo, and lives in the lib target that does not link the harness), and `tests/daemon_protocol.rs` (the one fast-tier crate that binds Unix domain sockets, and which `cargo test-e2e` runs). All three are fixed — the first through the harness, the other two through a ~40-line crate-internal resolver (`src/test_temp.rs`) that reaches the same private base without the ladder, the pre-flight or the seeded HOME. `docs/develop/e2e-temp-dirs.md` now states the boundary as a table of what is and is not contained, with the measurement behind each row. Everything under `tests/` is now contained, the seven fast-tier crates that do not link the harness included; the one remaining gap is documented as **not** contained rather than implied to be — the rest of `src/`'s unit tests, roughly 82 bare constructors across 22 files, left uncovered deliberately with no measured leak behind them.
  The harness now resolves its temp root from a ladder — `DAD_E2E_TMPDIR`, then a private UID-scoped `/var/tmp/dad-e2e-<uid>` directory, then the OS temp dir as a last resort — and every previously bare `tempfile::tempdir()` call site across the e2e suite now goes through `common::harness_tempdir()`, which resolves that root before it allocates. Pointing `tempfile`'s process-global default at the root is kept only as defence in depth: it is installed at the end of the root's lazy initialiser, so an allocation that ran before anything else asked the harness for a directory — the first statement of a test body, under nextest's one-process-per-test — still went to the OS temp dir. A new `linkage-check` rule (check 8) fails the build on a bare `tempfile` constructor anywhere under `tests/`, plus `src/dispatch.rs` — scoped by **directory** rather than by an enumerated list, so a new file under `tests/` inherits it instead of silently falling outside it. It matches the directory, file and spooled constructors alike (`tempdir`, `TempDir::new`/`with_prefix`/`with_suffix`, `NamedTempFile::new`/`with_prefix`/`with_suffix`, `tempfile`, `spooled_tempfile`) and deliberately never the `…_in` forms, which name their parent. A refused private directory or a refused `DAD_E2E_TMPDIR` now fails the run loudly instead of silently falling back to the RAM-backed tmpfs, and the pre-flight free-space check now probes the base that was actually chosen. `cargo xtask clean-e2e-tmp` gained a `--root <path>` flag to reap a specific location.
  `DAD_E2E_TMPDIR` is now walked from `/` with descriptor-relative, no-follow opens (`openat`/`mkdirat`), so every directory is judged by `fstat` on the descriptor it was opened with rather than on an earlier look at its name, and a component that appears between the two is refused instead of adopted. Each component is validated **before** it is resolved: a symlink is inspected without being followed and is only resolved when it belongs to you or to root, so a link another local user planted at a predictable name in a sticky directory such as `/var/tmp` cannot redirect the harness — sticky semantics protect that planted entry from you, so resolving first and judging afterwards was not enough. Links owned by root are still followed, which is what makes the check work on macOS, where `/var` is a symlink to `/private/var`. The base itself must now be owned by you at exactly mode 0700 (`chmod 700`) — the same bar the default `/var/tmp/dad-e2e-<uid>` parent already had to meet, since this is where the harness seeds real agent credentials — and the refusal names a umask that clears owner bits as a legitimate cause.
  `cargo xtask clean-e2e-tmp` now verifies the boundary it deletes under instead of assuming it. The harness proves `/var/tmp/dad-e2e-<uid>` is private before it writes there, but that is a predictable name in a world-writable directory and the reaper is the half that removes things, so it now requires a real directory owned by you with no group or other bits, inside a `/var/tmp` that is itself root-owned and sticky; a symlink at that name is refused rather than followed. Scanning and deletion both run against the path that vetting resolved rather than the spelling they were handed, so a symlinked component cannot be retargeted between listing a directory and removing it. Naming that same parent by hand with `--root` gets the identical treatment: a `--root` whose resolved directory is the private parent keeps the ownership, mode and sticky-holder checks and stays out of `--include-untagged`'s reach, so one directory cannot have two security postures depending on how it was spelled.
  On Windows, where `std` offers neither ownership nor an `openat`, the same value now gets a by-name walk that creates each missing component with `create_dir` rather than handing the whole path to `create_dir_all`, so a symlink, junction or other reparse point planted at a missing component is refused instead of adopted and silently followed. That closes the redirection, not the whole race: the check is a second lookup of the name rather than a descriptor, a plain directory another local user planted is still adopted because there is no owner to compare, and created directories inherit their parent's ACLs. The ACL-and-handle version is tracked by #163/#164.
  The macOS shape is now pinned by tests rather than reasoned about. `/var` there is a symlink to `private/var`, so the harness and the reaper hold two spellings of one directory (`/var/tmp/dad-e2e-501` and `/private/var/tmp/dad-e2e-501`); both are accepted, every comparison that matters is by resolved path or by the final component's name, and both fit the socket budget with room. That had to be pinned on Linux, by building the same symlinked-ancestor shape in a scratch directory: `build-macos` runs `cargo nextest run` without `--workspace`, so the `xtask` crates' tests — the reaper's included — never execute on macOS at all (#470).
  This is developer tooling only — no change to the shipped binary.
  Fixed three e2e tests (`idle_worker_011`, `focus_007`, `card_stats_005`) that were consistently red on `main`. Triage found no production defect: two earlier commits changed presentation behaviour — gating the auto-focus chain behind the `experimental` flag, and switching the selected card border from plain to thick — and three tests were never updated to match. The fixes scope the experimental flag correctly, crop observation to the right pane column range, and make the card-layout parser border-weight-agnostic while still enforcing coherent borders. Review follow-up then addressed the class rather than the three instances: the border-weight glyph table and the pane-column/card-title grid predicates now live once in the shared test harness instead of being re-encoded per file, the pane-column wait reports "no anchor" and "needle absent" as distinct failures instead of one indistinguishable timeout, and both predicates gained fast-tier guards so CI catches the next presentation change on every push rather than only in the pre-PR tier. This is test-only; no production code changed.
- **The xtask crates' tests now run in a gate**
  `cargo test-fast` and `cargo test-e2e` select the whole workspace. The workspace root is itself a package and there is no `default-members`, so cargo's default target selection was the **root package alone** — every test in `xtask/linkage-check` and `xtask/docs` executed in no gate anywhere: not the per-task alias, not the pre-PR alias, and not CI, which runs the same commands. The tier goes from 1657 to 1705 selected tests.
  This is the test-side sibling of the `--workspace` flag added to clippy in [#436](https://github.com/vfarcic/dot-agent-deck/issues/436), and the reason it outlived that fix is that clippy's half already covered the compile-time half: those tests are *built*, so a type error or a lint in them still failed. Only their runtime assertions were inert, which matters most for `clean-e2e-tmp` — a deletion tool whose safety properties (owned-prefix matching, opt-in untagged reaping, never following a symlink out of the temp root) are runtime assertions and nothing else.
  The three CI test jobs — `build`, `build-windows` and `build-macos` — carry the flag too, so they keep matching the aliases, which apply on every platform. Widening costs nothing measurable: all 48 newly-gated tests are under 40ms and run in ~0.07s in aggregate, while the tier's wall clock is set by its slowest single test.
- **`/verify-pr` Detects Vacuous E2E Runs Again**
  The `/verify-pr` reviewer skill flags e2e runs where a real-agent test skipped at runtime instead of executing. Those tests print `SKIP: <reason>` and return normally, so nextest counts them as **passed** — a fully green e2e tier that proved nothing about the surface under review.
  That safeguard had been silently inert. The detector in `checks.sh` matched `SKIP:` anchored at column 0, but nextest indents captured test output by four spaces under `--success-output=final`, so the pattern never matched. Every review recorded `E2E_RUNTIME_SKIPS=0`, never wrote `e2e-skips.txt`, and never emitted the `e2e-real-coverage` **ATTENTION** row — including two reviews that each had four genuinely skipped real-agent tests to report.
  Both match sites now allow leading whitespace, and the indent is stripped when writing `e2e-skips.txt` so the file reads as plain `SKIP: <reason>` lines. Reviewers once again see which real-agent tests declined to run, and can rerun them with `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` to turn a skip into a hard failure.



## [0.35.10] - 2026-08-10

### Added

- **Install with Nix, home-manager or NixOS**
  Nix, home-manager and NixOS users can now install dot-agent-deck declaratively. Until now the only documented install path was the Homebrew tap, so anyone on Nix had to hand-wrap the prebuilt binary and pin a fresh `sha256` on every release, or skip the tool ([#231](https://github.com/vfarcic/dot-agent-deck/issues/231)). `nix run github:vfarcic/dot-agent-deck` now runs it with nothing installed at all; adding the flake as an input gets you `packages.default` for `environment.systemPackages` or `home.packages`; and there is an `overlays.default` for people who would rather reach it as `pkgs.dot-agent-deck`. There is also a `devShells.default` if you want the toolchain without the tool.
  The derivation builds from source against the committed `Cargo.lock`, so there is **no per-release hash** for anyone to recompute and commit. That was the core ask in the issue: a single stale `sha256` is what made the hand-rolled packaging painful enough to be worth filing.
  home-manager users get a module as well. `imports = [ inputs.dot-agent-deck.homeModules.default ];` gives you `programs.dot-agent-deck`, where `enable = true` installs the binary and the freeform `settings` and `keybindings` attribute sets are rendered to `~/.config/dot-agent-deck/config.toml` and `~/.config/dot-agent-deck/keybindings.toml`. Neither file is written unless you actually put something in it, so turning the module on does not overwrite a config you already have. It manages those two files and no others: `session.toml` and `remotes.toml` are written by the application itself and would break if home-manager made them read-only symlinks, and `schedules.toml` is left for a follow-up because it is the one file whose location follows `$XDG_CONFIG_HOME`. Hooks stay a one-off `dot-agent-deck hooks install`, since that command edits other tools' configuration files and a generation switch cannot roll it back. See [Installation](https://agent-deck.devopstoolkit.ai/docs/installation) for a worked example.
  The flake pins the released version and passes it to the build as `DAD_VERSION`, so `--version` reports the release you actually installed rather than the `0.1.0` placeholder that `Cargo.toml` carries permanently. A Nix source build gets a tarball with no `.git`, so the `git describe` step in `build.rs` is unavailable and the fallback would otherwise land on the placeholder; this uses the injection seam that [#250](https://github.com/vfarcic/dot-agent-deck/issues/250) added for exactly this kind of packager. Because that pin is a hand-maintained line, a guard step in the release workflow fails the release if the flake's version and the tag being released ever disagree, and a `nix flake check` job in CI builds the derivation on every run, so a nixpkgs bump that breaks it is caught there rather than by the first user who types `nix run`.
  Two limits worth stating plainly. This **complements devbox, it does not replace it**: devbox stays the development environment, the flake is the consumer install. And the flake does not run the test suites (`doCheck = false`), because the fast tier needs `cargo-nextest` and the e2e tier needs live agent CLIs and network access, neither of which exists in a Nix sandbox; correctness stays gated by the existing CI matrix, and the flake's job is to prove the tree builds reproducibly from source.
  Cross-version contract (CLAUDE.md rule 12): no impact. The change is packaging, CI and docs only, and touches no daemon, protocol, orchestration or hook code, so there is no `PROTOCOL_VERSION` bump.
  **New: `dot-agent-deck daemon status [--json]`.** A read-only snapshot of every agent the local daemon currently manages — pane id, label, cwd, orchestration role, live status, and active tool — without attaching to any pane. `--json` emits a versioned document (`{"schema_version": 1, "agents": [...]}`) for scripts. It never prints prompt text, never starts a daemon that isn't already running, and reports a non-zero exit if the daemon can't be reached.
  **New: `dot-agent-deck worktree list [--json]` and `worktree reclaim [--yes]`.** `list` reports every linked git worktree with its resolved PR state, cleanliness, ownership, and gate verdict (`remove`/`ask`/`keep`) with a reason; `--json` emits a versioned document (`{"schema_version": 1, "worktrees": [...]}`). `reclaim` removes a worktree only when all three hold: its PR's state is `MERGED` (checked via `gh`, never git ancestry — squash-merges never enter `main`'s ancestry, so an ancestry check misses genuinely merged branches, and an ancestor branch with no PR at all must never be removed), the tree is clean, and the deck can prove it created the worktree. A worktree it cannot prove it created is reported as reclaimable-pending-confirmation, naming the exact path and the ready-to-copy `--yes` command, rather than removed silently. The branch always survives; only the worktree directory is removed. An unresolvable PR state (missing `gh`, a spawn or parse error, an ambiguous match) always keeps rather than guessing.



## [0.35.9] - 2026-08-09

### Added

- **An agent can now dispatch a task into its own worktree**
  Added the `dispatch` CLI verb and a new **dispatcher mode**. An agent can now run `dot-agent-deck dispatch <name> --task "..."` and the daemon creates a dedicated git worktree as a sibling of the repo (`../<repo>-dispatch-<name>`) and starts an isolated line of work inside it — one step, no manual worktree chore. Dispatcher mode is a built-in seeded mode that teaches an agent this one extra verb: a dispatcher pane is an ordinary conversational agent that reaches for `dispatch` when you say to start something as a separate line of work.
  You choose whether each unit starts as one agent or as a full multi-role orchestration — `--single` or `--orchestration [<name>]`, with `dot-agent-deck dispatch --list-targets` printing what the repo offers. The dispatcher asks you before its first dispatch rather than inferring it, because the same words ("work on these three features" vs "verify these three PRs") want opposite shapes. Naming an orchestration the repo does not define is an error rather than a silent fallback to something you did not pick.
  Either shape starts the unit the way opening it yourself with `Ctrl+n` would, plus your prompt. `--single` runs a real agent (the configured `default_command`, else the Claude default). `--orchestration` starts every role and hands the orchestrator its usual context — its own prompt template, the available agents, and the delegation protocol — with your task folded in, so it actually delegates instead of working alone while its team sits idle. (Scheduled issue-dispatch has the same gap; the shared composition makes fixing it cheap, but that is tracked separately on #222 so a shipped feature's behaviour does not change as a side effect.)
  Each role of a dispatched orchestration is labelled on its card with its **role name** from your `.dot-agent-deck.toml` — `orchestrator`, `coder`, `reviewer` — the same as an orchestration you open yourself with `Ctrl+n`. Dispatched role cards previously showed the agent's own session id, so a six-role team came up as six cards you could not tell apart. (A scheduled issue-dispatch's role cards were labelled with the schedule name, which had the same effect: every card in the tab read alike.) The tab still carries the identity of the dispatch itself, so the two questions — which unit is this, and which role — are answered in different places instead of competing for one label.
  Dispatcher mode is available to everyone — no feature flag to enable. It is documented at [Dispatcher Mode](https://agent-deck.devopstoolkit.ai/dispatcher-mode). Completions do not yet route back to the dispatching pane — that return edge is tracked separately.
  Closing a dispatched agent's card now works on the first Ctrl+W. It used to take two: the card came back, its agent still running, and only a second close removed it. Two things caused that. A card the daemon spawned — a dispatch, or a scheduled fire — has no pane attached in your deck until you focus it, so the close reported "pane not found" and the deck deliberately kept the card so you could retry; it now stops that agent by asking the daemon directly. And the daemon waited for the dispatched worktree's cleanup before answering the close, which on a worktree an agent has been working in means a `git status` walk of seconds — long enough for the deck to give up and keep the card. Cleanup now runs after the answer, since the agent is already stopped by then.
  One more close-path defect went with it, and it is the one that survived the first fix: a pane can carry more than one session, and closing removed only the one its card was built from. The leftover rendered as a ghost card badged "No agent", pointing at the closed agent's directory. It showed up specifically when the pane's command is one the deck cannot recognise as an agent — a `devbox run agent-coder` style launcher — because such a command is not wrapped, so the agent's own session never merges with the pane's placeholder. Closing a pane now drops every session belonging to it.
- **Orchestration tab labels are colored by the most urgent pane inside them**
  With several orchestration tabs open, there was no way to tell which one needed you without switching to each in turn. Every pane already carries a live status — Working, Thinking, Needs Input, Error, Idle — and the orchestration sidebar shows it per role, but that signal stopped at the tab you happened to be looking at. The more orchestrations you run in parallel, the more the answer to "what needs me right now" was hidden one keypress away in every direction.
  A background orchestration tab's label in the tab bar is now colored by the single most urgent status among its panes, using the same status palette the sidebar and deck cards already use — no new colors. Priority runs Error (red) > Needs Input (yellow) > Working (green) > Thinking (blue), so a tab with one errored role reads red no matter what its other roles are doing. A tab with a role waiting on your input turns yellow while you are somewhere else entirely.
  Color means "something in here needs you", so a tab with nothing going on does not compete for your eye: an all-idle tab renders in the ordinary tab color rather than a gray, and the tab you are currently on keeps its usual active highlight untinted — you are already looking at it. Keeping status color off those two cases also keeps every label at full contrast on light and dark terminals alike.
  Only orchestration tabs are colored. The Dashboard tab and single-agent Mode tabs already show their own status directly and are unchanged, as is the way any individual pane, sidebar entry, or deck card renders its own status once you are inside a tab.
- **Typing Into a Worker Pane Can Now Be a Decision Rather Than a Reflex (Experimental)**
  **This ships behind the `experimental` feature flag and is off by default.** Nothing about how you work today changes unless you turn it on: set `experimental = true` under a `[features]` table in your `.dot-agent-deck.toml`, or launch with `DOT_AGENT_DECK_EXPERIMENTAL=1`. It is gated precisely because it changes a reflex you already have — the default is deliberately restrictive, and that deserves real use before it reaches everyone. Everything below describes what you get once it is on.
  An orchestration is one workflow with a single coordinator, but every worker pane sits there with a cursor in it, inviting you to answer its question on the spot. Doing that puts a second, uncoordinated actor inside the workflow: you change state the orchestrator believes it owns, with no path for it to learn that you did. The deck does not look broken afterwards — it looks fine, and the model behind it has quietly diverged. Most often it is not even deliberate: you open a worker pane to check on it, get distracted, and type your next instruction into the pane in front of you instead of the one you meant.
  On an **orchestration tab**, keystrokes aimed at a worker role are now dropped rather than delivered, and the bottom bar says `Pane locked — Ctrl+d then Ctrl+e to unlock`. The orchestrator's own pane is never locked. Dashboard and mode tabs are not affected at all, nothing becomes read-only, and every pane still shows live output and scrolls as before. When you genuinely want to reach into a worker — a parked agent, a model that never called `work-done`, an agent waiting somewhere unexpected — it costs one deliberate `Ctrl+D`, `Ctrl+E`. That pause is the feature; a lock you have to remember to engage protects nothing, so it is on by default.
  **A worker that has stopped and asked you something is never locked.** While a role pane reports `WaitingForInput`, every key reaches it with no unlock at all, and the lock re-engages the moment that status clears — answering a question the agent itself asked is a response to a request, not an intrusion into one. An agent that never reports `WaitingForInput` gets no exemption and still needs the deliberate unlock.
  **Focus follows the lock.** While locked, the deck steers focus onto a role pane the moment it starts waiting on you (lowest-numbered first when several wait at once, advancing as each is dealt with) and back to the orchestrator once nothing is waiting. While unlocked, no automatic focus move happens at all — focus stays exactly where you put it until you lock again. Automatic focus moves also wait for you: if a keystroke of yours is still queued when the deck decides to move focus, the move is deferred to a later frame rather than applied, so what you typed always reaches the pane you typed it at — and never answers a prompt in a pane that jumped in front of you mid-sentence.
  `Ctrl+E` is claimed in command mode only, exactly like `Ctrl+W`. While you are typing in a pane the deck does not take it, so `0x05` reaches the program and readline's `end-of-line` works as usual. The lock is one setting for the whole deck rather than one per tab — unlocking on any orchestration tab unlocks all of them, and a newly opened one adopts the current value. It is not persisted: every deck starts locked.

### Fixed

- **A pane no longer sprouts a second card when a hook reports without an agent id**
  A pane could end up with two cards on the deck, showing two different statuses for the one agent. Which of the two the deck believed was decided by hash-map iteration order, so the same pane could read `Needs Input` on one run and `No agent` on the next with nothing having changed. An orchestration fixture with two roles rendering three cards — two of them labelled `worker` — is what surfaced it.
  The trigger is a hook event that names no agent id. That is not a malformed event: it is the shape agent-deck deliberately still accepts from pre-F9 hook scripts, and the shape any producer sends when `DOT_AGENT_DECK_AGENT_ID` did not reach it — a hand-written hook, a wrapper that scrubbed its environment, or `dot-agent-deck agent-event` invoked from a subprocess that lost the variable. Such an event matched neither the path that routes a report onto the pane's existing card nor the path that replaces a card outright, so it fell through and created a second one.
  An untagged report now lands on the card the pane already has, instead of starting a rival. The protection that pathway existed for is untouched — a legacy hook still cannot wipe the accumulated tool count, prompt history, or start time of the card it reports on, and it cannot blank the pane's agent identity either. What goes away is only the duplicate card, which was never load-bearing.
  Two things that read per-pane status get steadier as a result: orchestration tab labels colour from the pane's real status, and the command-entry lock's focus steering stops being able to chase a stale one. The sharpest symptom is gone too — a worker pane could display `Needs Input` while your keystrokes were dropped with a message insisting the pane was locked, because the card and the lock were reading different sessions for the same pane.
  One deliberate exception: the command-entry lock's `Needs Input` exemption is **not** granted on the strength of a report that names no agent id. That exemption decides whether your keystrokes reach a worker the lock is otherwise protecting, so it acts only on a status whose origin is established; a report that cannot say which agent generation it came from is displayed on the card as usual but does not open the lock. Until now an untagged report could not reach a pane's real card at all, so this restriction was already in force as a side effect — it is simply explicit now rather than incidental. A deck running entirely on pre-F9 hooks therefore reaches its worker panes with `Ctrl+d`, `Ctrl+e` rather than automatically, which is the same trade the lock already makes for any agent that never reports `Needs Input`.
- **The selected deck card is now visible whatever its agent is doing**
  Finding the card you were pointed at was harder than it should have been, in two different ways. While you were typing into a pane, the selected card was drawn in a dimmed magenta — and on a dark terminal that lands in the same band as the dark gray an idle agent gets, so the card you were driving became one of the hardest on screen to find. It was not only the outline: because a border's style bleeds into the titles drawn over it, the card's name and its `Last:` / `Tools:` counters faded along with it. Separately, an idle card's border is deliberately faint so that a resting agent recedes — but that meant selecting an idle card barely changed anything, because a border you can hardly see is no easier to see for being thicker.
  A selected card's border is now drawn in **your terminal's own foreground colour** — near-white on a dark theme, near-black on a light one, decided by your terminal rather than by us — and it is **thickened** at the same time, alongside the `▸ ` marker it already carried. Three cues at once, none of which depends on what the agent happens to be doing. Nothing is ever dimmed to indicate selection.
  Cards that are not selected are unchanged: each still carries its status colour, so a working agent reads green, one waiting on you reads yellow, one that errored reads red, and an idle one still recedes quietly into the background exactly as before.
  Which mode you are in still reads off the same card. In command mode, where the keyboard drives the deck, the selected card's border is bold; while you are typing into a pane it returns to normal weight, keeping its colour, its thick border and its marker. So "which card is selected" and "where are my keystrokes going" remain separate questions with separate answers, and neither is answered by making something harder to see.
  The selected card's border no longer reports its agent's status, since it is showing selection instead. The status badge in its top-right corner still does, at full colour.



## [0.35.8] - 2026-08-08

### Added

- **Toggle the Orchestration Sidebar/Pane-Column Split Ratio**
  In an orchestration tab, the role sidebar and the agent pane column split at a fixed 34/66 ratio — on a laptop screen that leaves the working pane noticeably narrower than it could be, and the only way to reclaim that width was a config edit and restart.
  `Ctrl+l` in command mode now toggles that split between the default 34/66 and a narrower-sidebar 25/75. Press it again to return to the default. The setting is a preference, not a per-tab property: one press applies to every orchestration tab, and an orchestration tab you open afterwards comes up at the split you chose instead of resetting to 34/66. Dashboard and mode tabs are untouched, and the state resets to 34/66 on the next launch rather than persisting.
  Like `Ctrl+w`, it is **command mode only**: while you are typing in a pane, and on every tab that is not an orchestration tab, `Ctrl+l` is left alone as ordinary input for whatever is running there — so an agent or shell still gets its clear-screen. Press `Ctrl+d` first to reach command mode, then `Ctrl+l`.
  Like every other keybinding, `toggle_orchestration_split` is remappable through `[global]` in `~/.config/dot-agent-deck/keybindings.toml`. See [Keyboard Shortcuts](https://agent-deck.devopstoolkit.ai/docs/keyboard-shortcuts) for the full list.

### Fixed

- **A Stopped Daemon Now Shuts Down Cleanly — and Says So**
  `dot-agent-deck daemon stop` and `daemon restart` terminate the daemon with `SIGTERM`, but the daemon installed no signal handler, so the signal hit the default disposition: the process died instantly. Your managed agents were killed by their terminals hanging up rather than by an orderly teardown, and — the part that hurt most — **nothing was written to the log**. A daemon that disappeared mid-session left no record of whether it had been stopped, had crashed, or had been killed by the kernel under memory pressure, so the one question worth asking after losing a session's panes was the one question the log could not answer.
  The daemon now handles `SIGTERM` and `SIGINT` (and Ctrl-C on Windows) by running the same shutdown it performs for an explicit stop request: your agents get the full termination grace period to flush their state, sockets are unlinked, and a warning line naming the signal goes to the log before it exits. Stopping the daemon still stops every agent under it — that is unchanged and by design — but it is now a clean stop you can find in the log afterwards.
  Sending a second `SIGTERM` while that shutdown is in progress exits immediately without waiting for teardown, so a wedged daemon is still killable with `pkill` — installing a handler means the signal is no longer fatal by default, and that escape hatch would otherwise have been lost.
  Two smaller diagnosis fixes ride along. When a pane gives up trying to reattach, the log now distinguishes **`daemon-unreachable`** (every lookup failed — the daemon went away and your agents may well be fine) from **`no-live-agent`** (the daemon answered and has no agent for that pane), and reports how many lookups were attempted and how many failed. Previously both reported the same thing, so a single daemon disappearing under several panes looked exactly like several agents dying independently. The pane's own on-screen message is unchanged.
  Finally, `dot-agent-deck wrap` now honours the same orphan and maximum-lifetime safety nets `daemon serve` has always had. Only the daemon read them before, so a wrapper whose parent test was killed could survive indefinitely; wrappers were found still running three days later, from a checkout that had since been deleted, one of them spinning a shell loop a hundred times a second. A wrapper that outlives its parent now terminates itself and takes its child with it, escalating to `SIGKILL` if the child ignores the polite signal.



## [0.35.7] - 2026-08-05

### Added

- **Devin CLI Is Now a First-Class Agent**
  Devin joins Claude, Codex, OpenCode and the rest as an agent the deck can spawn, track and orchestrate. Devin ships a Claude-Code-compatible hooks engine, so its native command hooks post the same stdin JSON shape Claude does and ride the existing hook socket — no new wire format, and nothing about the TUI↔daemon protocol changes. Devin's badge colour is LightBlue.
  The deck installs its hooks into Devin's user config — `$XDG_CONFIG_HOME/devin/config.json` when that variable is set, otherwise `~/.config/devin/config.json` — and auto-installs at daemon startup whenever `devin` is on `PATH`. The install merges into the existing file rather than clobbering it, never widens the file's permissions, and refuses to guess at content it cannot parse: malformed or JSONC config is backed up and the install errors out instead of silently destroying your model, permissions or MCP servers. `dot-agent-deck hooks install --agent devin` and `dot-agent-deck hooks uninstall --agent devin` are the explicit CLI.



## [0.35.6] - 2026-08-04

### Added

- **The stacked pane layout no longer wastes a row per non-focused pane**
  In the default `Stacked` pane layout, every non-focused pane used to render as an empty 1-row title-bar frame — a border drawn around nothing. In a 7-role orchestration tab ([#307](https://github.com/vfarcic/dot-agent-deck/issues/307)), that was six rows spent on frames for `developer`, `tester`, `reviewer`, `releaser`, `researcher` and `documenter`, none of which showed anything: on a laptop screen, roughly 13% of the vertical space. The orchestration sidebar already shows each role's live status, tool counts and click-to-focus, so the collapsed frame was a strictly poorer second copy of information already on screen.
  Non-focused panes are no longer drawn at all. The focused pane now reclaims every row the collapsed frames used to occupy. Nothing about agent lifecycle changes — every pane's PTY stays open, every agent keeps running, hooks keep arriving, and the sidebar (or dashboard cards) keeps showing live status for all of them. A non-focused pane's PTY is now sized as though it were the focused slot, so switching focus is instant with no resize thrash or reflow.
  Mode tabs, which render their two side panes simultaneously by design, are unaffected — they already use a fixed tiled split regardless of the global pane-layout setting.

### Fixed

  Fixed watch-wrapped mode panes staying permanently blank for commands that do not exit. Persistent panes (`[[modes.panes]]`) default to `watch = true`, which runs the command through the built-in `dot-agent-deck watch`; that wrapper showed the command's output only once the command finished. A pane running `tail -f app.log`, `kubectl logs -f`, or a dev server therefore showed nothing at all — no output, no error, no hint why.
  Watched commands now show their output as it is produced, so long-running and slow commands paint progressively. stdout and stderr appear interleaved in the order they arrive, rather than all of stdout followed by all of stderr.

### Miscellaneous

  Fixed the e2e test harness leaking temp directories, which could make a healthy branch fail dozens of unrelated-looking tests. Every temp dir the harness creates now nests under one per-process root that is removed when the process exits, and `cargo xtask clean-e2e-tmp` reaps whatever an interrupted run left behind. When the temp filesystem is genuinely too full, the suite now says so explicitly instead of surfacing the exhaustion as agent and daemon failures. This is developer tooling only — no change to the shipped binary.
  Fixed the local Windows pre-PR type-check (`scripts/windows-cross-check.sh`), which had been failing on every branch — including a clean `main` — since the reqwest 0.13 upgrade moved rustls onto its `aws-lc-rs` provider. It died inside `aws-lc-sys`'s build script, compiling that crate's ~600 C files with Linux gcc and Linux system headers against a Windows target, so it never reached a single line of this repo's own code and the documented gate produced no signal at all. The check now shims the C compiler the same way it already shimmed the archiver — `cargo check` never links, so nothing reads either artefact — which skips the C build entirely and leaves the Rust type-check real. A cold run takes about fifteen seconds and a warm one about five.
  CI now runs that script too, in a parallel `windows-cross-check` job, so the same silent rot cannot recur — nothing had ever exercised it, which is why a permanently-red documented gate went unnoticed for months. That job is not a second Windows code gate; `build-windows` still owns Windows, compiling natively with clippy and tests on a real Windows runner, and was unaffected by this bug throughout. This is developer tooling only, with no change to the shipped binary.



## [0.35.5] - 2026-08-03

### Added

- **The mode you are in is now unmistakable**
  `Ctrl+D` toggles between driving the deck and typing into a pane, and until now you had to infer which side of that toggle you were on. The pane border went some of the way, but the focused pane still showed a cursor in command mode — the loudest "type here" signal a terminal has, firing in the mode where typing does nothing — and the only words on screen read `[Command Mode Ctrl+D]` precisely when you were *not* in command mode. Acting on the wrong belief was easy, and the usual way to find out was a stray keystroke landing somewhere you did not mean it to.
  Four things changed. The focused pane now shows a cursor **only** while your keystrokes reach it — neither the highlighted block nor your terminal's own blinking cursor renders in command mode. A chip at the left of the bottom bar names the mode you are in right now, ` COMMAND ` or ` TYPING `, in the same place on every tab; the button beside it still names where `Ctrl+D` would take you, so one tells you where you are and the other where you can go. Entering command mode dims the focused pane and overlays a large `COMMAND MODE · Ctrl+D to type` banner, which clears itself after 2.5 seconds or the moment you press a command-mode key — but stays up, or comes back, when you type a key that is bound to nothing, since that is exactly the moment you probably thought you were talking to the agent. And on the dashboard, the selected card's highlight is de-emphasised while you type into a pane (it keeps its `▸ ` marker), so the deck looks inert exactly when the pane looks live.
  Command mode is also a genuine read-only inspect mode now. The pane dims but is never blanked — you can still see which agent is mid-work and which one you are about to close — and the scroll wheel over the focused agent pane scrolls it in command mode, matching side panes, which have always scrolled in any mode. Reviewing what an agent did no longer means dropping into the mode where a mistyped key goes into it. In command mode the wheel always drives Agent Deck's own scrollback and is never forwarded to the agent's mouse protocol, so a full-screen TUI in the pane cannot move under you while you read.
  `PageUp` and `PageDown` do the same from the keyboard. They are the new `scroll_pane_up` and `scroll_pane_down` actions in the `[dashboard]` section of `keybindings.toml`, remappable like every other binding, and they apply in command mode only — while you are typing in a pane those keys still go through to whatever is running there, so pagers and editors keep them.
  Everything here is on by default and none of it is configurable beyond the two new keybindings; the indicators use terminal-relative highlighting rather than fixed colours, so they read correctly on light and dark backgrounds alike.
  See [Keyboard Shortcuts](https://agent-deck.devopstoolkit.ai/docs/keyboard-shortcuts) for the full mode reference and the new bindings, and [Workspace Modes](https://agent-deck.devopstoolkit.ai/docs/workspace-modes) for how command mode reads as a resting state on a mode tab.
  Demo reel: https://youtu.be/lwir8zdUM0E

### Fixed

- **Session Cards Keep a Consistent Height as They Narrow**
  Session cards no longer restructure themselves when you resize the terminal. Previously, narrowing a card past a width threshold moved the `Last`/`Tools` counters onto their own row, making the card one row taller — which could push it down a density tier and show fewer prompts and tool lines exactly when you were trying to reclaim space (the blocker behind [#336](https://github.com/vfarcic/dot-agent-deck/issues/336), the orchestration sidebar ratio toggle).
  The `Last`/`Tools` counters now live in the card's bottom-right border, the same way the status badge already occupies the top border, so they cost no content rows at any width. Card height now depends only on density (Compact, Normal, Spacious), not on card width. The `Dir:` line also spans the card's full inner width and ellipsizes properly when a directory path is too long to fit, instead of being clipped without an ellipsis.
  See [Session Management](https://agent-deck.devopstoolkit.ai/session-management) for a look at the updated card layout, or watch the [demo reel](https://youtu.be/W73TozxLd8A) to see a live card keep its height while the terminal narrows around it.



## [0.35.4] - 2026-08-02

### Fixed

- **Scheduled Tasks Dialog Now Captures the Mouse Wheel**
  The Scheduled Tasks manager dialog no longer leaks mouse-wheel scrolling to the pane behind it. Previously, scrolling the wheel while the dialog was open scrolled the mode-tab pane underneath instead of the dialog itself, forcing users to scroll blind or fall back to the keyboard.
  The wheel now scrolls the task list in the dialog directly, matching the existing j/k keyboard navigation.
- **Source builds now report their real version instead of a silent 0.1.0**
  A binary built from source outside a tagged git checkout reported its version as `0.1.0` and its build id as `0.1.0-unknown` — indistinguishable from the placeholder `Cargo.toml` carries for exactly this case. `build.rs` derived both values from git (`describe`/`rev-parse`) and degraded to that placeholder whenever git metadata was missing — a source tarball, a shallow or `.git`-less clone, `cargo install --git`, or a sandboxed package build (Nix, distro packaging) — and there was no way for a packager or CI job to override it. Every version-aware path then misbehaved against the fake `0.1.0`: compatibility-based version negotiation, the newer-only remote upgrade nudge (a source build looked permanently ancient), the `remote add`/`--no-install` pre-flight, and the `DAD_BUILD_ID` daemon-restart handshake, where a constant `-unknown` across genuine rebuilds could suppress a restart an in-place upgrade was supposed to force.
  `DAD_VERSION`/`DAD_BUILD_ID` now resolve in order: a pre-set value in the build environment, then git, then `CARGO_PKG_VERSION` — unchanged behavior for release builds and ordinary dev checkouts, but packagers can now inject the real version directly. An injected version is validated with the same semver parser `src/version.rs` uses for everything else, so a malformed injection falls back to the placeholder rather than being accepted or panicking the build, and every `cargo:` directive `build.rs` emits is now guarded against line-protocol injection.
  This PR also closes a pre-existing, unrelated shell command injection in `.github/workflows/release.yml`, found during the security audit of this change because the file was already being edited. Several steps interpolated `github.event.inputs.version` and tag-derived values directly into `run:` scripts; git tag names may legally contain quotes, semicolons, and command substitution, and the affected steps carry `HOMEBREW_TAP_TOKEN`/`SCOOP_BUCKET_TOKEN` with `contents`/`packages` write access. All 13 interpolations across 8 steps are now rebound through `env:` blocks, and the prepared version is validated as SemVer before it reaches `GITHUB_OUTPUT`. The tag-triggered release path is unchanged — identical task/cargo/cross invocations and git command sequence.
  The Linux ARM64 artifact is cross-compiled inside a container, so the injected version has to cross that boundary to reach the build script. It now travels two independent channels — cross's own passthrough allowlist and an explicit `docker run -e` — and, because both of those live on the far side of the boundary they protect, the release job additionally verifies on the host that the finished ARM64 binary really does carry the version being released, failing the release rather than publishing a binary that reports a different version than the tag it ships under.
- **A respawned worker no longer leaves a duplicate, unreachable card on its pane**
  Delegating to a role configured with `clear = true` respawns the worker, and on Pi workers the pane was left showing **two** cards instead of one — the dead generation's card stacked on top of the live one. Both claimed the same pane, so the deck gave no clue which was which.
  Respawning a third, fourth or fifth time did not add further cards, but it did something quieter: the surviving card silently kept the *previous* generation's identity and event history, so the deck was describing a process that had already been killed while the new one worked unattributed.
  The second card was not just cosmetic clutter: it was unreachable. The orchestration deck derives its highlight from the pane id and resolves it to the first card matching that pane, so the highlight stayed pinned to the stale card and the live one could not be selected at all. The practical effect was a worker that was genuinely running and doing the delegated work while the only card you could reach was the corpse of the previous generation.
  A pane now ends up with a single card after a respawn: the stale generation is retired as soon as the replacement's first frame arrives, whichever kind of frame that is — Pi never announces a new session the way Claude Code and OpenCode do, which is why its respawns were the ones that duplicated. Every later respawn hands that one card over to the generation actually running, with a fresh history rather than the dead one's. A card also no longer disappears from a live pane when a late farewell frame arrives from the agent that just left. Scheduled tasks are unaffected in the other direction: a scheduled card is still replaced cleanly by the agent's real card when the agent starts, and still keeps the schedule's friendly name rather than reverting to a session id.
- **Generated delegation guidance now defaults to `--task-file`**
  The delegation protocol dot-agent-deck generates for an orchestrator showed only `dot-agent-deck delegate --to <role> --task "…"` and never mentioned `--task-file`. The orchestrator runs that command in its own shell, so the task text was rewritten before dot-agent-deck received it — backticks and `$(…)` executed and replaced by their output, usually empty — and pieces of an instruction disappeared while the delegation still reported success. Worker `work-done` summaries had the same exposure.
  The generated protocol, the `## When done` footer in every worker task file, and the prompt that generates new configs now lead with `--task-file` and reserve the inline form for a single line of plain text with no backticks, `$`, `"`, `\` or `!`. They also say how to produce the file: write it with a file-writing tool — never with shell redirection or a heredoc, because a line of the task text can terminate the heredoc and Bash then executes everything after it — name it from a fresh `[a-z0-9-]` slug, single-quote the path, keep secrets out of it, and delete it after the handoff. The footer's suggested path is role-specific, length-bounded, and outside the `work-done-*` names the deck writes itself, so a report cannot be overwritten by its own completion signal or by another role's. Preferring the file is not a hard dependency on it: having a file-writing tool is not the same as being allowed to use one, and a role launched with a restricted allowlist (`claude --allowedTools Bash Read`, say) would otherwise stop at an interactive approval prompt and never signal at all — so all three branches are now stated outright, in the footer above the inline example where a worker actually reads them. With no authorized file-writing tool, send a single plain line inline with `--task`; if the content fits neither form, say so plainly. The fallback is never shell redirection or a heredoc. This corrects advice rather than adding enforcement: `--task-file` has shipped on both commands (including `-` for stdin) for several releases, and it differs from the older suggestion to reference a long-context file inside `--task`, which shortens the description but still sends it through the shell. Bounded reads, coordination-file permissions, and a daemon-side clobber guard remain open in [#328](https://github.com/vfarcic/dot-agent-deck/issues/328), [#329](https://github.com/vfarcic/dot-agent-deck/issues/329), and [#331](https://github.com/vfarcic/dot-agent-deck/issues/331).
  Demo reel for PRD #303: https://youtu.be/7ZTG6zi4nCc. Watch before merging.



## [0.35.3] - 2026-07-31

### Fixed

- **Command mode is now visible on the pane border**
  There was no reliable way to tell whether you were in command mode. The focused pane rendered its cyan focus border identically whether or not it was accepting keystrokes, so the loudest signal on screen said "type here" while the keyboard was actually driving the deck. The only real cues lived in the bottom bar — the first button flipping between `[Command Mode Ctrl+D]` and `[Back to Pane Ctrl+D]`, and `[Close Ctrl+W]` going dim — which meant identifying your own mode required reading and diffing button labels in the corner of the screen. On a mode tab with a full-screen pane, pressing `Ctrl+D` changed exactly one row of the display.
  The focus accent is now gated on actually being in PaneInput: cyan appears only while your keystrokes reach the pane. In command mode the focused pane's border falls through to its agent's status color — green working, blue thinking, yellow waiting, red error, gray idle — the same role the deck card uses for that state. Pressing `Ctrl+D` now visibly changes the pane you are looking at, and command mode gains a side benefit: while you navigate, every border reports what its agent is doing instead of all of them reading "focused".
  Focus itself moves to border **weight** so nothing is lost. The focused pane draws a thick border (`┃`) in command mode, where color can no longer carry that fact, which matters because `j`/`k` cycles pane focus on a mode tab and focus decides where `Enter` / `Ctrl+D` return you. Color now answers "are my keystrokes landing here?" and thickness answers "which pane is focused?" — one channel each instead of both competing for the same one.
  Nothing changes size. Border weight does not affect a pane's inner area, so PTY dimensions, layout, and reflow are untouched; only the glyphs and the color change. Thickness was chosen over a fourth accent color because the 16-color-safe palette is already full (green/blue/yellow/red are statuses, cyan is focus, magenta is selection) and the remaining candidates are grays — the exact low-contrast trap on light terminal backgrounds that the terminal-relative color model exists to avoid. One consequence worth knowing: a focused pane with no backing agent session — a plain shell pane — has no status to fall back to, so in command mode it renders the same dimmed border unfocused status-less panes already use, with the thick weight as its focus marker.
- **Starting the deck no longer fails on machines with a slow interactive shell**
  On a machine whose interactive login shell takes more than five seconds to start, the very first launch of the deck always failed with `daemon failed to start within 5000ms: endpoint /tmp/dot-agent-deck-attach-<uid>.sock never became available`, and then succeeded on the next try. The retry-succeeds behaviour made it read as flaky infrastructure, but it was deterministic: two startup budgets in different modules disagreed with each other, and the shell's speed decided which one won.
  The daemon does real work before it binds the endpoint the launcher is waiting for. At `daemon serve` startup it captures the user's login-shell PATH — running `$SHELL -ilc` so that installers which append their `PATH` line to `~/.bashrc` after the standard non-interactive guard are still seen — and that capture is allowed up to ten seconds, followed by materializing the pi orchestrator extension and the Codex hooks. Only then does it call `bind`. The lazy-spawn launcher, meanwhile, polled for the endpoint for a hardcoded five seconds: half the time the daemon was explicitly permitted to spend before binding. Any interactive shell slower than that deadline produced the failure, and a zsh with `compinit`, a plugin manager, and generated completions for a few CLIs measures around six seconds — not a hung or misconfigured shell, just an ordinary heavy one. The daemon then bound normally a moment after the launcher had already given up, which is why it was up and working by the time you ran the command again.
  The launcher's budget is now derived from the daemon's own worst-case pre-bind budget instead of being chosen independently, so the two cannot drift apart again, and a unit test asserts the ordering directly rather than trying to reproduce a slow shell. The healthy path is unchanged and still returns as soon as the endpoint appears, typically in milliseconds — the longer bound is only ever spent when a daemon is genuinely missing or dead, where the failure message now takes about fifteen seconds to arrive rather than five.
  The timeout message also stopped pointing you at an empty file. It advised checking `daemon.log` for daemon stderr, but that log only receives output when the daemon was started with `DOT_AGENT_DECK_LOG` set, so on a default install it is zero bytes — which read as evidence that the daemon had never started at all, the opposite of what was happening. The message now says so explicitly.
- **The dashboard stats bar no longer loses its `tools` total to a per-agent-type breakdown**
  On a deck running more than one kind of agent, the stats bar appended a per-agent-type count breakdown (`14 ClaudeCode │ 8 Codex`) after the status totals. That bar is drawn into the last row of the *left dashboard column* — not the full terminal width — and as an unwrapped single-line paragraph it clips silently at the right edge, with no ellipsis to show anything was dropped. Each type segment costs roughly 12-18 columns, so on a real multi-agent deck the breakdown pushed the segments after it off-screen: the `tools` total, which the renderer treats as always-present, and the `mode:` indicator. The reported symptom was a bar that appeared to simply end at `… │ 19 idle │ 14 ClaudeCode`, which also made it look as though only one agent type was active when several were.
  The breakdown is now removed rather than made narrower. The information it carried was already on screen and better placed: every deck card renders its agent type as a registry-colored badge (`Claude · name`, `Codex · name`) directly below the bar, so the aggregate restated in one scarce row what the cards show per session. Removing it also stops the cost from growing with each agent type added to the registry. The status totals, the `tools` total, and the `mode:` indicator are unchanged.
  Bars narrow enough to clip even the status counts still truncate at the right edge without an ellipsis; giving the bar a defined truncation priority is a separate change.



## [0.35.2] - 2026-07-30

### Fixed

- **Delegated tasks no longer get lost when a worker respawns**
  Delegating to a role configured with `clear = true` could leave the worker sitting idle forever with no sign that anything had been sent. The respawned agent's `SessionStart` hook fires early in its boot sequence — before its TUI is ready to accept input — and the daemon wrote the task pointer the instant that signal arrived, so the write raced the agent's startup. Land late enough and it worked; land mid-boot and the task text arrived but the Enter was swallowed, leaving it unsubmitted in the input box; land early and the whole prompt was dropped and the worker never started. This is why the reported symptoms looked like three different bugs, why the failure rate varied by machine speed and load, and why setting `clear = false` worked around it.
  Delegation now waits for a readiness buffer after the respawned worker signals `SessionStart` — and also after the fallback wait expires, since a timeout means readiness was never confirmed at all. The default is 1000 ms: the spawn-time path's warm-pane 500 ms, doubled because a respawn is a cold start. A new slow-readiness regression test toggles only the buffer and shows the pointer lost at `0` and delivered-and-submitted at `1000`, which pins the mechanism against a deterministic fixture built to ignore input for 650 ms. To be precise about what that does and does not establish: it verifies the gate's behaviour, not any real agent's startup distribution, so the buffer is a well-targeted delay rather than a measured threshold. Pi's native seed delivery and Codex's wrapper-side readiness fix are untouched.
  A delegate that goes undelivered is also no longer silent. A worker that receives its task pointer and then emits no turn-shaped agent event within its response window now produces a daemon warning plus a visible notice in the orchestrator's pane, so "never got the task" is distinguishable from "still thinking" instead of looking like a healthy, idle card. The notice is written without an Enter so it reads as scrollback rather than as a prompt — best effort, not a guarantee, since whether an agent treats a bare line feed as "submit" is unverified per agent and a later prompt can carry pending notice bytes with it, which is why the line carries no project-supplied text at all (the role name and panes go to the daemon log). It is bound to the orchestrator's identity, so it cannot land in an unrelated agent that later inherited the pane, and it is cancelled outright if the worker reports `work-done`, the delegation is superseded, or either pane closes. Its window defaults to the idle-worker timeout capped at 30 seconds, and `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` shortens it, turns it off (`0`), or arms it on its own — the two detectors are now independently switchable in both directions.
  Task pointers themselves are now delivered under the same identity guard the diagnostics use. The delegate path holds a pane id across the respawn wait plus the new buffer, and an unguarded write keyed on that string could deliver — and submit — one orchestration's task into a successor agent that inherited the pane after a close, respawn, re-home or teardown. Delivery is now bound to the exact worker agent the task was composed for and re-validated against the pane's closing state and orchestration membership immediately before writing.
  The buffer is overridable with `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` (milliseconds; `0` disables the wait, values above 30000 are capped) for operators whose machines need longer and for test harnesses that need to skip it. It is a deliberate stopgap — a fixed delay cannot prove that an agent is listening. The durable fix is a real "TUI ready" signal from the agent side, tracked in [#243](https://github.com/vfarcic/dot-agent-deck/issues/243).
  Demo reel for PRD #249: https://youtu.be/v7ONU4LTa3E. Watch before merging. It shows the fixed path end to end against two real interactive workers — Claude Code and OpenCode — each respawned through `clear = true`, receiving and submitting its task pointer, and visibly doing the delegated work.



## [0.35.1] - 2026-07-30

### Fixed

- **Ctrl+W no longer destroys a pane by accident**
  `Ctrl+W` — delete-previous-word in shells, readline, vim, and nearly every other TUI — instantly and irreversibly tore down a pane in dot-agent-deck from any mode, with no confirmation. Typing it inside an embedded shell or agent pane destroyed the pane instead of deleting a word. Three users hit this independently and reported it separately (#88, #192, #218); all three are fixed by this change.
  **Intentional behavior change — please read if `Ctrl+W` is muscle memory for you:** `Ctrl+W` no longer closes a pane while you are inside it. It now reaches the shell or agent as a normal keystroke (word-delete), exactly as it would in any other terminal. Closing a pane is now a command-mode-only action, and it always asks for confirmation first — there is no `y`/`n` one-key shortcut for that confirmation; confirm with `Down` + `Enter` or a mouse click, and the default selection is Cancel so an accidental keypress cannot close anything.
  Three other defects are fixed alongside the behavior change. First, a stale pane whose agent had already stopped on the daemon side used to wedge forever on close — the daemon's "Agent not found" response was treated as a failure and retried against an id that could never succeed, forcing a full detach and relaunch to clear it. That response is now treated as already-stopped, so the pane closes cleanly. Second, every path that can close a pane — the keybinding, the `[Close]` button, the tab-strip `×`, and the modal's own `[Close]` — now goes through the same confirmation, bound to the specific tab or pane that was armed when the modal opened, so navigating away while the modal is open can never close the wrong target. Third, the hints bar and help overlay are now mode-aware: they no longer advertise `close` in a mode where `Ctrl+W` does not close anything, and `Ctrl+D` is now a genuine two-way toggle between the dashboard and the pane you came from, so command mode always shows how to get back.
  See the [keyboard shortcuts documentation](https://agent-deck.devopstoolkit.ai/docs/keyboard-shortcuts) for the updated key behavior per mode.
- **A pane that loses its agent now says so, instead of going quietly dead**
  When a pane's agent went away and the deck's reconnect attempts ran out, the pane kept rendering its last frame and looked completely healthy — but every keystroke was silently dropped. The only hint was a status message that flashed `PTY write failed: Pane <id> stream I/O task ended`, naming an internal task rather than telling you the agent was gone. Selecting such a pane and typing looked, from the outside, like the deck had frozen.
  Those panes are now labelled `— disconnected` in the title, so the state is visible before you type anything, and their last output is preserved so you can still read what the agent did before it went away. Typing into one now reports what actually happened and what you can do about it — "Agent is no longer running — pane is disconnected. Close it to start over." — instead of an internal error. Nothing is closed automatically: the pane stays until you close it.
  The two situations that lead here are reported distinctly, because their causes are unrelated: an agent that exits on every restart attempt (usually the agent's own command failing at startup) versus an agent the daemon no longer has at all (stopped deliberately, or a daemon restart underneath the pane).
  Both give-up paths now log at warning level and name which one was taken, so a report of "my pane died" can actually be diagnosed. Previously they logged at debug level, which meant that unless you had already set `DOT_AGENT_DECK_LOG`, four different causes produced one identical symptom and left no evidence behind. See [Troubleshooting › A pane says "disconnected" and ignores what you type](https://agent-deck.devopstoolkit.ai/docs/troubleshooting) for how to capture that detail when filing an issue.
- **An agent card that appears and vanishes now says where it came from**
  If a card for an agent you never started flickers onto the dashboard and disappears again, that is another deck's agent posting into your daemon — most often a test run, or a second checkout, whose child process inherited a `DOT_AGENT_DECK_SOCKET` pointing at your session. The card is registered because the hook arrives, then retired because no local pane backs it.
  Until now this left nothing to go on: the daemon logged it as an ordinary `Received event`, indistinguishable from a real agent starting, so the only way to find out was to notice the flicker by eye and then read the log afterwards knowing exactly what to grep for. The daemon now logs a warning naming the pane, the session and the agent type when a `SessionStart` arrives for a pane it never spawned, along with the usual cause. Enable file logging with `DOT_AGENT_DECK_LOG=1` and search for `did not spawn`.
  This is a warning rather than a refusal on purpose: a pane can legitimately belong to a client whose agent the daemon does not own, and dropping those hooks would break it. Nothing about which events are accepted has changed.



## [0.35.0] - 2026-07-28

### Changed

- **Orchestration routing identity gained a per-tab instance id (semantic contract change, `PROTOCOL_VERSION` unchanged at 6)**
  fix: concurrent orchestrations no longer cross-deliver delegate/work-done signals; same-directory orchestrations now warn and point at worktrees. Opening the same orchestration in two tabs from one directory used to produce two byte-identical routing identities, so a delegate reached *both* tabs' workers and a work-done landed on *either* tab's orchestrator non-deterministically. Each orchestration tab now mints its own instance token, shared by every role pane of that tab, and the daemon routes on it — so each tab is an isolated routing group even when the orchestration name and directory are identical.
  The compatibility consideration is the routing key itself: the daemon's `pane_orchestration_map` value changed from the `(orchestration_name, orchestration_cwd)` tuple to an identity that is either `Instance(id)` or `NameCwd(name, cwd)`. That is a **semantic contract change behind a stable wire** per rule 12 (see `docs/develop/versioning.md`) — the frames still deserialize on both sides, but the meaning of "same routing group" now depends on which variant a pane's client produced. `PROTOCOL_VERSION` is deliberately **not** bumped: the new `orchestration_id` rides `TabMembership::Orchestration` as an additive optional field that older peers round-trip untouched, and a pane from a pre-#140 client simply resolves to the `NameCwd` fallback, which is byte-equivalent to the previous behaviour. Because the wire stays additively forward- and backward-compatible, the break is versioned through this fragment and the `0.x` minor bump rather than through a handshake refusal.
  Cross-version behaviour: a newer TUI against an older daemon (or an older TUI against a newer daemon) keeps routing a single orchestration correctly via the `NameCwd` fallback — correct across directories and across differently-named orchestrations, and ambiguous only in the same-name-same-directory case that has always been ambiguous. Mixed-variant identities never compare equal, so a tokened pane and a token-less pane are never merged into one routing group on the strength of a coincidental name-and-directory match. Only the newer-client-plus-newer-daemon pairing gains the per-tab partition, so the previous-release-daemon manual test at release is: build the branch, start a daemon from the previous release with an agent under it, run the branch TUI against it, and confirm a delegate still routes and hooks (work-done, status) still arrive.
  The product stance ships alongside the fix: same-directory concurrency is legal but discouraged, because the two resources the daemon cannot partition — the `.dot-agent-deck/*-{role}.md` coordination files and the working tree — are shared no matter how routing is keyed. Selecting an orchestration whose directory already hosts a live one now shows a non-blocking warning in the new-pane form naming both shared resources and pointing at a git worktree (`/worktree-prd`) as the isolated alternative. The supported model — concurrent orchestrations are safe across directories, one worktree per parallel line of work — is documented in `docs/orchestration.md`.

### Added

- **Orchestrations Report Workers That Go Silent**
  An orchestrator that delegates work and then waits gets no execution turns until the worker answers — so a worker that dies, hangs, or quietly stalls leaves the whole run parked with nothing to show for it. Nobody notices until you come back and look.
  The daemon now watches every outstanding delegation. If a worker has not reported `work-done` after `worker_response_timeout_minutes` (default **120**), the daemon injects one self-describing prompt into the orchestrator session — *"A delegated worker has not responded with work-done (dot-agent-deck daemon report, not a message from a person or an agent). It was delegated 2 hours ago. … It may be stuck, waiting on input, or still working: check its pane and decide how to proceed — if this needs the user, notify them; otherwise keep waiting, re-delegate, or reassign."* — and the orchestrator decides what happens next. The daemon itself never notifies anyone; it only reports the condition, which keeps the deck out of the business of holding chat credentials and leaves the judgment call with the agent that has the context. The prompt names itself as a daemon report and labels the role name as untrusted config metadata, so an agent reading it cannot mistake either for a message from you.
  The prompt fires once per delegation, so a stuck run cannot turn into a stream of nags, and an arriving `work-done` cancels the timer — a worker that finishes just as the deadline passes produces no alert. Closing a worker's pane cancels it too, on the grounds that you are already handling that worker.
  Set `worker_response_timeout_minutes` at the top level of `.dot-agent-deck.toml` (above the first `[[table]]` header, or TOML makes it a key of the preceding table and it is silently ignored). Values from 1 to 10080 minutes are accepted; **`0` disables the detector entirely** — no timers, no prompts — and an out-of-range value falls back to the default rather than being clamped.
  Because the idle prompt travels over the existing prompt-injection path, no new wire message was added: the change is non-breaking, and an older TUI attached to an upgraded daemon simply sees the prompt as ordinary pane output. Two limits worth knowing: a daemon restart forgets every outstanding delegation and silently disarms its pending timers, and v1 measures elapsed time since delegation rather than actual agent activity, so a legitimately long task can produce one discardable alert.
  Demo reel: https://youtu.be/arJLOwhiynE
- **Concurrent orchestrations no longer cross-deliver delegate / work-done signals**
  Opening the same orchestration in two tabs from one directory used to make a delegate land in *both* tabs' workers and a work-done land on *either* tab's orchestrator, non-deterministically — the two tabs were indistinguishable to the daemon's routing. Each orchestration tab now mints its own instance token internally, so every role pane the tab spawns routes as its own isolated group even when the orchestration name and directory are identical. Delegation and work-done feedback now always reach the tab that actually sent them, with no configuration change required.
  Opening a new orchestration in a directory that already runs one now shows a non-blocking warning naming the two things that still aren't isolated between them — the `.dot-agent-deck/*-{role}.md` coordination files and the working tree itself — and points at a git worktree as the isolated alternative. The warning informs; it never blocks, so opening a second same-directory orchestration on purpose still works exactly as before.
  The [Orchestration docs](https://agent-deck.devopstoolkit.ai/docs/orchestration) describe the supported model: concurrent orchestrations are safe across directories, and a worktree per orchestration is the way to run more than one in parallel on the same project. The previous "one orchestration tab at a time" workaround wording is gone.
  Demo reel: https://youtu.be/LKtl5IsDzww
- **Native Windows Support — Process, Path & Filesystem Backends**
  The Windows daemon can now actually run. [#42](https://github.com/vfarcic/dot-agent-deck/issues/42) laid the cross-platform `src/platform/` foundation but intentionally hard-failed the Windows daemon at bind time until the remaining backends were secure; this change implements every one of them and lifts that hard-fail. dot-agent-deck now resolves config/state/lock/runtime directories via `%LOCALAPPDATA%`/`%APPDATA%`/`%USERPROFILE%` (with the existing `DOT_AGENT_DECK_*` env overrides still authoritative), wraps agent spawn through `%COMSPEC%`/`cmd /C`, detaches the daemon with `DETACHED_PROCESS`/`CREATE_NEW_PROCESS_GROUP` (handling job breakaway), and serializes concurrent spawns with a named mutex. Peer-PID resolution (`GetNamedPipeServerProcessId`/`GetNamedPipeClientProcessId`) and `daemon stop` now work end-to-end — graceful `KIND_SHUTDOWN`, `CTRL_BREAK_EVENT` best-effort, then a Job-Object `TerminateJobObject` backstop that reaps descendants. Clipboard writes go through `CONOUT$` OSC 52, the ConPTY-appropriate equivalent of the Unix `/dev/tty` write.
  Alongside the new backends, every Windows security gap flagged by #42's review is closed: named pipes are created with an explicit current-user-SID owner and DACL (`O:<sid>D:P(A;;GA;;;<sid>)`) on both `bind` and every `accept`'d instance, and both IPC client entry points — the UI/attach client and the hook client — verify the server's owner SID before trusting a connection, so a foreign local user can no longer pipe-squat a predictable per-user pipe name to read agent output or spoof the daemon. Config files that may carry secrets (`remotes.toml`, `schedules.toml`, `session.toml`) get an explicit owner-only ACL applied immediately on creation. The synchronous TUI request path now has a real read/write deadline on Windows (overlapped I/O with a millisecond timeout), matching the Unix 5-second guarantee instead of risking an indefinite hang against a wedged daemon. Stale-endpoint detection is short-circuited on Windows, where a named pipe has no filesystem inode to probe.
  Unix/Linux behavior is unchanged — every backend above is a `cfg`-gated Windows-only addition alongside the existing Unix implementation. Full interactive end-to-end validation on a real Windows host and release binaries ship in the follow-up [#164](https://github.com/vfarcic/dot-agent-deck/issues/164).
- **Shift+Enter inserts a newline in embedded agent panes**
  Pressing Shift+Enter in an embedded agent pane now inserts a newline into the agent's draft instead of submitting it — the behavior you get running the agent directly in the same terminal — with **no terminal configuration at all** on any terminal that implements the enhanced ("kitty") keyboard protocol. Two deck-side defects compounded to cause this: the deck never asked the terminal for the enhanced protocol (so a kitty-capable terminal stayed in legacy mode and delivered a bare carriage return, losing the modifier before any deck code ran), and the pane-input encoder dropped the SHIFT modifier even when it did arrive — Shift+Enter and plain Enter were literally the same byte on the wire, so every agent read both as "submit".
  Both halves are fixed. The deck now pushes `DISAMBIGUATE_ESCAPE_CODES` at startup and pops it on *every* way out — a normal quit, a terminal I/O error that aborts the session early, and the panic path — so no keyboard mode leaks into the shell you drop back into; the push is gated on the terminal actually reporting support, so inside tmux (which reports none) the deck degrades to its previous behavior rather than pushing a mode nothing honors. Pane-input key forwarding is now modifier-aware: Shift+Enter is forwarded as `ESC[13;2u` — the one newline encoding verified to work across all four supported agents (Claude Code, OpenCode, Pi, Codex) — Ctrl+Enter as `ESC[13;5u`, Shift/Ctrl+arrows in their modifier-bearing CSI form (`ESC[1;2A` and friends), and the non-letter C0 controls the enhanced protocol disambiguates now reach the pane as their control byte rather than as a literal character (so Ctrl+[ is Escape again — vim leaves insert mode inside an embedded pane instead of typing a `[`). Plain Enter still submits, and the submit-debounce is unchanged because a CSI-u newline carries no `\r`.
  The previously documented Ghostty workaround (`keybind = shift+enter=csi:13;2u`) is no longer necessary; if you already have it, it still works and does no harm. There is no `experimental` flag to enable — this is a correctness fix to an existing input path, so it ships on by default.
  Cross-version contract (CLAUDE.md rule 12): no impact. The interactive keystroke path writes opaque bytes through the existing attach/PTY channel, and the change touches only `src/ui.rs`, tests, and docs — nothing in `src/daemon_protocol.rs`, the daemon, hooks, or orchestration. No `PROTOCOL_VERSION` bump and no semantic (same-wire, different-meaning) break; an older daemon and a newer TUI interoperate exactly as before.
  See [Troubleshooting](https://agent-deck.devopstoolkit.ai/docs/troubleshooting) for what to check if Shift+Enter still submits (running inside tmux is the main case).

### Fixed

- **Delegating to a Codex worker delivers the prompt again**
  A `clear = true` delegate to a Codex worker silently lost its prompt. The worker restarted, came up with an empty composer, and did nothing: the orchestrator believed it had delegated, while the operator watched a pane that appeared to restart and then sit idle. Codex was effectively unusable as an orchestration worker, because every delegation to it was dropped. Both defects behind that are fixed, so a Codex worker now receives its task and starts working like every other agent.
  Prompt delivery now waits for a signal that actually means "the agent can accept input". `dot-agent-deck wrap` emits a session-start event the moment it forks the child, purely so the dashboard card appears immediately for a slow-booting agent — but at that point the pane is often still running only the launcher, seconds before the agent's own interface exists. That fork-time event is now marked as card-surfacing only, and the delegate readiness gate ignores it for agents that will post a genuine session-start of their own, so the prompt is no longer typed into a launcher's line discipline and echoed away. Wrapper-strategy agents with no native hooks are unaffected: their fork-time event remains their readiness signal, so they still receive prompts immediately rather than waiting out a timeout. That fallback timeout, which only applies when an agent's native hooks never fire, is raised from 10 to 30 seconds to match measured Codex startup.
  A pane's launch shape is also stable now. Previously a pane started from a command whose name did not identify an agent (for example `devbox run codex-big`) launched unwrapped, and then, once hook events revealed the real agent type, came back up *wrapped* after its first delegate — the same pane running a different process tree before and after. The agent type learned from hook events now updates the dashboard badge only, never how the pane relaunches. A worker whose role command you have not touched therefore restarts exactly as it first started, while an edited role command is still honored: how the pane launches follows the command it is actually running, so editing a role to a different agent no longer relaunches it disguised as the old one.
  Both changes are additive on the wire in both directions: an older build and a newer one still interoperate, so no action is required when upgrading.
  Demo reel for PRD #225: https://youtu.be/m_1o0nFReho. Watch before merging.



## [0.34.1] - 2026-07-26

### Fixed

- **Pi agents now show "Idle" between turns, like every other agent**
  A Pi pane used to read **"Needs Input"** whenever it finished a turn — most visibly an orchestrator that had just delegated to a worker, or an auditor that had finished its pass — even though nothing needed the user: it was simply idle, waiting for the worker's result. Pi was the only backend that behaved this way. Claude, OpenCode, and Codex all show **Idle** when a turn ends and reserve "Needs Input" for a genuine prompt (a permission request or attention notification).
  Pi now matches them: ending a turn reports **Idle**. Because Pi does not currently surface a permission/attention signal, a Pi pane simply never shows "Needs Input" — exactly like a Claude pane that never hits a permission prompt. This is a status-label change only; delegation and worker wake-up were always correct (a finished orchestrator is woken when its worker reports back), and nothing about the wire protocol changed.



## [0.34.0] - 2026-07-25

### Changed

- **Attach-protocol bump: guarded input delivery + typed stream rejections (`PROTOCOL_VERSION` 5 → 6)**
  Input delivery from the TUI to a daemon-driven pane is now identity- and idempotency-guarded, and the daemon reports a *typed* reason when it refuses a keystroke instead of dropping it silently. Two coordinated wire changes ride one `PROTOCOL_VERSION` bump (5 → 6):
  - **New attach-stream frame `KIND_STREAM_REJECT` (server → client).** When the daemon refuses a `KIND_STREAM_IN` key/paste frame because the focused target went non-live (history-only / view-only), exited, or rebound while the stream stayed open, it now emits a non-terminal `KIND_STREAM_REJECT` frame carrying a short reason (e.g. `history-only`) rather than debug-logging the drop and leaving the client stuck typing into a dead pane. Adding a server→client frame kind changes the attach-stream wire shape, so per rule 12 this is a hard `PROTOCOL_VERSION` bump, not an additive-field change. The client surfaces the reason and leaves its input mode for both key and paste; the stream stays open.
  - **Guarded-send capability handshake (semantic break behind a stable wire).** A newer client that issues an *identity-bearing* `write-and-submit` (carrying `expected_agent_id` / `expected_session_id` / `delivery_id`) now first checks that the daemon advertises a `guarded_send` capability on its `Hello` reply. The daemon enforces the guards: an exact agent-and-session-generation match before writing (refusing `stale` / `wrong-session` with no bytes), atomic `delivery_id` deduplication (a lost-response retry replays the first result; a partial write is reported `ambiguous`, never blind-retried), and a re-validation of liveness/ownership *after* acquiring the target writer. An older daemon silently ignores those fields and just returns `ok=true`, so a new client that trusted it could double-submit on a retry or mis-deliver on a rebind. To prevent that, a new client now **fails safe** — it refuses to submit an identity-bearing prompt when the capability is absent, preserving pre-PRD-20 fire-once semantics against an old daemon. The capability rides the `Hello` reply as an additive optional field (an old daemon simply omits it), so it is decoupled from the version number; the version bump is driven by the new frame kind above.
  Classification: a cross-version compatibility consideration for the **TUI↔daemon send/attach contract** — a semantic break (a new client must not trust an old daemon's unguarded `ok=true`) plus a stream wire-shape change (the new frame kind). This is `breaking` per rule 12. While the major version is `0`, a `breaking` fragment bumps the **minor** digit.
  Cross-version behavior after the bump: the exact-match attach handshake refuses an old-reader/new-daemon or new-reader/old-daemon pairing at connect time (a clean `ProtocolMismatch`, recover by upgrading), and on the local socket the build-version (`DAD_BUILD_ID`) handshake already forces a daemon restart on any in-place binary upgrade. **A previous-release-daemon manual test is required at release**: build the branch, start a daemon from the previous release with an agent under it, run the branch TUI against that older daemon, and confirm a delegate still routes and hooks (work-done, status) still arrive — and that an identity-bearing send against the old daemon now fails safe rather than double-submitting.

### Added

- **Multi-agent machinery + Codex as a first-class agent**
  Adding a new agent to dot-agent-deck used to mean touching scattered `match AgentType` arms for detection, badge colour, label, default command, and the install/event path. PRD #20 replaces that with a curated, compiled-in **agent registry** (`src/agent_registry.rs`): one cohesive entry per agent carries its label, detection basenames, default command, badge colour, and the **integration strategy** it uses (`NativeHooks` / `Plugin` / `Extension` / `Wrapper`). Detection, coloured card badges, the `type:` filter, per-agent default commands, and startup install are all now *derived* from that one entry — adding an agent that reuses a shipped strategy is a registry entry plus a release, not a cross-file edit. The move is behaviour-preserving for the existing agents (Claude Code, OpenCode, Pi): the prior test suite passes unchanged.
  **Codex is the first agent on the new `Wrapper` strategy, and it ships with full event parity.** `dot-agent-deck wrap -- codex` hosts Codex on an inner PTY (so it stays fully interactive) while teeing its output through pattern detection. But because bare interactive Codex paints an ANSI TUI with no machine-readable JSON, the rich events — prompt text, tool calls, and turn-completion Idle — come from Codex's **Claude-Code-compatible native hooks** instead of stdout scraping. The deck installs a `hooks.json` into your Codex home (`$CODEX_HOME`, else `~/.codex`) whose hooks shell `dot-agent-deck hook --agent codex`, and — because Codex only runs hooks it trusts — records trust for **exactly those entries** in that home's `config.toml`, pinned to each hook's content hash as Codex itself reports it. A third-party hook sitting in the very same file stays untrusted, and your `config.toml` is edited surgically: comments, your model choice, and any trust you recorded yourself come back byte-for-byte. Setup runs at startup and again at Codex pane launch, so it is **independent of how you start Codex** — bare `codex`, an absolute path, an alias, or a launcher like `devbox run codex-big` all behave identically, with **nothing to add to your script**. Those payloads are the same shape Claude posts (a shell tool arrives as `tool_name: "Bash"` with a string `command`), so they ride the existing `AgentEvent` hook socket — no new wire, no protocol bump. The coarse stdout/`codex exec --json` classifier remains as a fallback when hooks can't fire.
  The dashboard now distinguishes agents visually: each card shows a coloured **agent-type badge**, the `/` filter supports **`type:codex`** (and `type:claude`, `type:opencode`, `type:pi`), and the stats bar breaks down by agent type when multiple are active. Spawning is registry-driven end to end — the **new-pane Agent selector** seeds the Command field from the selected agent's default command, and a Wrapper-strategy command is auto-rewritten to run under `dot-agent-deck wrap`.
  Liveness is now honest. The `AgentEvent` protocol gained a `live_target` descriptor (`kind` + `writable`) and input delivery returns a real **`send_result`** (`applied` / `queued` / `stale` / `wrong-session` / `history-only` / `no-live-target`) instead of fire-and-forget. A wrapped Codex session declares `Pty`/`Live` when it runs inside a deck-managed pane and `Process`/`HistoryOnly` when standalone, so the UI renders view-only sessions distinctly and surfaces failed or stale sends rather than silently dropping keystrokes.
  **Using Codex as a role or worker** additionally needs sandbox network access so the deck's `delegate` / `work-done` commands can reach the daemon socket — launch with `--sandbox workspace-write --ask-for-approval never -c "sandbox_workspace_write.network_access=true"`. See [Troubleshooting](https://agent-deck.devopstoolkit.ai/docs/troubleshooting), which also covers what to check if a Codex card shows only coarse status.
  This is machinery meant to be built on: the follow-up [Gemini](https://github.com/vfarcic/dot-agent-deck/blob/main/prds/211-gemini-adapter.md) (wrapper) and [Aider](https://github.com/vfarcic/dot-agent-deck/blob/main/prds/212-aider-adapter.md) (log-watcher) PRDs reuse this seam, and the maintainer-facing [agent adapter guide](https://github.com/vfarcic/dot-agent-deck/blob/main/docs/develop/agent-adapters.md) documents the full "add an agent" checklist. There is no new user-facing flag to enable — Codex, badges, and the type filter ship visible by default.
  Demo reel for PRD #20: https://youtu.be/ogIesEQ_nPk. Watch before merging.
- **Native Windows Support — Foundation**
  Groundwork for running dot-agent-deck natively on Windows (`x86_64-pc-windows-msvc`) without WSL. This change lands the cross-platform foundation — a `src/platform/` abstraction seam, a native Windows named-pipe IPC transport, and a continuous `windows-latest` CI gate — so the port can be completed and shipped in the follow-ups. It does **not** yet produce a Windows release binary (that is [#164](https://github.com/vfarcic/dot-agent-deck/issues/164)), and the Windows daemon is intentionally hard-failed for now (see the note below).
  The internal IPC layer has been rewritten behind a cross-platform abstraction (`src/platform/`). On Unix the behavior is identical to before — Unix domain sockets, `flock`, `getsockopt(SO_PEERCRED)`/`LOCAL_PEERPID`, and `setsid` are preserved unchanged (the Unix backends are behavior-for-behavior lifts of the previous inline code). On Windows, named pipes replace Unix domain sockets (`\\.\pipe\dot-agent-deck-{user}-hook` / `-attach`), which removes the entire class of stale-socket bugs, and `GetNamedPipeServerProcessId` gives zero-protocol-byte peer-PID resolution (the analogue of `SO_PEERCRED`, used by `daemon stop` and the version handshake). The `libc` crate is now gated to `cfg(unix)` dependencies; `windows-sys 0.59` and `dirs 6` are added as `cfg(windows)` counterparts. A continuous `windows-latest` CI job (build + clippy + nextest) keeps the Windows branches compiling and green on every commit.
  **Note:** This is the foundation only. The Windows daemon is intentionally hard-failed (`IpcListener::bind` returns `Unsupported` before creating any pipe), so the remaining Windows backends — process detach, spawn-serialization locking, filesystem/pipe security descriptors, and clipboard — are compiling skeletons whose real implementations, together with the secure named-pipe security descriptor, land in the follow-up [#163](https://github.com/vfarcic/dot-agent-deck/issues/163). Full interactive e2e validation and release binaries (`.exe`, Scoop) ship in [#164](https://github.com/vfarcic/dot-agent-deck/issues/164).

### Fixed

- **Restored agent panes show Idle immediately**
  Restored panes whose saved command identifies Claude Code, OpenCode, or Pi now show `Idle` as soon as the agent process starts. Previously, Agent Deck inferred the agent type for the daemon but discarded it from the local dashboard placeholder, so a recognized agent incorrectly appeared as `No agent` until its first lifecycle event.
  The inferred type is also retained when a saved mode cannot be rebuilt and falls back to a plain dashboard pane. No configuration changes are required.
- **Pi and OpenCode agents now report status regardless of how they are launched**
  Previously, in a mixed-agent setup only Claude cards showed live status in the dashboard — Pi and OpenCode cards stayed on "No agent" both after starting and while working. Two independent causes are fixed.
  **Pi** launched through any wrapper — `devbox run pi-big`, a shell script, or an absolute path — got no status because its orchestrator extension was materialized only when the spawn command's basename was literally `pi`. The extension is now materialized once at daemon startup (like Claude's hooks and OpenCode's plugin install at startup), so it no longer depends on how the agent is launched. The location mirrors Pi's own resolution — it honors `PI_CODING_AGENT_DIR` (falling back to `~/.pi/agent`), so a user who relocates Pi's directory still gets a status-tracked pane.
  **OpenCode** got no status because its plugin was installed into a nested directory (`plugin/dot-agent-deck/index.js`) that current OpenCode versions do not scan — they discover local plugins only as flat files directly under `plugin/`. The plugin is now installed as a flat `plugin/dot-agent-deck.js`, and the obsolete nested directory is removed automatically on the next launch, so existing users are migrated with no manual step. This applies to every user through the normal startup auto-install; no reinstall or configuration change is required.



## [0.33.0] - 2026-07-14

### Changed

- **Attach-protocol bump: first-class `Pi` agent type (`PROTOCOL_VERSION` 4 → 5)**
  Pi is now a first-class, status-tracked agent type, so `AgentType` gained a wire-serialized `pi` value. That value rides `AgentRecord.agent_type` (in the `ListAgents` response) and `AgentEvent.agent_type` (in the daemon→TUI `KIND_EVENT` broadcast) — both payloads a peer decodes as a whole. A build that predates this variant and lacks a catch-all fallback fails the entire response/frame decode when it sees `agent_type = "pi"`, breaking its agent list and live status stream. That is a non-forward-compatible payload-schema change (the same class as PRD #120's new `BroadcastMsg` variant), so this is classified `breaking` per rule 12 and `PROTOCOL_VERSION` bumped from 4 to 5, which the attach handshake refuses across.
  Classification: this is a cross-version compatibility break for **pre-Pi readers** (an older TUI attached to a newer daemon that runs a Pi pane), not generic user-facing breakage. It supersedes the earlier "no `.breaking.md` needed" note, which was correct only for the purely additive `dot-agent-deck agent-event` subcommand — not for the enum addition.
  Mitigation: `AgentType` now also carries a `#[serde(other)]` fallback that decodes any unrecognized value (a `pi` record at an old reader, or a future agent type at today's build) to the neutral `None` ("No agent") placeholder instead of erroring, so this build and every future one degrade gracefully — future agent-type additions need no further bump. Already-released pre-Pi binaries predate that fallback; for them the `PROTOCOL_VERSION` bump is the actual guard: the exact-match handshake turns an old-reader/new-daemon pairing into a clean connect-time `ProtocolMismatch` (recover by upgrading the reader) rather than a mid-session deserialize crash. On the local socket the same skew is already forced to a daemon restart by the build-version (`DAD_BUILD_ID`) handshake, which fires on any in-place binary upgrade. While the major version is `0`, a `breaking` fragment bumps the **minor** digit.

### Added

- **New-agent command defaults to the last executed command**
  The new-agent form's Command field now pre-fills from the last command you launched when no `default_command` is configured, eliminating the need to retype the same command on every spawn.
  The seed follows a strict fallback chain: if `default_command` is set in your config it wins unconditionally (no change for existing users); otherwise the field pre-fills from the most recent command you launched from the new-agent form; if no prior command exists (fresh install or cleared state) the field remains blank, preserving the current behavior. The value is global, persisted across deck restarts, and reflects the last command you launched from the form in any mode (schedule / issue-dispatch authoring included). The field remains editable; a wrong pre-fill costs a single clear.
  See the [Configuration reference](https://agent-deck.devopstoolkit.ai/docs/configuration) for details on `default_command` and the fallback behavior.
  Demo reel for PRD #196: https://youtu.be/I6A-uOEirEo
- **Pi as a First-Class Agent**
  [Pi](https://github.com/earendil-works/pi) is now a third, first-class, status-tracked agent type alongside `claude` and `opencode` — and its TypeScript extension API makes the orchestrator role deterministic instead of prompt-and-pray. `dot-agent-deck` does not bundle or vendor Pi itself (only a small extension is compiled into the binary); Pi is detected on `PATH` exactly like the other two agents.
  For a Pi orchestrator, `delegate(role, task)` and `work-done(summary)` are now native, schema-validated tools instead of a CLI string the model has to remember to type correctly — calling the tool shells the existing `dot-agent-deck delegate` / `work-done` commands and the daemon routes to the worker pane exactly as it does today. Status reporting is event-driven: the bundled extension subscribes to Pi's own event bus and reports lifecycle/status through a new `dot-agent-deck agent-event --type <state>` subcommand, so a Pi pane shows running / waiting-for-input / finished in the TUI **with no Claude-Code-style hook installed and no `settings.json` mutation** — including headless, unattended panes with no client attached. Prompt delivery is native too: the extension pulls the pane's seed (a read-only `dot-agent-deck get-seed`) on `session_start` and delivers it through Pi's own `sendUserMessage`, so the orchestrator starts working without the deck typing into the terminal (PTY injection remains only as a bounded fallback). Because status plumbing lives at the `AgentType` level, a plain `pi` pane opened from the dashboard or a scheduled `pi` job are status-tracked the same way; the orchestrator flow is the flagship use case, not the only one.
  Setup is zero-step: install `pi` (`npm install -g @earendil-works/pi-coding-agent`) and point a role at it with `command = "pi"` in `.dot-agent-deck.toml`. The deck **auto-materializes** the bundled extension into Pi's extension directory the first time it spawns a Pi pane — there is no manual install command to run. (`dot-agent-deck orchestrator setup` remains as an optional explicit path that detects `pi` on `PATH`, printing the install command if it's missing, and materializes the extension ahead of time.)
  Pi ships visible by default — no feature flag to enable. It deliberately does not replace `claude`/`opencode`, remove hooks for non-Pi agents, or adopt Pi's own multi-agent orchestration — `dot-agent-deck`'s daemon remains the orchestrator-of-record.
  See [Orchestration](https://agent-deck.devopstoolkit.ai/docs/orchestration) for setup and usage, and the [Pi extension developer guide](https://github.com/vfarcic/dot-agent-deck/blob/main/docs/develop/pi-extension.md) for the tool/event contract.
  Demo reel for PRD #201: https://youtu.be/gSw9zc0Y54E



## [0.32.1] - 2026-07-11

### Fixed

- **TUI no longer crashes on a wide character in a short pane**
  Attaching to a deck — most visibly over `connect` to a remote — no longer renders garbled and then exits when a dispatched orchestration surfaces many role panes stacked into one column. A pane that collapses to a single row could feed a wide character (CJK text, emoji) into the terminal emulator in a way that panicked the whole TUI on attach; the panic is now contained at the agent-output boundary. A malformed output chunk from an agent is dropped (that pane may render briefly stale) instead of taking the session down, so the deck stays up and the rest of the panes keep streaming.
- **macOS: version-mismatch daemon restart no longer silently no-ops**
  On macOS, attaching a new-build TUI to a still-running older-build daemon now actually restarts that daemon, as it always has on Linux. The build-version handshake re-resolved the daemon's PID over its socket to guard against PID reuse, and on macOS that lookup fails once the daemon closes its end of the handshake connection — which the code mistook for "the daemon already exited," so it skipped the restart and left you attached to the stale, incompatible daemon. The restart now falls back to the PID captured when the connection was live, so the old daemon is terminated and a fresh one at the current build takes over.



## [0.32.0] - 2026-06-25

### Changed

- **Attach-protocol bump: live orchestration surfacing (`PROTOCOL_VERSION` 3 → 4)**
  Dispatched orchestrations now surface as a live tab on an already-attached TUI. To carry the orchestration's structural membership (name, cwd, per-role index/name/start-flag/pane) the daemon→TUI event broadcast (`BroadcastMsg`, forwarded over the attach socket's `KIND_EVENT` frame) gained a new `orchestration_surface` variant. That is a wire-shape change an older peer would fail to deserialize, so `PROTOCOL_VERSION` bumped from 3 to 4.
  Compatibility impact: a TUI and daemon must be on the same `PROTOCOL_VERSION` to interoperate. The local attach/subscribe handshake does not itself compare `PROTOCOL_VERSION` — refusal across a mismatch is delegated to the build-version handshake (`ensure_compatible_daemon_or_die`), which is effective because a protocol bump always rides a new build and therefore a new `DAD_BUILD_ID`. The daemon deliberately outlives the TUI, so after upgrading in place an older still-running daemon and the new TUI differ on `DAD_BUILD_ID`; the build-version handshake detects that and prompts to restart the daemon (the standard recovery) before the new `orchestration_surface` frames can reach an older peer. Hook events and the single-agent live-surface path are unchanged.

### Added

- **Smarter initial configs from the AI config generator**
  The AI config generator (`dot-agent-deck generate-config`) now produces initial `.dot-agent-deck.toml` files that require less hand-editing. Four targeted improvements were derived from a systematic analysis of hand-improved configs across the author's real projects and landed into `assets/config_gen_prompt.md` and `assets/roles.toml`.
  **What changed in generated configs:**
  - **Release role gets a mandatory human merge gate (P1).** The generated release role now produces a two-phase flow — open the PR and wait for CI + automated review, report results back, then **stop** before merging. Auto-merge is gone. Reviewers see the output before the branch lands.
  - **Test-mandating projects get a tester role and RED/GREEN chain (P2).** When the generator detects that a project requires tests, it now proposes a `tester` role and wires a `tester → coder → tester` feedback loop so the cycle enforces GREEN before declaring the task done.
  - **Auditor is now a default-on role (P3).** The auditor was previously omitted unless explicitly requested. It now appears in the initial role roster for projects where it is relevant, avoiding a common post-generation hand-addition.
  - **Coder and tester prompts name the project's actual test/lint command (P4).** Role `prompt_template`s for `coder` and `tester` now include the project's concrete test runner and lint invocation (e.g. `cargo nextest run`, `cargo clippy -- -D warnings`) instead of generic placeholders.
  These improvements were validated by regenerating configs for five sibling repos after the edits and diffing each against its live `.dot-agent-deck.toml`; two of five showed a measurably smaller structured diff across the targeted regions. The repeatable re-run procedure is documented in `docs/develop/config-gen-regeneration.md` so the comparison can be repeated as more projects are added.
- **Scheduled issue dispatch (`issue_dispatch` task type)**
  dot-agent-deck can now automatically spin up agents on open GitHub issues on a cron schedule. A new `issue_dispatch` scheduled-task type — configured in `~/.config/dot-agent-deck/schedules.toml` — clones (or fast-forwards) a target repo, enumerates its open issues up to a configurable cap, and launches one agent worktree per issue, skipping any issue that already has an in-flight worktree or an open PR on the `agent/issue-<n>` branch.
  The task is defined by four fields: `repo` (`owner/name`), `working_dir` (clone parent), `prompt` (a free-text template with `{{issue_number}}` substituted per issue, default `Work on issue {{issue_number}}`), and `max_per_run` (an enumeration cap, default 3). The agent is spawned with the deck's standard `spawn` primitive and runs against the per-issue worktree, so full orchestration configs in the target repo's `.dot-agent-deck.toml` are picked up automatically. Tab-close cleans up the per-issue worktree (while preserving the clone) via a new daemon-side worktree registry and close-detection watcher.
  You can author one of these tasks by hand, with `dot-agent-deck schedule add --repo <owner/name> --max-per-run <N> …` (no `--command` needed — the per-issue agent comes from each cloned repo's config), or — behind the `experimental` feature flag — with the guided `schedule: issues` option in the new-pane dialog. A configured `issue_dispatch` task always runs regardless of the flag; the flag gates only that in-deck guided authoring option (enable it with `[features] experimental = true` in `.dot-agent-deck.toml` or `DOT_AGENT_DECK_EXPERIMENTAL=1`).
  See [Scheduled tasks — issue dispatch](https://agent-deck.devopstoolkit.ai/docs/scheduled-tasks) for configuration examples and a walkthrough.
  Demo reel for PRD #120: https://youtu.be/IZYdqPqmEMU.
- **Reconnect Restores Live Session Status**
  When reconnecting to a running daemon, each agent card now shows the agent's real status immediately — no more cards stuck on "Idle" or "No agent" until the next event arrives.
  Previously, reconnecting with `dot-agent-deck connect` (after a disconnect, ssh drop, or closing the TUI) rebuilt the dashboard from spawn-time metadata only, resetting every card to `Idle` and dropping any event-derived agent label. An agent that was genuinely idle or waiting for input would never self-heal — its card stayed wrong for the entire session. This was especially visible after the auto-reconnect introduced in PRD #148 (laptop sleep/wake): the ssh session resumed but the dashboard showed stale state.
  The daemon's live, event-derived session state (`status`, agent type, active tool, tool count, first prompts, and last user prompt) is now attached to the reconnect snapshot and used to seed each card on hydration. An agent mid-tool keeps its active-tool label and tally; a card that earned its "ClaudeCode" or "OpenCode" label via events keeps it across the reconnect instead of reverting to "No agent". Older daemons that don't send the snapshot degrade gracefully to the previous bare-placeholder behavior.
  See [Session Management](https://agent-deck.devopstoolkit.ai/session-management) for details on resuming sessions.
  **Demo:** [reconnect restores live status](https://youtu.be/QlmG_dLywU8)

### Fixed

- **Delegated Workers Start Without a Manual Enter**
  Delegating to a worker role now injects a single-line prompt into the worker pane instead of a multi-line block. Previously the multi-line prompt (which carried the `## When done` completion instructions) could land in the worker's input as a compacted bracketed-paste that sat unsubmitted until an operator pressed Enter — stalling unattended orchestration. The completion instructions now live in the worker's task file (`.dot-agent-deck/worker-task-<role>.md`), so the worker still knows to signal back via `dot-agent-deck work-done` once the single-line pointer auto-submits.
- **OpenCode plugin install now refreshes every existing layout**
  Fixes a silent failure where OpenCode panes showed "No agent" / `Tools: 0` indefinitely on machines that have both `~/.config/opencode/` and `~/.opencode/` present. Previously, `auto_install` and the `install` subcommand wrote the plugin only to whichever root `detect_opencode_root` picked first (XDG wins), leaving the other layout's plugin pointing at a stale binary path. The `execFileSync` call in the stale plugin threw `ENOENT`, was silently swallowed, and no events reached the daemon.
  Both `auto_install` (runs on dashboard startup) and `dot-agent-deck install` now fan out and write the plugin into every existing layout root — mirroring how `uninstall` already sweeps all layouts. If only one root exists, only that one is written. If neither exists, `auto_install` remains a no-op and `install` creates the XDG default as before. The dashboard always overwrites the plugin on startup, so manually-edited plugin files in either layout will be replaced.



## [0.31.2] - 2026-06-21

### Added

- **Consent-based daemon restart and smarter connect behavior**
  Version differences between the TUI and a running daemon no longer block you from connecting. Previously, any build-id difference — even a patch upgrade — triggered a hard failure that forced you to upgrade both sides immediately, stopping all running agents in the process.
  The TUI-to-daemon attach handshake is now consent-based: when you run a newer TUI against an older daemon with **no running agents**, the daemon restarts silently and you connect normally. When **agents are running**, the prompt names each live agent and asks for confirmation before stopping them; declining keeps the existing daemon and all agents intact. Non-TTY and CI environments exit non-zero on a mandatory restart, preserving script behavior.
  The blocking laptop-to-remote version comparison in `connect` is removed. An un-upgraded remote host now connects normally — no forced `remote upgrade` just because your laptop is one patch ahead. When your laptop is newer, an optional one-step nudge appears (`y` upgrades the remote and connects; `Enter`/`n` connects as-is); the nudge is skipped in non-TTY environments and never suggests a downgrade. The `AttachResponse` handshake now carries `running_agents` (count and names) and `daemon_version` as additive optional fields — forward-compatible with older daemons that omit them.
  See [Installation](https://agent-deck.devopstoolkit.ai/docs/installation), [Remote Environments](https://agent-deck.devopstoolkit.ai/docs/remote-environments), and [Troubleshooting](https://agent-deck.devopstoolkit.ai/docs/troubleshooting) for updated behavior details.
- **PRD Demo Reel**
  At PRD completion, the pre-PR gate now automatically produces a single narrated MP4 that shows — for each e2e test the PRD added or changed — a readable title/description card followed immediately by that test's terminal recording, in catalog order. The reel is uploaded unlisted to YouTube and the URL is surfaced to the maintainer pre-merge and posted as a PR comment, making "watch the new behavior before approving" a one-click step instead of replaying individual asciinema casts.
  The system is split into two components: a reusable **engine skill** (`.claude/skills/demo-reel/`) that accepts a format-agnostic manifest (`[{title, description, clip}]` where `clip` is a `.cast`, `.gif`, or `.mp4`) and produces a single stitched MP4 with per-entry title/description cards rendered as terminal frames through `agg`, and a **dot-agent-deck adapter** (`.claude/skills/demo-reel-adapter/`) that builds the manifest from the branch's new/changed `#[spec]` e2e tests by reading each test's `test.md` title and `## Scenario` paragraph. When the branch changed no e2e tests, both the reel step and the PR comment skip cleanly with a clear reason. The engine is also directly invocable by a human or CI via `reel.sh manifest.json [--publish]`.
  Prerequisites — `agg`, `ffmpeg`, and a YouTube uploader — are added to `devbox.json`. A one-time YouTube OAuth refresh token must be provisioned by a human and stored via `vals` / `.env.vals.yaml`; the engine checks for all prerequisites and fails with an actionable message if any are missing.
  See the [Demo Reel developer guide](https://github.com/vfarcic/dot-agent-deck/blob/main/docs/develop/demo-reel.md) for the manifest contract, credential setup, and local usage instructions.

### Fixed

- **Cross-version connect and agent-detection fixes**
  Connecting a laptop one patch ahead of a remote no longer fails with a build-id mismatch error. The laptop-side `connect` version/build comparison that blocked this was removed; it compared the wrong pair (laptop binary vs remote binary rather than remote TUI vs remote daemon) and protected nothing.
  When an older daemon omits the new `running_agents` field from the handshake response, the TUI now falls back to `list_agents()` to determine whether agents are running before deciding to restart. This prevents a silent agent kill when a newer TUI first attaches to an older daemon that does not yet send the field.



## [0.31.1] - 2026-06-21

### Fixed

- **Bare commands installed via `~/.bashrc` (such as `opencode`) now resolve in panes**
  The daemon's login-shell PATH capture now runs your `$SHELL` as an **interactive** login shell (`$SHELL -ilc`) instead of a login-only one (`$SHELL -lc`). An interactive login shell sources `~/.bashrc` exactly as an SSH session does, so directories added there — for example `~/.opencode/bin`, where the opencode installer appends its `PATH` line *after* the standard non-interactive guard (`case $- in *i*) ;; *) return;; esac`) — are now captured. Previously only directories exported from login profiles like `~/.profile` (such as `~/.local/bin`, where `claude` lives) were seen, so a bare `opencode` failed to spawn even though it worked over SSH. The rule of thumb now holds: if a command resolves when you SSH into the machine, it resolves in a dot-agent-deck pane. As before, the PATH is captured once at daemon startup, so a profile change or newly installed tool takes effect after a daemon restart.
- **Real Agent Shown Immediately on Reconnect**
  Reconnecting the TUI to a running daemon (for example `ctrl+c` → stop → `dot-agent-deck connect`) now shows each deck's real agent (`Claude Code` / `OpenCode`) right away, instead of displaying "No agent" until that agent next emitted a hook. The daemon now remembers the agent type it learns from hook events, so `list_agents` reports it on the next connect.



## [0.31.0] - 2026-06-20

### Added

- **Login-shell PATH parity + configurable schedule authoring**
  Bare commands like `claude` and `opencode` now resolve correctly in every spawned pane — dashboard panes, scheduled-task fires, and the schedule-authoring helper — even when the daemon is launched without the user's login profile (for example, over a non-interactive SSH session or a systemd service). Previously, daemons started without a login profile lacked `~/.local/bin` on their PATH, causing pane spawns to fail with "Unable to spawn `<cmd>` because it doesn't exist on the filesystem and was not found in PATH."
  At daemon startup the login-shell PATH is captured once (`$SHELL -lc 'printf %s "$PATH"'`) and applied to the daemon's own process environment, so every subsequently spawned pane inherits it automatically. If the capture fails for any reason (no `$SHELL`, timeout, empty output), the daemon keeps its inherited PATH with no regression. A profile change — such as adding a new tool to `~/.local/bin` — takes effect after a daemon restart.
  The hardcoded `claude` authoring agent is replaced with a configurable command. In the Scheduled Tasks manager, pressing `a` (Add) or `e` (Edit) now opens the same directory-picker and form used by `Ctrl+n`, mode-locked to schedule creation (titled **New Schedule** or **Edit Schedule**). Pick a working directory, set the Command to any agent you have installed, and confirm — the authoring agent spawns in the chosen directory. The Command field pre-fills from `default_command`; any bare name or full path is accepted.
  See [Configuration](https://agent-deck.devopstoolkit.ai/docs/configuration), [Scheduled Tasks](https://agent-deck.devopstoolkit.ai/docs/scheduled-tasks), and [Troubleshooting](https://agent-deck.devopstoolkit.ai/docs/troubleshooting) for details.



## [0.30.0] - 2026-06-15

### Changed

- **Auto-Restore TUI State on Attach; `--continue` Flag Removed**
  Running `dot-agent-deck` now **restores your previous workspace automatically** — panes, agents, and orchestration tabs are recreated from the last saved snapshot. Previously, an empty session was the default and `--continue` was required to restore state; that flag is now removed.
  **What changed for users:**
  - **Auto-restore is the new default.** Both `dot-agent-deck` (local) and `dot-agent-deck connect` (remote) restore the previous session on startup. On a fresh machine with no prior snapshot, the dashboard starts empty as before. On reconnect after a daemon crash, the workspace is recreated from the snapshot — agents are respawned and orchestration tabs are rebuilt.
  - **`--continue` is removed.** Passing `--continue` prints a friendly message explaining that auto-restore is now the default and exits. Remove `--continue` from any wrapper scripts or aliases.
  - **Fresh start via `dot-agent-deck snapshot clear`.** To begin a new session without the previous workspace, run `dot-agent-deck snapshot clear`. This deletes the global snapshot (`~/.config/dot-agent-deck/session.toml`, or the path set by `DOT_AGENT_DECK_SESSION`) and the next launch starts empty.
  - **Snapshot stays continuously fresh.** The snapshot is written on every meaningful state change (new pane, rename, close, agent stop/restart, orchestration changes) and on detach — not only at clean quit. A 750 ms coalescer prevents excessive disk writes during bursts.
  - **Orchestration tabs survive restart.** When the daemon is empty (first launch on a new machine, or after a crash), orchestration tabs are rebuilt from the snapshot: orchestrator pane, role panes in order, prompts, and role cursor position. On config drift (config deleted or roles removed), a warning is shown and the tab falls back to a plain dashboard pane.
  **Migration:** Remove `--continue` from any wrapper scripts or shell aliases. Auto-restore is now the default — no flag needed.
  See the [Session Management docs](https://agent-deck.devopstoolkit.ai/docs/session-management) for full details on the restore model and the `snapshot clear` escape hatch.

### Added

- **Roomier Button Bar and Auto-Sized Modals**
  The bottom button bar and modal dialogs no longer cram content into fixed-footprint surfaces as the UI grows.
  The button bar now wraps to a second row when full labels don't fit at the current terminal width, keeping complete labels for every button. Previously, at split-screen or windowed widths (~120 columns), the bar collapsed all buttons to shortcut-only chips while the Scheduled Tasks button inconsistently kept its label. The wrapped layout is uniform — every button degrades equally — and the dashboard or pane region shrinks by exactly the extra row so content is never overlapped or clipped (a minimum of one content row is always reserved).
  Modal dialogs (Scheduled Tasks manager, new-pane/new-deck form, and confirmation prompts) are now content-driven auto-sized via a shared helper: each modal sizes itself to its content, is clamped to no more than 90% of the terminal in each dimension, and is centered on screen. The per-dialog band-aids introduced over prior sessions (`wrap_to_width`, `truncate_cell`, `layout_mode_chips`) have been removed and superseded by this single consistent approach.
- **Consistent Color Scheme Across Deck Cards and Embedded Panes**
  Agent status is now color-coded consistently whether an agent appears as a dashboard deck card or as an embedded pane — the same state looks the same everywhere in the TUI.
  Previously, deck card borders encoded agent status (green = working, blue = thinking, yellow = waiting, red = error) while embedded pane borders encoded focus (cyan = focused, dimmed = unfocused), so the same agent could look different depending on the surface rendering it. Now both surfaces share a single semantic palette: border color encodes status in both contexts, selection is indicated by a Magenta highlight and `▸` arrow marker, and focus is indicated by Cyan — none of these roles overlap, so status, selection, and focus are always visually distinguishable.
  The status color mapping is: Working = Green, Thinking = Blue, Waiting = Yellow, Error = Red, Idle = DarkGray.

### Fixed

- **Corrected Ctrl+D Footer Label**
  The pane footer now correctly labels the Ctrl+D keybinding as `[Command Mode Ctrl+D]` instead of the misleading `[Detach Ctrl+D]` — the binding returns to the dashboard/command mode, not a daemon detach (daemon detach remains the Ctrl+C Quit modal).



## [0.29.1] - 2026-06-14

### Fixed

- **Dashboard Switch on Single-Agent Card Create**
  Creating a new single-agent card now always switches the view to the Dashboard tab with the new card selected and focused, regardless of which tab was active when the new-deck dialog was opened.
  Previously, pressing `Ctrl+N` from an orchestration or mode tab and creating a plain card (no mode, no orchestration) left the active tab unchanged — the new card appeared on the Dashboard, but the view stayed in the orchestration or mode tab. Users had to manually switch back to the Dashboard to see and interact with their newly created card. Orchestration and mode creation were already unaffected (they switch to their own new tab on creation); this fix closes the gap for single-agent cards.
  The leaving tab's live focus is captured before the switch, so switching back to the orchestration or mode tab afterward restores its previous focus correctly.
- **Orchestration Tab Name Survives Detach/Reattach**
  The custom name you type when launching an orchestration now persists across detach and reattach. Previously the typed name appeared correctly on creation, but reattaching to the orchestration — including after reconnecting to a remote daemon — silently reverted the tab title to the name from the TOML config (or the working-directory basename for unnamed orchestrations), discarding whatever you had entered.
  The name now travels with each role through the daemon, so reattaching restores the title you chose. Orchestrations launched without a custom name continue to use the config name, with the existing cwd-basename fallback.



## [0.29.0] - 2026-06-14

### Added

- **Experimental Feature Flag**
  In-flight features can now be gated behind a single `experimental` flag so work-in-progress surfaces merge to `main` without appearing in normal use.
  The flag is **off by default**. Enable it two ways:
  - **Config file** — add `[features] experimental = true` to `.dot-agent-deck.toml` while the deck is running; the flag reloads within ~2 seconds with no restart required.
  - **Environment variable** — set `DOT_AGENT_DECK_EXPERIMENTAL=1` before starting the deck; the env value always wins over the config file.
  Each flag-gated feature declares a thin per-feature wrapper (`features::show_<feature>()`) so a single `grep` finds every call site, enabling a mechanical, diff-clean removal when the feature graduates to fully visible.
  See the [Experimental Flag](https://github.com/vfarcic/dot-agent-deck/blob/main/docs/develop/experimental-flag.md) reference for configuration details and the graduation workflow.
- **Remote Connect Survives Sleep/Wake**
  Remote sessions no longer freeze when your laptop sleeps. Previously, closing the lid dropped the underlying TCP connection silently — the SSH client had no way to detect the dead socket, so on wake the TUI was frozen, keystrokes went nowhere, and the only escape was closing the terminal tab entirely and reconnecting manually.
  Two layers work together to eliminate this: SSH keepalive probing on the live session detects the dropped connection within roughly 45 seconds of wake and terminates the session cleanly (`ServerAliveInterval=15`, `ServerAliveCountMax=3`). An automatic reconnect loop then re-probes the remote host and re-spawns the session transparently — you see a brief "reconnecting to `<name>`…" message and land back in your running agents without having to touch anything. Because the remote daemon is persistent and agent state is preserved across reconnects, you re-attach to the same agents you were already working with.
  Reconnection is bounded: if the host is unreachable after five attempts (`MAX_CONNECT_ATTEMPTS=5`), the session exits cleanly and restores the local terminal to a sane state. Only SSH transport failures (exit code 255) trigger reconnection — a deliberate quit, Ctrl-C, or remote TUI crash exits immediately as before.
  See the [Remote Environments guide](https://devopstoolkit.ai/docs/ui/remote-environments) for details on the keepalive tuning and retry behaviour.

### Fixed

- **Deck Selection Highlight Now Clears on Tab Switch**
  The blue selection highlight on Dashboard and Orchestration deck cards no longer persists after switching away to another tab. Previously, the highlight stayed active even after switching tabs, creating a visual/functional mismatch where a card appeared ready to act on but wasn't.
  The selection highlight is now active only when the user has explicitly engaged with it. Switching away from or back to a deck deactivates the highlight. To reactivate: press `j` to jump to the first card, `k` to jump to the last card, `1`–`9` to select a numbered card directly, or `Enter` to restore the previously-selected card. Once active, `j`/`k` navigate normally and the highlight persists until the next tab switch. The highlight also reactivates when a deck pane genuinely becomes focused (a real focus change), but not merely from the focus restored by switching back to the tab. This behavior applies consistently to both the Dashboard and Orchestration decks.
- **Orchestration deck card heights right-sized to content**
  Orchestration deck cards previously reserved more vertical space than their content filled, showing 1–2 blank rows at the bottom of each card and forcing unnecessary scrolling with 7 or more decks.
  Card heights are now derived from the exact lines `render_session_card` emits: Compact cards shrink from 7 to 5 rows (wide) or 8 to 6 rows (narrow), eliminating the 2-row gap that caused the most visible waste. Normal and Spacious tiers are also tightened by 1 row each. With the corrected Compact height, 7 decks in the single-column orchestration panel fit in ~35 rows — well within a typical ~48-row card area — so all decks are visible without scrolling. Scrolling still engages when the deck count genuinely exceeds the available space.
- **Worker context now resets on delegation when the directory name differs from the orchestration name**
  When an orchestration ran from a directory whose name differed from the `name` declared in its `.dot-agent-deck.toml` — most commonly a git **worktree** (e.g. `myproject-feature-x` vs a config named `myproject`) — delegating a task to a worker silently failed to restart that worker. The worker carried over the full conversation from its previous tasks instead of cold-starting, which let context accumulate and pushed agents into unnecessary context compaction. Orchestrations run from a directory whose name matched the config name (the common case) were unaffected, which is why the problem only surfaced in worktrees.
  The cause was that the orchestration's internal identity and its display title were the same value: opening an orchestration seeded the title from the directory basename and that value overwrote the configured orchestration name, so the per-delegate role lookup (which re-reads the on-disk config) no longer matched and the clear/restart step was skipped.
  Identity and title are now decoupled. Delegate routing and the per-role clear/restart decision use the orchestration `name` from `.dot-agent-deck.toml`, disambiguated per working directory, while the tab title still shows the name you entered when opening the orchestration. Worker roles using the default `clear = true` now reliably cold-start on each delegation regardless of the directory name; roles set to `clear = false` continue to retain their context as before.
- **Rendering Contract — Eliminate Recurring Visual Glitches**
  Fixes a class of recurring visual rendering bugs — scrambled text near the bottom of panes, empty space on the right edge after resize, and short-lived artefacts on tab switch or mode change — by replacing scattered, symptom-level patches with an explicit four-invariant rendering contract.
  The render path now enforces: a single layout pass per frame (`compute_frame_layout` / `FrameLayout`) so no render function computes its own rects; layout-driven PTY resize via a single `resize_panes_to_layout` call that replaced all ad hoc per-site `resize_pane_pty` calls in tab open/close, mode switch, pane recreation, and `Event::Resize`; and `TerminalWidget` rendering 1:1 against its assigned area with no `min(area, screen)` clamp or cursor-anchored row windowing. Resize sequencing is fixed so compute, resize, and render always share the same live area within a frame.
  A `debug_assert` guards the PTY-size-equals-area invariant in debug builds; release builds log once on mismatch and fall back to `min` rather than panicking. Six failure-mode reproducers are added to the test catalog and run in CI. Validated across 1 039 e2e scenarios with zero contract violations.
  See the rendering contract (`docs/develop/rendering-contract.md`) in the repository for the full invariant specification.



## [0.28.0] - 2026-06-12

### Added

- **Scheduled Prompt Dispatch**
  Schedule prompts to run on a cron and land in the deck automatically. Previously, every recurring agent task was a manual ritual: open a terminal at the right time, navigate to the right directory, paste the prompt, and wait. Now the deck's daemon fires the prompt on your behalf — whether you're watching or not.
  Schedule any prompt with a standard five-field cron expression (`0 9 * * MON-FRI`) in a global config file at `~/.config/dot-agent-deck/schedules.toml`. Each entry specifies a working directory, the agent command, and the prompt to deliver. At fire time, the daemon reads the target directory's `.dot-agent-deck.toml`: if it defines an orchestration, the prompt goes to the orchestrator role; otherwise a single-agent card is opened. New working directories are created automatically. A reuse-by-default policy means repeated fires update the same tab rather than accumulating new ones; `new_tab_per_fire = true` opts into per-fire history when you need it.
  Three ways to manage schedules: converse with a seeded agent via the new **"schedule" mode** in the new-deck dialog (best for crafting multi-line prompts and testing them live), use the `dot-agent-deck schedule add|update|remove|list|enable|disable|run-now|reload` CLI directly, or hand-edit the TOML and run `schedule reload`. A **Scheduled Tasks manager dialog** (press `S`) lists all schedules with their status and next-fire time, and lets you add, edit, delete, or run a task immediately without leaving the deck. The daemon stays alive between fires even with no active agents, so a daily task never silently misses its window because the daemon GC'd itself overnight.
  See the [Scheduled Tasks guide](https://devopstoolkit.ai/docs/ui/scheduled-tasks) for the full config reference, authoring walkthrough, and daemon-supervision recipe for unattended (always-on) use.
- **Customizable Keybindings**
  All TUI keyboard shortcuts are now remappable via a TOML config file, resolving conflicts with terminal emulators that intercept keys like `Alt+n` or `Alt+w` and accommodating personal preferences, accessibility needs, and international keyboard layouts.
  Create `~/.config/dot-agent-deck/keybindings.toml` (or point `$DOT_AGENT_DECK_KEYBINDINGS` at any path) and override only what you need—defaults apply for everything else. Global actions (`dashboard`, `new_pane`, `close_pane`, `toggle_layout`, `jump_1`–`jump_9`) and dashboard navigation keys (`move_down`, `move_up`, `filter`, `rename`, `help`, `approve_permission`, etc.) are all remappable. Empty a binding (`new_pane = ""`) to unbind it entirely. Conflicting assignments produce a warning on stderr and the first-defined binding wins. `Ctrl+C` always opens the quit flow and cannot be remapped. The help overlay (`?`) and hints bar update dynamically to reflect your active bindings.
  See [docs/keyboard-shortcuts.md](docs/keyboard-shortcuts.md) for the full defaults table and configuration format.
- **Mouse Parity for Keyboard Actions**
  Every keyboard action in dot-agent-deck is now reachable by mouse click. Previously, users had to know the right keystroke for creating panes, switching tabs, dismissing modals, navigating the dashboard, picking directories, and filling out forms — the mouse worked only for click-to-focus and scrolling. Now every action has a visible, clickable affordance with its keyboard shortcut shown inline (e.g., `[New Pane Ctrl+N]`), making the UI self-documenting for mouse-first users while teaching keyboard shortcuts to those learning the key set.
  A persistent global button bar at the bottom of every screen exposes New Pane, Close, Toggle Layout, Help, and Quit. The tab strip gains click-to-switch on all tabs (Dashboard, Mode, Orchestration) and a `[×]` close affordance on Mode and Orchestration tabs. Dashboard cards are now clickable (single-click to select, double-click to enter PaneInput), with explicit Filter, Rename, and Generate-config buttons alongside the existing keystrokes. Modal dialogs (quit-confirm, config-gen, star-prompt, help overlay) each gain clickable Yes/No/Cancel buttons that work alongside the existing keyboard selection flow. Inline-edit rows (filter, rename) expose Apply/Cancel buttons; PaneInput mode shows a `[Detach Ctrl+D]` affordance. The directory picker and new-pane form are fully clickable — rows, breadcrumbs, mode chips, and Submit/Cancel buttons.
  All existing keyboard shortcuts and prior mouse behaviors (click-to-focus pane, scroll forwarding, text selection, Ctrl+click hyperlinks, child-app mouse pass-through) are preserved unchanged. Clicks are hit-tested against button regions first and fall through to existing pane logic on a miss. The `?` help overlay and docs have been updated to reflect the new button bar.
  See the [dot-agent-deck documentation](https://devopstoolkit.ai/docs/ui) for the full keyboard/mouse reference.

### Fixed

- **Light Terminal Background Compatibility**
  The dashboard is now readable on light terminal backgrounds (e.g., Solarized Light, macOS Terminal white). Previously the canvas, overlays, and prompt dialogs were painted with a hardcoded dark background — a black slab over a light theme — and neutral text used hardcoded `White`/`Gray` colors that were invisible or extremely low-contrast.
  Every color the dashboard emits is now expressed in the terminal's own frame of reference, so it inherits whatever foreground and background the terminal is already using:
  - Backgrounds are left unpainted (`Color::Reset`) so the terminal's own background shows through — no more black slab. This applies to the canvas, the tab bar, every overlay/prompt dialog, and the embedded terminal panes.
  - Primary text uses the terminal's default foreground; secondary and muted text dim that same foreground rather than hardcoding a gray.
  - Selection and active-tab highlights invert in place (`REVERSED`) instead of painting an absolute tint.
  - Status and accent colors (Working, Error, Thinking, borders, badges) stay as named ANSI colors, which the terminal's theme already remaps.
  Because styling is now terminal-relative, the previous OSC-11 background detection, the light/dark `ColorPalette`, and the `--theme` flag / `theme` config key are no longer needed and have been removed. Readability is pinned by structural L1 tests that assert no overlay or card paints an absolute `Color::Rgb` background and that selection highlights use `REVERSED`.
- **Per-Tab Selection Memory**
  Each tab now remembers its own deck card and pane focus independently, fixing a bug where switching tabs would overwrite or lose your selection in other tabs.
  Previously, switching between Dashboard, Mode, and Orchestration tabs caused selections to bleed across tabs: whichever deck card or side pane was last focused anywhere in the app would appear selected when landing on any other tab. After a tab switch, keystrokes could silently reach a pane in a tab you were no longer viewing. The more tabs open, the more disorienting the behaviour.
  Tab selection state is now keyed on stable IDs (session id for dashboard cards, pane id for Mode and Orchestration side panes) rather than positional indices, so selections survive filter changes, sort changes, session restarts, and reactive pane recreation. On every tab switch, the destination tab's remembered selection is restored—both the visual highlight and the actual keyboard focus. When a remembered session or pane no longer exists (session ended, pane closed), the tab falls back cleanly to the first available item rather than pointing at stale state.

### Miscellaneous

- **Test daemons no longer leak to PID 1 on abrupt exit**
  The integration/E2E harness disables daemon idle-shutdown for determinism and only cleaned daemons up in `Drop`, which never runs on `SIGKILL` / panic-abort / nextest-timeout / `Ctrl-C`. Such daemons orphaned to `init` (PID 1) and kept running for hours after a test run died abruptly.
  Two env-gated, test-only daemon self-defense nets now prevent this (both OFF by default, so detached/lazy-spawned production daemons are unaffected):
  - `DOT_AGENT_DECK_EXIT_WHEN_ORPHANED` — the daemon captures its parent pid at startup and gracefully shuts down once it is orphaned (parent becomes PID 1 or otherwise changes).
  - `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` — a hard backstop that self-exits after N seconds regardless, covering detached daemons the orphan watchdog can't help.
  Harness `Drop` paths additionally reap the whole process group so a daemon's spawned agents go down with it. New L2 test `lifecycle/orphan-exit/001` proves an orphaned, idle-disabled daemon self-exits within seconds.



## [0.27.1] - 2026-06-06

### Fixed

- **Scrollback No Longer Scrambled After Reconnect**
  Reconnecting to a `dot-agent-deck connect` session no longer corrupts scrollback in agent panes. Previously, every reconnect caused overlapping text, narrow vertical text strips on the right edge, and duplicated inner-TUI status lines when scrolling up to read prior model output — sometimes making history unreadable for the rest of the session.
  The root cause was that the client's vt100 parser was always initialized at a hard-coded 24×80 during snapshot replay, even when the daemon's PTY was opened at the terminal's real dimensions (e.g. 120 columns × 40 rows). Absolute cursor-position and line-wrap sequences sized for the real geometry were mis-parsed at the wrong width, permanently baking the corruption into scrollback. The live viewport recovered automatically on SIGWINCH redraw, hiding the bug until the user scrolled up.
  The daemon now records each agent's current PTY dimensions and reports them via `list_agents` (added `rows`/`cols` to `AgentRecord` with `serde(default)` for backward compatibility with older daemons). The client initializes the vt100 parser at those dimensions before replaying the snapshot, with a defensive clamp and fallback to 24×80 when talking to a legacy daemon. The daemon also clears its scrollback ring on every real PTY resize, ensuring every snapshot delivered to a reconnecting client covers a single consistent (rows, cols) epoch.
- **Orchestrator spawn-time role prompt now submits reliably**
  Fixes the orchestrator's spawn-time role prompt failing to submit in slower or remote-daemon environments. After PRD #100's interleave-race fix shipped in v0.27.0, the role prompt payload still arrived in the orchestrator's input box as un-dispatched text — the trailing Enter was interpreted as a literal newline rather than a submit.
  The root cause is a timing race: Claude Code's `SessionStart` hook fires early in its boot sequence, before its TUI input is ready to interpret `\r` as submit. The spawn-time path, which sends the role prompt immediately after detecting `SessionStart`, was writing into a pane that had not yet entered submit-aware mode. The fix adds a 500ms readiness buffer (`SPAWN_TIME_READINESS_BUFFER`) between `SessionStart` detection and the role-prompt write, giving Claude Code's TUI input time to reach a submit-ready state. A regression test (`tests/spawn_time_role_prompt_submit_after_session_start.rs`) drives a Python slow-readiness agent stub and toggle-verifies the fix: the test fails at `BUFFER=0` and passes at `BUFFER=500`.
  Daemon-side `RUST_LOG=pane_write=trace`-gated trace instrumentation is also included, adding four trace events across the PTY write path (payload, submit terminator, notice terminator, and `STREAM_IN` forwarding). Each event carries `pane_id` and `agent_id` for future cross-path debugging.

### Miscellaneous

- **TUI Testing Harness**
  A two-layer automated test harness for the dot-agent-deck TUI, replacing manual regression testing with reproducible, reviewable assertions.
  **L1 (in-process)** uses ratatui `TestBackend` + `insta` snapshots for pure widget and layout regressions — no subprocess, millisecond-fast, runs on every `cargo test-fast`. **L2 (end-to-end)** spawns the real binary in an isolated PTY via `portable-pty`, parses the rendered screen through a `vt100` parser, and asserts on the deck's observable state (panes, statuses, focus, hook delivery, attach stream presence) — never on agent text. L2 tests are gated by `--features e2e` and run as `cargo test-e2e` before each release.
  Two seed tests ship with the harness: `dashboard/pane/004` (L1, new-pane appears in layout) and `hooks/delivery/001` (L2, hook fires and deck receives it). A chain-smoke test (`chain-smoke/claude/001`) validates a real Claude Code session end-to-end. Test documentation is auto-generated from `/// Scenario:` comments via `cargo xtask docs --tests`, with `cargo xtask linkage-check` enforcing that every `#[spec(...)]` annotation references a valid catalog entry.



## [0.27.0] - 2026-05-25

### Added

- **Hide Command Field in New-Pane Form When Orchestration is Selected**
  When opening the new-pane form (`Ctrl+n`) and choosing an orchestration mode, the Command field is now hidden. Since orchestration role panes always use the command from `.dot-agent-deck.toml`, the field served no purpose and only added confusion. Selecting "No mode" or a workspace mode restores the Command field as before. `Tab`/`Shift+Tab` navigation and `Enter` confirmation all work correctly with the hidden field, and the footer hint updates to reflect the available fields.
- **Tab Name Truncation**
  Tab names in the TUI are now truncated to fit the terminal width, so every open tab remains visible and reachable at a glance.
  Previously, when many tabs were open or one tab had a long name (such as a branch-style orchestration name like `dot-agent-deck-prd-110-fix-session-reuse-…`), the `Tabs` bar would clip the right-most tabs entirely off-screen. Those tabs were still reachable via `Tab` / `Shift+Tab` but invisible, making it hard to know which sessions were running.
  Truncation now applies an equal-cap strategy before labels are handed to the widget: each label is allowed at most `floor(available_width / tab_count)` cells (accounting for `" label "` padding and `│` dividers). Labels already shorter than the cap render in full; longer labels are truncated to the cap width with a trailing `…`. The cap recalculates on every frame, so resizing the terminal, opening a tab, closing a tab, or renaming an orchestration session all produce correct widths immediately — no manual refresh needed.

### Fixed

- **Fix Orchestrator Spawn-Time Role Prompt Not Submitting**
  When orchestration starts, the deck automatically injects the initial role prompt into the orchestrator agent's input. The trailing Enter sometimes did not trigger submission — instead, the cursor dropped to a new line inside the input box with the prompt text un-submitted.
  Root cause: the TUI client's spawn-time write used two `KIND_STREAM_IN` frames separated by a 150ms sleep. The daemon's per-agent writer mutex was released between frames, leaving a window during which a concurrent daemon-initiated write (e.g. a sibling worker's work-done feedback) could interleave, fusing its payload and CR into the gap. The orchestrator agent then saw the daemon's CR first — submitting the fused line — and the user's trailing CR landed in an empty input box and was rendered as a newline.
  Fix: a new `WriteAndSubmit` RPC routes the spawn-time injection through the daemon's existing `write_to_pane_and_submit` primitive, which holds the per-agent writer mutex across the full `payload → SUBMIT_DELAY → CR` sequence — the same atomic contract used by the daemon-initiated orchestration-delegate path, which has always worked correctly.



## [0.26.1] - 2026-05-25

### Fixed

- **Orchestration Role Cards Stay Consistent Across Respawn and Reconnect**
  Two follow-on symptoms from the orchestration GA release are fixed. After an F9 delegate respawn (`clear = true`), the orchestration tab no longer shows a duplicate card for the same role — the stale card from the previous agent is now retired when the fresh one starts. And after reconnecting to a daemon with a role whose worker had already exited (typically a `clear = false` agent that finished its workflow), the configured role no longer briefly disappears from the orchestration tab; every role in the config keeps a placeholder card on hydration so the tab layout matches `.dot-agent-deck.toml`.



## [0.26.0] - 2026-05-25

### Added

- **Orchestration — generally available**
  Run multi-agent pipelines where a designated orchestrator coordinates specialist workers — a coder, reviewer, auditor, release agent, or any roles that fit your project. Previously, this capability existed in the codebase but was undocumented and effectively hidden from users.
  Define an `[[orchestrations]]` block in `.dot-agent-deck.toml` with one role marked `start = true` as the orchestrator and the rest as workers. Press `Ctrl+n`, navigate to your project directory, and cycle the **Mode** field to the orchestration name — the deck opens a dedicated tab with a pane for every role. The orchestrator reads a task, delegates work to workers, waits for results, and chains the next step. Workers run independently in their own panes and report back when done. Parallel delegation is supported: the orchestrator can send work to multiple roles simultaneously.
  A built-in role library (coder, reviewer, auditor, tester, documenter, release, researcher) provides starting-point prompts and commands for common workflows. Generate a project-tailored config in one step by pressing `Ctrl+d` → `g` on the dashboard — the agent analyzes your project and proposes roles, commands, and prompt templates. Treat the generated config as a starting point and edit it freely.
  See [Orchestration](https://devopstoolkit.ai/docs/ui/orchestration) for the full configuration reference, examples, and troubleshooting.

### Fixed

- **Stale-Daemon Version Skew Detection and `daemon stop`/`restart` Commands**
  The local TUI now detects when it is connecting to a daemon built from a different commit and exits with a clear error message instead of silently misfiring. Previously, upgrading the `dot-agent-deck` binary while a daemon from the previous build was still running could cause delegate prompts to appear queued in the TUI but never progress — the stale daemon's handler code predated internal role-map schema changes, so orchestration silently no-op'd with no error visible to the user.
  A new `DAD_BUILD_ID` build variable (`<version>-g<short-sha>[-dirty]`, e.g. `0.25.0-g243b049`) is compiled into every binary. On startup, the TUI performs a build-version handshake with the local daemon and, on mismatch, prints a message naming both build IDs and exits non-zero — prompting the user to run `dot-agent-deck daemon stop` before relaunching. The same field is compared on remote connections (`probe_remote_protocol`), where a mismatch points at `remote upgrade` instead.
  Two new CLI subcommands make daemon lifecycle management safe and documented: `dot-agent-deck daemon stop` gracefully shuts down the local daemon (via `SIGTERM`, falling back to `SIGKILL` after 5 s with `--force`), refusing if managed agents are still alive unless `--force` is passed. `dot-agent-deck daemon restart` stops the daemon and lets the next TUI launch re-spawn it. Both commands discover the daemon PID via socket peer credentials (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS) — an OS-level facility that works against any daemon version, including pre-handshake binaries.
  See the updated [Installation](https://devopstoolkit.ai/docs/ui/installation) and [Troubleshooting](https://devopstoolkit.ai/docs/ui/troubleshooting) docs for the recommended upgrade flow and recovery steps.
- **Session Reuse No Longer Hides New Sessions After clear=true Delegate**
  Delegating to a worker with `clear=true` now correctly shows a fresh session card in the TUI dashboard. Previously, the session-reuse guard introduced for opencode deck continuity would remap a respawned agent's new `session_id` back to the old one, making it appear as though no new session had started — even though the daemon had correctly killed and respawned the worker process.
  The reuse logic now checks the `agent_id` carried in each `SessionStart` event before applying the remap. When the same agent restarts naturally within a pane (e.g., an opencode crash or config reload), the session card is reused as before. When a different `agent_id` arrives — which is what happens after a `clear=true` delegate — the reuse is skipped and a new session card is created, so the dashboard accurately reflects the fresh agent run.
  Pre-F9 hook scripts that emit `SessionStart` without an `agent_id` continue to fall through to the reuse path, preserving backward compatibility.
- **Orchestration Tabs Restored on Remote Reconnect**
  Fixes orchestration panes being dumped onto the dashboard tab when a TUI reconnects to a daemon running on a different host (e.g., a laptop TUI reconnecting to a VM daemon). Previously, the hydration code tried to load the config file from the daemon's local path—a path that doesn't exist on the TUI's machine—causing every active orchestration session to silently fall back to the dashboard instead of appearing in its own orchestration tab.
  The TUI now synthesises a minimal `OrchestrationConfig` directly from the metadata already present in the daemon's `list_agents` response (orchestration name, role names, and role indices). The synthesised config is structurally complete, so tabs are rebuilt correctly without needing access to the daemon-side config file. When the config file *is* available locally (same-host connections), it is still used to enrich display-only fields like `description` and `prompt_template`—no regression for local connections.
  The active-tab reset on reconnect is also fixed: after hydrating orchestration tabs the TUI now lands on the first orchestration tab rather than unconditionally snapping back to the dashboard. Users reconnecting to running orchestration sessions will find their tabs where they left them.
  Security hardening included: role indices from the wire are capped at 256 (preventing multi-gigabyte allocation from a malformed daemon response), and role and orchestration names are validated to reject control bytes and ANSI escape sequences that could disrupt terminal rendering or spoof tab labels.



## [0.25.2] - 2026-05-24

### Fixed

- **Orchestration Tab Name Now Reflects User Input**
  Typing a custom name in the new-pane form when launching an orchestration now correctly appears as the tab title. Previously the tab always showed the name from the TOML config (or the working-directory basename fallback), silently discarding whatever was typed in the Name field.
  Leaving the Name field empty continues to use the config name, with the existing cwd-basename fallback for unnamed orchestrations.

## [0.25.1] - 2026-05-24

### Fixed

- **Pre-daemon parity restoration**
  The PRD #76 and PRD #93 transitions to an always-external daemon silently regressed several user-visible behaviors. PRD #92 audited every baseline feature and fixed the gaps. Users get back a deck that behaves consistently with the pre-daemon era.
  **Stop option in the Ctrl+C dialog**. The quit confirmation (Ctrl+d → Ctrl+C) now has three options — Detach (default), Stop, Cancel — instead of the post-PRD-#93 two-option Detach/Cancel. Stop terminates managed agents and exits the daemon, restoring the pre-daemon "one gesture takes everything down" behavior. A secondary y/n confirmation appears only when one or more agents are alive, defaulting to No so accidental presses don't lose work.
  **Working `y` / `n` permission keys**. PRD #18's help overlay has long documented `y` (approve) / `n` (deny) on the dashboard, but no handler ever existed. Now: when a card's selected session is in `WaitingForInput`, pressing `y` on the dashboard approves the permission prompt and `n` denies it — both forwarded to the pane's PTY without needing to focus into the pane. Other statuses: no-op.
  **Ctrl+W close-pane fixes**. Three regressions repaired:
  - *Errors are now visible.* When the daemon's `StopAgent` RPC fails or times out, the pane card stays on the dashboard and the error appears in the status bar so the user can retry. Pre-fix, failed RPCs silently dropped cards while the underlying agents stayed alive in the daemon registry. Orchestration tab and mode-tab group closes now return per-pane outcomes — successful panes close, failed ones stay with their errors surfaced.
  - *Descendant processes are reaped.* Pressing Ctrl+W on a shell-wrapped agent (commands launched via `$SHELL -c <cmd>`) now kills the agent and everything it spawned — language servers, file watchers, child processes. Pre-fix, only the shell wrapper died and its descendants were orphaned to init.
  - *Graceful close with SIGTERM.* Ctrl+W now sends SIGTERM and waits up to 3 seconds before escalating to SIGKILL. Pre-fix, Ctrl+W went straight to SIGKILL — uncatchable — so well-behaved agents had no chance to run cleanup hooks. Daemon-wide Stop (Ctrl+C → Stop above) already had this graceful pattern; single-pane Ctrl+W now matches.
  **Orchestration `clear` semantics restored**. The `clear` field on `[[orchestrations.roles]]` in `.dot-agent-deck.toml` is honored again. Workers configured with `clear = true` (the default) get respawned (kill + fresh spawn) before each delegate, restoring the pre-daemon contract that an orchestration role starts each task with a fresh agent context. The `release` role's explicit `clear = false` continues to opt out and preserve scrollback for the release-flow walkthrough.
  **Live pane view across respawns**. When a worker is respawned (the `clear = true` flow above), the dashboard pane view now transitions to the new agent's output without requiring a manual detach and re-attach. The TUI auto-renews its per-pane stream subscription with an exponential backoff that covers production agents whose startup gap can run several seconds.
  **Documentation accuracy**. `docs/configuration.md` and `docs/remote-requirements.md` now correctly reflect the `/tmp/dot-agent-deck-{uid}.sock` socket path with the per-user disambiguation suffix, instead of the stale `/tmp/dot-agent-deck.sock`.



## [0.25.0] - 2026-05-22

### Added

- **Always-External Daemon**
  The deck now always uses an external daemon process, eliminating the architectural split between local and remote modes. This unifies the codebase and ensures all code paths are exercised by day-to-day development.
  **Agent persistence across deck restarts**: When you detach from the deck or quit the TUI, your running agents continue to execute in the daemon. Reconnecting with `dot-agent-deck` picks them back up in the same state—the same as remote behavior. You can detach, rebuild the deck binary, and reconnect without losing in-flight work.
  **Daemon lifecycle management**: The daemon spawns automatically when you first run `dot-agent-deck` if one is not already running. It remains alive as long as agents are running. Once all agents finish and the last client disconnects, the daemon exits after an idle timeout (default 30 seconds). This timeout is configurable via the `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` environment variable.
  See the [Getting Started Guide](https://devopstoolkit.ai/docs/ui/getting-started) for details on daemon lifecycle and the detach workflow.



## [0.25.0-alpha.0] - 2026-05-20

### Added

- **Remote Agent Environments (Preview)**
  **Status: experimental.** APIs, CLI flags, and the attach-socket wire format may
  change without notice in subsequent pre-releases. Not yet documented on
  [devopstoolkit.ai](https://devopstoolkit.ai/) — full docs and graduation to
  stable land with [PRD #87](https://github.com/vfarcic/dot-agent-deck/issues/87).
  Run `dot-agent-deck` against agents on a remote host over `ssh -t`. The TUI
  launches against a per-host daemon that is lazy-spawned on first attach, so
  agents survive your laptop's network drops and your `Ctrl+C` — the next
  `dot-agent-deck connect <name>` re-attaches to the same sessions with scrollback
  intact.
  Workflow:
  ```
  dot-agent-deck remote add lab user@host        # one-time registration
  dot-agent-deck connect lab                     # opens the TUI against the remote
  dot-agent-deck remote upgrade lab              # roll the remote binary forward
  ```
  Strict protocol-version handshake on `connect` (M2.21) fails fast on
  laptop/remote skew with a directional upgrade hint, so an out-of-sync pair
  can't silently disable live dashboard updates.
  Known scope for this preview:
  - Docs site (`devopstoolkit.ai`) is not yet wired — PRD #87 owns the
    publication moment.
  - `--theme` is not propagated through the `ssh -t` wrapper — deferred to
    PRD #93.
  - The "deck always uses an external daemon" unification is in flight under
    PRD #93; until it lands, local and remote modes are two separate code paths.
  To try the preview on a release:
  ```
  # laptop (Homebrew, opt-in parallel formula)
  brew install vfarcic/tap/dot-agent-deck-beta
  dot-agent-deck-beta --help
  # remote host (explicit version pin via the existing flow)
  dot-agent-deck remote upgrade <name> --version 0.25.0-alpha.0
  ```



## [0.24.7] - 2026-05-13

### Fixed

- **Normal-Mode Hints Reappear Immediately After Exiting PaneInput**
  Pressing `Ctrl+d` to leave PaneInput mode now restores the Normal-mode navigation hints right away. Previously, the "PaneInput mode — type to interact, Ctrl+d for dashboard" status message lingered for up to 15 seconds and hid the hint bar, leaving users with no visual confirmation they were back in command mode.
  The mode-transition handler now clears the status message in addition to switching `ui.mode`, so the hint bar (`?: help`, `j/k`, `Ctrl+n`, `Ctrl+d: dashboard`, etc.) is visible immediately.



## [0.24.6] - 2026-05-13

### Fixed

- **Permission Prompt Status No Longer Flickers During Concurrent Subagent Tools**
  Session cards now keep their "Needs Input" status when a concurrent subagent fires a tool event. Previously, a subagent's `PreToolUse` would flip the card back to "Working" while the user was looking at an active permission prompt — making the prompt easy to miss when several agents were running in parallel.
  The dashboard now preserves `WaitingForInput` across `ToolStart` events from concurrent subagents while still updating the active-tool display, so the prompt card stays visible until the user responds.



## [0.24.5] - 2026-05-06

### Fixed

  - **Mode tabs not restored on `--continue`**
    `dot-agent-deck --continue` now restores mode tabs alongside plain dashboard panes. Previously, only plain panes came back: when you exited with mode tabs open, the session snapshot was written *after* the close-tab teardown loop had already unregistered the mode-tab agent panes from `pane_metadata`, so the `mode = Some(...)` field was stripped from the saved `session.toml` and the existing restore path never had data to act on. The snapshot now runs before teardown, capturing every live pane's mode metadata while it's still registered.
    After restore, each mode tab reappears in the tab bar with its original name, the agent pane re-runs its configured command (e.g. `claude`), and all side panes from `.dot-agent-deck.toml` come back running their configured commands. If the project's `.dot-agent-deck.toml` was deleted or the mode renamed between exit and restore, a clear warning is shown and the pane falls back to a plain dashboard pane instead of failing silently. Old `session.toml` files without a `mode` field continue to load without error. This resolves the bug noted in v0.24.2's docs accuracy pass — the "Mode tabs are restored on `--continue`" claim is now actually true. (Orchestration tabs remain unrestored — tracked separately in PRD #74.)



## [0.24.4] - 2026-05-05

### Fixed

- **Docs site port leak in directory redirects**
  Clicking links like `/docs/installation` (no trailing slash) on the public docs site no longer bounces users to a non-routable `http://agent-deck.devopstoolkit.ai:8080/...` URL. The docs container now ships a custom nginx config that disables `absolute_redirect` and `port_in_redirect`, so directory 301 redirects emit relative `Location` headers and the upstream Gateway's host, scheme, and port are preserved.
- **OpenCode worker status updates not appearing on dashboard cards**
  On systems where OpenCode 1.x is installed under the XDG layout (`~/.config/opencode/` instead of legacy `~/.opencode/`), the dashboard's auto-installer never wrote its plugin, so `session.*` and `tool.execute.*` events from OpenCode workers never reached the daemon and card statuses stayed frozen on their initial state. The installer now resolves the active OpenCode root by checking XDG first (`$XDG_CONFIG_HOME/opencode`, defaulting to `~/.config/opencode`) and falling back to `~/.opencode`. Explicit `dot-agent-deck hooks install --agent opencode` targets the detected root or the XDG default; uninstall sweeps both layouts.



## [0.24.2] - 2026-04-28

### Documentation

  Restructure the docs site with annotated screenshots, accuracy fixes, and a richer home page. The landing page gains a "Why Agent Deck" narrative, design principles, and tabbed installation instructions for macOS / Linux / Windows (WSL). Getting Started, Session Management, and Workspace Modes pages now include screenshots illustrating the dashboard layout, session card details, and mode tabs in action. Several long-standing inaccuracies were corrected: `Ctrl+c` quit-dialog behavior, `Ctrl+w` semantics on dashboard vs. mode tabs, command-mode requirements for `Tab` / `Shift+Tab` / `j` / `k`, and the "Mode tabs are restored on `--continue`" claim (now tracked as a bug — see issues #68 and #69).



## [0.24.1] - 2026-04-27

### Fixed

  Release binaries now report the correct version. Previously the release workflow's build job used a shallow checkout without tag refs, causing `git describe` in `build.rs` to fall back to `CARGO_PKG_VERSION` (`0.1.0`) — so installed binaries always reported `v0.1.0` and the update banner appeared even on the latest release.



## [0.24.0] - 2026-04-27

### Added

- **Customizable Config-Gen Prompt and Orchestration Role Library**
  The Ctrl+G config-generation prompt and orchestration role definitions now live in editable asset files instead of being hardcoded in the binary. The prompt template is at `assets/config_gen_prompt.md` and the role library (coder, reviewer, auditor, tester, documenter, release, researcher) is at `assets/roles.toml`. Both are bundled at compile time, so behavior is unchanged for users who don't customize, but contributors can iterate on the prompt without touching Rust source.
  The default prompt has also been improved: it now teaches the AI to discover project-defined agent launchers (devbox/npm/task scripts, `.claude/`/`opencode.json` configs, etc.), match them to roles by semantic intent, record the full invocation form (e.g. `devbox run agent-big`, never the bare script name), and propose a dedicated `release` role by default whenever the project has release-flow signals — with explicit context-handoff guidance for the orchestrator so workers cold-starting with no shared scratchpad still receive the file paths and prior findings they need.
  The bundled `.dot-agent-deck.toml` reflects these defaults: a `release` role with `clear = false` so it can resume after CI flakes, and a context-handoff section in the orchestrator's `prompt_template`.

### Fixed

- **Reliable Prompt Submission to Agent Panes**
  Prompts written to agent panes now self-submit reliably instead of sitting in the agent's input buffer waiting for a manual Enter. Multi-line prompts are wrapped in bracketed paste so embedded newlines stay as input rather than triggering a premature submit, and a brief delay between the payload and the trailing carriage return makes agent CLIs (Claude Code, opencode) honor it as Enter rather than absorbing it as a newline-in-input.
  This affects two flows: pressing Ctrl+G to generate a `.dot-agent-deck.toml` config, and orchestration startup where the orchestrator's bootstrap prompt is injected into its agent pane. Both previously left the prompt un-submitted in some cases — the orchestration path additionally fused the role launch command into the prompt buffer because the role command was being written twice (once when the pane was spawned, once again after resize). The duplicate write has been removed.
  Status-bar messages now stay visible for 15 seconds instead of 3, so wrapped error messages such as "Orchestration failed: …" remain readable.
- **Docs Pod Readiness Probe Failure on Startup**
  The docs deployment's readiness and liveness probes now include a 5-second initial delay, preventing transient "connection refused" failures during pod startup. Previously, probes fired immediately before nginx had finished initializing and bound to port 8080, causing unhealthy pod events on every new pod creation.



## [0.23.0] - 2026-04-23

### Added

  Clickable hyperlinks in embedded terminal panes. Tools like Claude Code emit OSC 8 hyperlink sequences for URLs — these are now parsed and tracked so that Ctrl+click on a link row opens the URL in your default browser.



## [0.22.1] - 2026-04-22

### Fixed

  ### Fixed
  - **Mode tab agent pane leak on close**
    Closing a mode tab (Ctrl+W) now properly closes the agent pane's embedded PTY. Previously, `close_tab()` only closed persistent and reactive panes via `deactivate_mode()`, leaving the agent pane orphaned in the embedded pane controller. These orphaned panes accumulated on the dashboard's right-side terminal pane list each time a mode tab was closed and reopened.



## [0.22.0] - 2026-04-21

### Added

  Add multi-role agent orchestration system that enables coordinated multi-agent workflows with a dedicated orchestrator agent driving delegation decisions. Supports parallel fan-out, worker context isolation, and interactive panes.



## [0.21.0] - 2026-04-15

### Added

- **Improved Help Overlay**
  The `?` help overlay now shows every keybinding regardless of context, laid out in two columns for easier scanning. Previously, the Tab Navigation section was hidden whenever the dashboard tab was active — making tab shortcuts undiscoverable for users who hadn't yet opened a mode tab.
  The overlay now includes Mode Tab (in-tab pane navigation) and Directory Picker sections that were documented elsewhere but missing from the in-app help. All section headers use a single accent color for visual consistency.
  See the [Keyboard Shortcuts](https://agent-deck.devopstoolkit.ai/keyboard-shortcuts) reference for the full list.



## [0.20.3] - 2026-04-13

### Documentation

- **Troubleshooting Documentation**
  Added troubleshooting guide for Ghostty terminal users experiencing Shift+Enter not creating newlines when using Claude Code or other AI coding agents inside dot-agent-deck.
  Documents the root cause (Ghostty intercepts Shift+Enter when mouse capture is enabled) and provides the configuration solution using CSI u format keybind: `keybind = shift+enter=csi:13;2u`
  See the [Troubleshooting Guide](https://agent-deck.devopstoolkit.ai/troubleshooting) for complete instructions.



## [0.17.1] - 2026-04-13

### Documentation

- **Troubleshooting Documentation**
  Added troubleshooting guide for Ghostty terminal users experiencing Shift+Enter not creating newlines when using Claude Code or other AI coding agents inside dot-agent-deck.
  Documents the root cause (Ghostty intercepts Shift+Enter when mouse capture is enabled) and provides the configuration solution using CSI u format keybind: `keybind = shift+enter=csi:13;2u`
  See the [Troubleshooting Guide](https://agent-deck.devopstoolkit.ai/troubleshooting) for complete instructions.



## [0.20.0] - 2026-04-11

### Fixed

- **Mode Tab Fixes and Enhancements**
  A batch of fixes addressing mode tab usability issues discovered during real-world usage.
  ### Text Wrapping in Agent Pane
  The agent pane (Claude Code) in mode tabs now wraps text correctly at the pane boundary. Previously, text could extend beyond the visible area because the PTY was sized before the process started, or because switching tabs didn't update PTY dimensions. Three root causes were fixed:
  - **Agent pane command now starts after PTY resize** — the agent pane is created as an empty shell, resized to the correct 50% width, and only then receives the command. The mode's `init_command` (e.g., `devbox shell`) is also sent to the agent pane.
  - **Ctrl+t layout toggle uses correct width** — was hardcoded to 67% (dashboard width) for all panes; now uses 50% for mode tabs.
  - **Tab switching resizes PTYs** — switching between dashboard (67%) and mode tabs (50%) now triggers a PTY resize so processes see the correct terminal width immediately.
  ### Mode Tab Session Restore (`--continue`)
  Mode tabs are now fully restored when starting with `--continue`. Each saved pane records its mode name, and on restore the app looks up the mode config from the project's `.dot-agent-deck.toml` to recreate the full mode tab with agent and side panes. Falls back to a plain dashboard pane if the mode config is missing. The app always starts on the dashboard after restore for a better overview.
  ### Pane Navigation
  - **Up/Down arrows cycle through all panes** including the agent pane, not just side panes. Down now wraps from the last side pane back to the agent.
  - **Focus highlight syncs correctly** — navigating with j/k/Up/Down now updates the embedded controller's focus, fixing a bug where a previously-focused side pane kept its cyan border even when the agent pane was selected.
  ### Reactive Pane Prompt Suppression
  Reactive (rule-triggered) panes now hide the shell prompt (`PS1`/`PS2`/`PROMPT`) so automated command output appears cleanly without prompt clutter. When entering a reactive pane manually (via `Enter`), a minimal `$ ` prompt is restored. Leaving with `Ctrl+d` re-suppresses it. The screen is cleared after prompt changes to keep output clean.
  ### Terminal Widget Rendering
  Fixed a rendering bug where panes (especially the Clippy watch pane) would show only the last line of output. The viewport anchor now uses the cursor position instead of scanning for the last row with content, which was fooled by stray characters from shell initialization.
  ### Config Generation Hint
  The persistent "g: generate .dot-agent-deck.toml" hint was removed from dashboard cards. Instead, a yellow italic tip appears contextually in the new-pane form when no modes are configured: "Tip: press g on dashboard to create modes".



## [0.19.0] - 2026-04-11

### Added

- **AI-Generated ASCII Art for Idle Dashboard Cards**
  Idle dashboard cards now display funny, context-aware ASCII art instead of a static status display. When an agent session goes idle for more than 5 minutes (configurable), dot-agent-deck calls a lightweight LLM to generate humorous ASCII art based on the session's prompts and final response, then animates it directly in the dashboard card.
  The `dot-agent-deck ascii` CLI subcommand provides standalone art generation — pipe in prompts and get ASCII art back, useful for scripting or quick demos. The dashboard integration captures both first and last prompts to give the LLM full narrative context, producing art that reflects what the agent actually worked on. Multi-frame animations cycle on the dashboard tick loop, and a generate-validate-retry mechanism (up to 3 attempts) ensures broken art never reaches the screen — falling back to the flashing-dot indicator if needed. Art only renders in Spacious card density mode to avoid truncation artifacts.
  Enable the feature with `dot-agent-deck config set idle_art.enabled true` and set `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY` for OpenAI, or use Ollama for zero-cost local generation). Configure provider and model in `[idle_art]` section of `.dot-agent-deck.toml`.
  See the [Configuration Guide](https://devopstoolkit.ai/docs/dot-agent-deck/configuration) for setup details.



## [0.18.0] - 2026-04-10

### Added

- **Extensible Modes System**
  Workspace modes transform dot-agent-deck from a multi-agent dashboard into a focused development environment. When AI agents execute commands, relevant output now appears in dedicated side panes alongside the agent instead of being buried in the conversation.
  Each mode is defined in a per-project `.dot-agent-deck.toml` config file. Modes create tab-based workspaces with an agent pane on the left and configurable side panes on the right in a 50/50 layout. Side panes come in two types: persistent panes run predefined commands immediately (e.g., `cargo watch -x test`, `kubectl get pods -w`), while reactive panes populate automatically when the agent executes commands matching user-defined regex rules (e.g., `kubectl describe`, `terraform plan`). Watch rules periodically re-execute commands via the built-in `dot-agent-deck watch` subcommand for live-updating output.
  Mode activation is integrated into the new-agent flow: press `Ctrl+n`, select a directory, and if a `.dot-agent-deck.toml` exists, cycle through available modes with arrow keys in the unified form. Tab navigation uses `Tab`/`Shift+Tab`, arrow keys, or `h`/`l` for vim users. Side panes support keyboard focus (`j`/`k`), click-to-focus, and full shell interaction via `Enter` on a focused pane. The `dot-agent-deck init` command scaffolds new config files, and `dot-agent-deck validate` checks config correctness. An agent-driven config generation flow (`g` on dashboard cards) analyzes the project and proposes a config interactively.
  See the [Workspace Modes Guide](https://devopstoolkit.ai/docs/ui/workspace-modes) for configuration examples and usage details.

### Fixed

- **Dashboard Card for Every Pane**
  Panes created with `Ctrl+n` now immediately display a dashboard card, even before an agent starts. Previously, new panes had no card until an agent emitted its first event, leaving the pane orphaned with no way to switch back to it or close it.
  A placeholder card appears instantly with a "No agent" label and a muted gray border, distinguishing it from active sessions. When an agent starts in the pane, the placeholder transitions seamlessly into a real session card. If a session ends (e.g., via `/clear`), the placeholder is restored so the pane remains visible and reusable. Placeholder sessions are excluded from active session statistics to keep dashboard counts accurate.



## [0.17.0] - 2026-04-06

### Added

- **Auto-Install Hooks on Startup**
  Agent hooks are now automatically installed when the dashboard launches. The CLI detects which agents are present (`~/.claude/` for Claude Code, `~/.opencode/` for OpenCode) and installs hooks for each one. Manual `hooks install` commands are no longer required for normal use.



## [0.16.0] - 2026-04-06

### Added

- **Project Documentation Site**
  A dedicated documentation site built with Docusaurus v3 replaces the monolithic README as the primary resource for users. Previously, all guides, configuration details, and feature overviews lived in the README, making it increasingly difficult to navigate as the project grew.
  The site covers the core user journey: installation, getting started, configuration, keyboard shortcuts, session management, and licensing. A custom homepage provides a polished entry point with feature highlights and quick-start links. The docs directory in the repository serves as the single source of truth — update a Markdown file and the site rebuilds automatically.
  Deployment uses a multi-stage Docker build (Node.js builder + nginx) published to ghcr.io, with a Helm chart for Kubernetes hosting monitored by Argo CD. The CI/CD pipeline builds and publishes the docs image as part of the release workflow on version tags.
  Visit the documentation at [agent-deck.devopstoolkit.ai](https://agent-deck.devopstoolkit.ai).
- **Star Repo Reminder Dialog**
  A non-intrusive dialog now appears every 10 launches encouraging you to star the GitHub repository. The dialog offers three options: press `s` to open the repo in your browser and dismiss permanently, `l` or `Esc` to snooze (reminder returns after 10 more launches), or `d` to permanently hide the dialog.
  State is persisted in `~/.config/dot-agent-deck/star-prompt-state.json` so your preference survives across sessions.
- **Arrow Key Navigation Focuses Panes**
  Arrow keys and vim-style navigation (`j`/`k`/`h`/`l`) on the dashboard now focus the selected session's pane, matching the behavior of the 1-9 number key shortcuts. Previously, arrow keys only moved the card highlight without switching the pane view.

### Fixed

- **"Needs Input" Status Clears After Tool Completion**
  Dashboard session cards no longer remain stuck on "Needs Input" after a permission-gated tool finishes executing. Previously, approving a tool (e.g., a long-running `gcloud` command) left the status as "Needs Input" indefinitely because the `PostToolUse` event did not update the session status. The status now transitions to "Thinking" once the tool completes, accurately reflecting that the agent is processing the result.



## [0.15.0] - 2026-04-05

### Added

- **Light Theme Option for Dashboard**
  The dashboard now adapts to your terminal's color scheme instead of forcing a black background. Previously, the hardcoded black background created a visual mismatch for users running light terminal themes — the dashboard pane appeared as a dark rectangle next to light-themed agent panes.
  On startup, the dashboard auto-detects whether your terminal uses a light or dark background (via OSC 11 query) and selects the appropriate foreground color palette. Accent colors (Cyan, Green, Yellow, Red, Blue, Magenta) remain unchanged since terminals already remap these per-theme. Only neutral text colors (titles, labels, secondary text) switch between themes to maintain readability and visual hierarchy on both light and dark backgrounds.
  Use `--theme auto|light|dark` (default: `auto`) to override auto-detection when needed — useful for tmux or SSH sessions where detection may not work reliably. The theme can also be set in the config file, for example `theme = "auto"`, `theme = "light"`, or `theme = "dark"`. The `dashboard` subcommand has been removed since `dot-agent-deck` defaults to dashboard mode and top-level args now work directly.
- **Session Restore**
  Pick up where you left off with automatic session persistence. Previously, launching `dot-agent-deck` always started from a blank slate, requiring users to re-open every agent pane, reselect directories, re-enter names, and retype commands each time.
  The dashboard now automatically tracks every pane's launch metadata (directory, name, and command) while running and persists the full pane set on exit — no explicit save step required. On the next launch, pass `--continue` to restore all saved panes in their original directories with their original commands and names. Panes that reference directories that no longer exist are skipped with a warning, so partial restores work gracefully without aborting.
  Session state is stored in `~/.config/dot-agent-deck/session.toml` (configurable via the `DOT_AGENT_DECK_SESSION` environment variable). The file uses a simple TOML format that can be edited manually or synced with dotfiles. Start with `dot-agent-deck --continue` or `dot-agent-deck dashboard --continue` to restore your last session.

### Fixed

- **Fix stuck "Needs Input" status**
  Removed the permission approval queue and blocking `PermissionRequest` hook that caused sessions to display "Needs Input" indefinitely. The deck previously registered both a `Notification` and a `PermissionRequest` hook for the same permission event — the blocking hook delayed every permission prompt and left stale entries in the queue when users approved in the terminal instead of the deck. The deck's permission UI (y/n approval) was already disabled, making the blocking hook purely harmful.
  The "Needs Input" status indicator still works correctly via the fire-and-forget `Notification` hook and clears automatically when the agent resumes work.



## [0.14.4] - 2026-04-05

### Fixed

  **Stale "Needs Input" status clears promptly after permission approval**
  The dashboard no longer shows "Needs Input" after the user approves a permission prompt. ToolStart events now match the front of the pending permission queue by tool name, so the permission is dequeued and the status transitions to "Working" immediately. Previously, synthetic permission IDs could never match real tool_use_ids, causing the status to stay stuck until an Idle event arrived.



## [0.14.3] - 2026-04-05

### Fixed

  **Stale "Needs Input" status clears promptly after permission approval**
  The dashboard no longer shows "Needs Input" after the user approves a permission prompt. Previously, approved permissions were not dequeued from the pending list, causing the status to stay stuck until an Idle event arrived. ToolStart events now resolve the matching permission by tool_use_id so the status transitions to "Working" immediately.



## [0.14.2] - 2026-04-05

### Fixed

- **Status indicator blinks at a comfortable rate and Idle status now blinks**
  The status dot for "Needs Input" now pulses at ~1 blink per second instead of flickering rapidly at ~30Hz. The "Idle" status also blinks now, since it represents a state where the user needs to provide the next prompt.



## [0.14.1] - 2026-04-05

### Fixed

- **Permission prompt status no longer overridden by concurrent tool events**
  The dashboard now correctly maintains "Needs Input" status when a permission prompt is active and subagent tools complete concurrently. Previously, `ToolStart` and `ToolEnd` events from subagent tools (e.g., an Explore agent running Bash commands) would override the `WaitingForInput` status back to `Working`, making the permission prompt invisible in the dashboard card. The status now stays as "Needs Input" until all pending permissions are resolved.



## [0.14.0] - 2026-04-04

### Changed

- **Native Terminal Panes (Zellij Removed)**
  Zellij is no longer required. dot-agent-deck now embeds terminal panes directly using Ratatui-native widgets with `portable-pty` for PTY management and `vt100` for terminal emulation. The application is now a single binary with no external dependencies.
  All keybindings have switched from Alt-based to Ctrl-based: `Ctrl+1`-`Ctrl+9` to select cards, `Ctrl+t` to toggle layout, `Ctrl+d` to return to dashboard from a pane. Terminal panes support mouse text selection (double-click word, triple-click paragraph), clipboard copy via OSC 52, mouse scrollback, bracketed paste, and `Alt+Backspace`/`Alt+arrows` for word-level editing. Layout modes (stacked/tiled) are now managed internally with the dashboard at 33% left and panes at 67% right.
  Users who previously installed Zellij solely for dot-agent-deck can uninstall it. No configuration migration is needed — press `Ctrl+n` to open the directory picker and create a new embedded pane.



## [0.13.0] - 2026-04-03

### Added

- **Permission Prompt Control from Dashboard**
  Respond to agent permission prompts directly from the dashboard without switching panes. Previously, when Claude Code or OpenCode needed permission to run a tool (e.g., execute a bash command), users had to switch to that specific agent's pane to approve or deny — breaking the dashboard workflow and making multi-agent oversight tedious.
  Session cards now display a permission banner showing the tool name and details when an agent requests approval. Cards with pending permissions are highlighted with a distinct border color. Press `y` to allow or `n` to deny directly from the dashboard — the decision is sent back to the agent, which continues or receives denial feedback immediately. Multiple agents can have pending permissions simultaneously, and each is handled independently.
  The feature works through the `PermissionRequest` hook mechanism: the hook process stays connected to the daemon via a Unix socket while a oneshot channel mediates the response from the TUI. A 10-minute timeout prevents stale permissions from blocking agents indefinitely.



## [0.12.1] - 2026-04-02

### Fixed

- **OpenCode Prompts Render Again**
  The bundled OpenCode plugin now emits `session.prompt` events as soon as `message.created` fires, so OpenCode decks once again show the `Prmt:` label after opencode.ai’s recent API change. Reinstall the plugin (`dot-agent-deck hooks install --agent opencode`) to pick up the fix.



## [0.12.0] - 2026-04-02

### Added

- **Directory Picker Filtering**
  Finding a project directory is now instant. The new `/` shortcut puts the New Pane directory picker into filter mode so you can type part of a folder name (case insensitive) and see just the matches while the `..` parent entry stays available. Navigation wraps from the start/end of the list, and Esc clears the filter so a second Esc or `q` still closes the popup.
  Press `/` to start filtering, type to narrow the list, use `↑`/`↓` (or `j/k`) to move through the results, and hit `Enter` to accept the filter and keep navigating. Backspace edits the query, Esc clears it, and directories without subfolders now immediately confirm the selection instead of forcing you to go up.



## [v0.11.6] - 2026-04-02

### Fixed

- **OpenCode Decks Survive Session Clears**
  Clearing an OpenCode chat inside OpenCode now reuses the existing deck in dot-agent-deck instead of leaving the stale card behind and spawning a second one. The dashboard now remaps all incoming events that reference the same `pane_id` to the original session so pane layouts remain stable across `/clear` and new-chat resets.


## [v0.11.5] - 2026-04-02

### Fixed

- **Reliable OpenCode Decks**
  OpenCode sessions now show up immediately and stay inside a single deck even when you clear prompts or start a fresh chat inside the same TUI window. Previously every restart created a brand-new card (and sometimes no card at all) because the OpenCode plugin lost track of its session IDs, so the dashboard could not correlate the lifecycle events.
  The plugin now emits `session.prompt` events as soon as a user message arrives, synthesizes `session.created` and `session.deleted` transitions when OpenCode misses them, keeps a canonical session ID per working directory, and flushes the deck as soon as you exit with `Ctrl+C`. Reinstall the hook with `dot-agent-deck hooks install --agent opencode` (or rerun the installer via `cargo run`) to pick up the fix.



## [0.11.4] - 2026-04-02

### Fixed

- **Dashboard Shortcut Fix**
  `Opt+d` from agent panes in the second column now jumps directly back to the dashboard even when every pane is visible. Previously the shortcut only moved focus left one column, so multi-column layouts forced two keypresses to reach the dashboard while stacked mode kept working as expected.



## [0.11.3] - 2026-04-02

### Fixed

- **Balanced Pane Layout Toggle**
  Pressing `t` now fans agent panes out on an even grid, so each column and row gets equal space instead of inheriting inconsistent sizes from the `children` placeholder.

### Changed

- **Devbox Agent Script Defaults to OpenCode**
  Running `devbox run agent` now launches the `opencode` CLI so OpenCode sessions can be spun up without passing extra flags. The previous default pointed at `claude`, which no longer reflects the recommended workflow for the dashboard’s bundled OpenCode plugin.


## [0.11.2] - 2026-04-01

### Fixed

- **OpenCode Sessions Render Correctly**
  OpenCode panes now appear in the dashboard alongside Claude Code again. The bundled OpenCode plugin was rewritten to use OpenCode's new `DotAgentDeckPlugin` export so session, tool, and permission events are forwarded in the format the daemon expects. Previously, OpenCode quietly stopped emitting compatible events after their plugin API change, leaving the third card empty in dot-agent-deck.
  Reinstall the plugin with `dot-agent-deck hooks install --agent opencode` to pick up the fix—future OpenCode upgrades will continue to stream into the dashboard without manual tweaks.


## [0.11.1] - 2026-04-01

### Fixed

- **Version Update Notification**
  The upgrade notification in the dashboard status bar now reliably detects newer releases. Previously, a 24-hour version check cache could retain stale data, causing the app to incorrectly conclude no update was available. The cache has been removed — each launch now fetches the latest release directly from GitHub (in the background, with a 10-second timeout).



## [0.11.0] - 2026-04-01

### Added

- **OpenCode Agent Support**
  Monitor OpenCode (opencode.ai) sessions alongside Claude Code in the same unified dashboard. Previously, only Claude Code sessions were visible, forcing developers to context-switch between terminals to track what each agent is doing.
  OpenCode sessions now appear in the dashboard with an "OpenCode" label, with full event mapping for session lifecycle, tool execution, and permission prompts. The `hook` subcommand accepts an `--agent opencode` flag to receive events from OpenCode's native plugin system, and the `hooks install --agent opencode` command sets up a thin JS plugin in `~/.opencode/plugin/dot-agent-deck/` that automatically forwards events to the dashboard. Uninstalling is equally simple with `hooks uninstall --agent opencode`. All existing Claude Code functionality remains unchanged — Claude Code is still the default when no `--agent` flag is specified.



## [0.10.0] - 2026-04-01

### Added

- ## Toggle Stacked/Tiled Pane Layout
- 
- Switch between stacked and tiled layouts to see all agent panes at once. Previously, multiple agent panes used a stacked layout where only the active pane was expanded — making it impossible to monitor all agents simultaneously.
- 
- Press `t` from the dashboard (Normal mode) or `Alt+t` from any pane to cycle between layouts. In stacked mode, only the focused agent pane is expanded while others collapse to title bars. In tiled mode, all agent panes share the right column equally with responsive breakpoints: a single column for 1–3 agents, two columns for 4–6 agents, and three columns for 7 or more agents. The dashboard pane stays fixed at 33% width in both layouts.



## [0.9.1] - 2026-04-01

### Fixed

- Use true black (RGB 0,0,0) background instead of ANSI black, fixing purple background on terminals with custom themes. Modals now also have an explicit black background.
- Update notification no longer replaces keyboard shortcuts in the bottom bar; it now appears alongside them.
- Derive binary version from git tags instead of hardcoded Cargo.toml value, fixing incorrect "current v0.1.0" in update notifications.



## [0.8.0] - 2026-04-01

### Added

- Add `--version` / `-V` flag to display the current version.



## [0.7.1] - 2026-04-01

### Fixed

- Force black background on dashboard pane so colors remain readable on light terminal themes.



## [0.7.0] - 2026-04-01

### Added

- Add version update notification that checks GitHub Releases on startup and displays a non-intrusive TUI notification when a newer version is available. Results are cached for 24 hours to minimize API calls.



## [0.6.1] - 2026-04-01

### Fixed

- Fix WaitingForInput status not showing during permission prompts (e.g., Bash approval). The v0.4.1 guard incorrectly suppressed Notification events when a tool was active.



## [0.6.0] - 2026-04-01

### Fixed

- ## Fix Stats Bar Visibility
- 
- The idle count and tools count in the bottom stats bar were nearly invisible on dark terminal backgrounds. Changed their color from DarkGray to Gray for readable contrast while remaining visually subdued.



## [0.5.0] - 2026-03-31

### Added

- ## Aggregate Stats Bar
- 
- A persistent status bar at the bottom of the dashboard shows real-time aggregate metrics across all sessions. Instead of visually scanning every card to tally how many agents are active, waiting, or erroring, the stats bar provides an instant overview.
- 
- The bar displays total active sessions, per-status counts (working, thinking, compacting, waiting, error, idle), and a cumulative tool call count. Each status category is color-coded — green for working, yellow for waiting, red for errors — and zero-count categories are hidden to save space. Counts update automatically as agent events arrive with no user interaction required.

### Fixed

- ## WaitingForInput Status During AskUserQuestion
- 
- The dashboard now correctly shows "Waiting for Input" when Claude Code uses the AskUserQuestion tool. A previous fix to prevent spurious waiting status during non-interactive tools (like Bash) inadvertently blocked the status transition for interactive tools that genuinely wait for user input.



## [0.4.2] - 2026-03-31

### Fixed

- ## Cleaner Multi-Prompt Display
- 
- The "Prmt:" label now appears only on the first prompt line in session cards. Additional prompts are indented with spaces instead of repeating the label, reducing visual clutter when cards have room to show multiple prompts.



## [0.4.1] - 2026-03-31

### Fixed

- Fixed "Needs Input" status getting stuck in sidebar when a Notification event arrived while a tool was actively running.



## [0.4.0] - 2026-03-31

### Added

- ## Adaptive Card Density
- 
- Dashboard cards now automatically adjust their content density based on available screen height. When all cards fit on screen, each card shows up to three recent prompts and three tool commands for richer context. When cards would overflow, the layout switches to a compact mode showing one prompt and one tool per card, fitting more sessions on screen without scrolling.
- 
- The density recalculates on every frame, so resizing the terminal instantly adapts the layout. Three modes are available: Spacious (10 rows, 3 prompts, 3 tools), Normal (8 rows, 1 prompt, 3 tools), and Compact (6 rows, 1 prompt, 1 tool). The dashboard always selects the most spacious mode that avoids scrolling.



## [0.3.1] - 2026-03-31

### Fixed

- Preserve card position when a Claude Code session is cleared — restarted sessions on the same pane now keep their original index instead of jumping to the end.
- Fix changelog assembly to recognize semantic fragment types (feature, bugfix, breaking) so release notes are generated and fragments cleaned up correctly.



## [0.1.0] - 2026-03-27

### Added

- ## GitHub Actions CI/CD Workflows
- 
- Automated CI/CD pipeline for the project. Pull requests now run cargo fmt, clippy, build, and test checks automatically, with cargo audit for dependency vulnerability scanning.
- 
- Pushing a `v*` tag triggers multi-platform release builds for Linux (amd64/arm64), macOS (Intel/Apple Silicon), and Windows (amd64), with SHA256 checksums for all binaries. Releases are published to GitHub with auto-generated changelog notes from `changelog.d/` fragments. Homebrew formulas are published to `vfarcic/homebrew-tap` and Scoop manifests to `vfarcic/scoop-bucket` for easy installation.
- 
- Supporting workflows auto-label PRs based on changed files and manage stale issues/PRs. A `Taskfile.yml` provides distribution tasks for checksum generation, Homebrew formula creation, and Scoop manifest creation.
