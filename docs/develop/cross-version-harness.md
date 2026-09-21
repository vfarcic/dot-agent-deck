# The cross-version harness — running rule 12 as a command

CLAUDE.md rule 12 requires, for any change touching the daemon, the TUI↔daemon protocol, orchestration or hooks, a **cross-version manual test**: build the branch, start a daemon from the previous release with an agent under it, run the branch TUI against that older daemon, and confirm a delegate still routes and hooks still arrive. `cargo xver` does that, scripted.

```sh
cargo xver --branch agent/dispatch-issue-1121
```

`cargo xver -- --branch …` is accepted too. The alias already ends in `--`, so that spelling used to reach clap as a positional argument and fail; the binary now drops one leading `--`.

That is the **forward** direction, and it is the default. `--direction reverse` runs the opposite pairing — the branch daemon with the previous release's TUI and CLI — and `--direction both` runs one after the other, each in its own sandbox with its own evidence file. [The reverse direction](#the-reverse-direction) says why rule 12's pairing alone leaves a gap and what reverse does and does not close.

## Why this exists

Rule 12's procedure is written in keystrokes because a person was always going to run it, and four of its paragraphs are about ways a person silently gets it wrong. That had a consequence nobody intended: a dispatched agent reading rule 12 concludes it cannot type into two full-screen TUIs, declines the check, and leaves the review thread open. Eight green, mergeable pull requests were blocked on exactly that one thread at the time this was written.

**Rule 12's requirement is the scenario, not human fingers.** A driver that reproduces the same scenario and asserts the same tells discharges the same obligation — and unlike a person's recollection, it leaves a file a reviewer can read afterwards.

## What a run does

A run has two halves. The **outer** half is the command you type; the **inner** half is the same binary, started by the outer half inside one private [bubblewrap](https://github.com/containers/bubblewrap) namespace. The daemon, both TUIs and every deck CLI call are started by the inner half, inside that namespace. The one deck binary the outer half runs itself is the old binary's `--version` — a static print, run with an empty environment, to verify the download.

1. **Outer, preflight.** Refuses a tmpfs or short-of-space runs root or target dir (`--min-free-gib`, default 100), proves an unprivileged `bwrap --unshare-all --disable-userns` namespace starts on this host, snapshots every host endpoint candidate (file identity, listening socket inodes, and the owning pid and start time of each listener), censuses the host's deck processes — by executable name, as far as this uid can read them — by pid, start time and exe, and marks how far the operator's real deck log reached. In `resolved` mode it also requires the host's flat endpoints and per-uid endpoint sockets absent.
2. **Outer, inputs.** Downloads the previous release's `dot-agent-deck-linux-amd64` with `gh release download` into `../dot-agent-deck-xver-releases/<tag>/`, requires `--version` to print exactly `dot-agent-deck <version>`, and records its SHA-256. Fetches the branch into a **standalone build clone** at `../dot-agent-deck-xver-src`, detached at `FETCH_HEAD`, and builds it with a `CARGO_TARGET_DIR` at `../dot-agent-deck-xver-target` reused across branches (see [The build clone](#the-build-clone)).
3. **Outer, sandbox.** Creates a fresh, owner-only (`0700`) sandbox `$S` under `../dot-agent-deck-xver-runs/<branch slug>-<epoch>/`, refusing to reuse any existing entry. Writes the three-role fixture into `$S/project/.dot-agent-deck.toml` and makes `$S/project` a standalone `git init` repository. Hard-links (or copies, across filesystems) the old binary, the branch binary and the harness itself into `$S`, so every deck binary the namespace runs, and the harness itself, has a path under `$S`.
4. **Outer, namespace.** Starts `bwrap` with the harness inside it and answers the inner half over its stdio (see [The pre-connect assertion](#the-pre-connect-assertion)).
5. **Inner, proof.** Reports its mount, PID and network namespaces to the outer half, which refuses any it shares; proves each mask by `dev:ino` and the operator's home empty; requires its own environment to be exactly the plan's allowlisted entries; and requires the fixture to be what stops the deck's project-config walk. Runs both builds' `daemon hello` and refuses a same-build-id pairing before any daemon starts.
6. **Inner, scenario.** (Forward; [reverse](#the-reverse-direction) swaps which build plays each part and changes nothing else.) Starts the **old** daemon and records its full identity; waits, by reading the kernel's socket table rather than by starting a client, until it listens where the endpoint matrix says it must; attaches the **old** TUI over a PTY and opens the orchestration through the production `Ctrl+N` flow, sending each key only once the screen shows the state that key is meant for (the picker's footer, the form, the orchestration chip hiding the Command field, `Enter: submit`) rather than after a fixed pause; confirms through `daemon status --json` that all three roles are live; closes that TUI with `Ctrl+D`, `Ctrl+C`, **Detach** and re-checks; attaches the **branch** TUI, records the build-version mismatch prompt and **declines** it with `n`; delegates from inside the orchestrator pane, then issues `work-done` and — last — `agent-event --type running` from inside a worker pane. The pre-connect assertion runs before every one of those clients.
7. **Inner, teardown.** Stops the branch TUI and then the daemon, each by verified identity (see [Teardown](#teardown-is-by-verified-identity)), then censuses the PID namespace for survivors.
8. **Outer, postconditions.** After the namespace has exited: requires no process left in the run's PID namespace and none referring to `$S`, the host's endpoint candidates unchanged since baseline, no host listener under `$S`, and no line mentioning `$S` among the bytes appended to the operator's real deck log; reports which baseline deck processes are still the same process.
9. Writes a markdown evidence file to `.dot-agent-deck/xver-evidence/<branch slug>.md` — `<branch slug>-reverse.md` for a reverse run, so the two directions of one branch never overwrite each other — and removes `$S` only after a clean pass with all postconditions met.

## The four tells

A run asserts these and prints each one with the value it was decided on, because the evidence file is what resolves a review thread and a reader who did not watch the run has to be able to check it.

| tell | what it rules out |
| --- | --- |
| exactly one `Attach protocol listening` line in the sandbox log | two means the branch TUI lazy-spawned its own daemon — the tell for *both* the no-agents cause and the 30-second idle-window cause, whichever swallowed the run |
| the same daemon process — its pid, and `/proc/<pid>/exe` still the old binary — at the start and at the end; plus `ss -xlp` naming that pid as the endpoint's owner at both ends | the daemon was replaced partway |
| a delegate still routed | the payload really landed in the target pane |
| hooks (work-done, status) still arrived | the daemon's feedback really reached the orchestrator's pane, and the status change really reached its own state |

Tell 2's `ss -xlp` runs inside the namespace, so the pids it names are the run's private PID namespace's; the evidence also records the kernel table's own answer (listening inode and holders) at both ends. Tell 2 does **not** re-read a build id at the end, and its title says so: the build id rests on two earlier observations — the executable it checks is the staged binary whose `daemon hello` build id the preflight recorded, and the mismatch prompt reported the running daemon's build id over the wire. A tell the harness could not measure is reported as **not checked** and the run is **INCOMPLETE**, not a pass. The table is written for forward; in reverse the same four tells pin the **branch** daemon and binary, and [The reverse direction](#the-reverse-direction) says what a second `Attach protocol listening` line means there.

## Why no tell passes vacuously

**What has been demonstrated by a run is narrower than the heading.** Pointing `--old-binary` at the branch build makes both sides report one build id, and the run is refused before any daemon starts. That shows the preflight's same-build-id refusal (`compare_hellos`) is not vacuous: a same-version pairing cannot be dressed up as a cross-version run. Because that refusal fires upstream of all four tells, no tell runs in it, so it demonstrates nothing about the tells themselves. Their non-vacuity rests on the reasoning below, traced through the code by review rather than demonstrated by a run.

- **Tell 1.** It counts lines in the sandbox's `deck.log`. Every deck process of the run is started with `DOT_AGENT_DECK_LOG` pinned to that file, and a daemon a client lazy-spawns inherits that client's environment, so a second daemon writes its own line there. A log that cannot be read counts as zero lines, and zero fails exactly as two does: only one passes. It is counted after every other step.
- **Tell 2.** The attach endpoint's listeners are captured before the second TUI exists and again at the end, and the tell passes only when the two sets are equal **and** contain the recorded daemon pid — two empty sets are equal but contain no pid, so they fail — with that pid still alive and `/proc/<pid>/exe` the daemon's binary (the old one forward, the branch's in reverse). The daemon is the harness's own child and has not been reaped while it is alive, so its pid names that process and no other. `ss` being unavailable makes the tell **not checked**, which makes the run INCOMPLETE.
- **Tell 3.** It is asserted on the target `coder` pane's screen after focusing that pane. The stacked layout draws only the focused pane, and `coder` is `cat`, which shows only what the daemon wrote into its PTY, so the sentinel echoed by the orchestrator's own shell cannot satisfy it. The task file the daemon writes is read for an informational note and does not decide the verdict.
- **Tell 4.** `work-done` is asserted by the daemon-authored feedback line appearing in the orchestrator's pane, a different pane from the one that issued the command, so the issuing shell's echo cannot satisfy it. The status half is asserted by `daemon status --json` reporting the worker pane non-empty and not `Idle`, which is the daemon's own state. Both halves must hold, and a status wait that times out is a FAIL.
- **The aggregation.** An isolation failure makes the run INCOMPLETE whatever the tells say; otherwise any failing tell makes it FAIL, and no tells or an unmeasured one make it INCOMPLETE; only then is it a PASS. The command exits zero only for a PASS whose outer half completed and whose postconditions all held, and a postcondition that could not run counts as one that failed. `report.rs` and `main.rs` unit-test that chain.

## The five false greens, and where each one is handled

Rule 12 documents four ways this procedure silently measures nothing; PR #1179 added a fifth. A false green here is worse than not running the check at all, because it clears a gate with nothing behind it.

1. **No agents under the old daemon.** With zero agents the branch TUI takes `MismatchAction::SilentRestart` — it SIGTERMs the old daemon and lazy-spawns its own, with no prompt and no output. The scenario's orchestration step is what avoids it, and the missing mismatch prompt is what catches it: the harness treats "no prompt" as a hard failure of the run rather than as a pass, because the prompt appearing *is* the proof the scenario was reached.
2. **The 30-second idle window.** `DEFAULT_IDLE_SHUTDOWN_SECS` is 30, so more than 30 s between `daemon serve` and the first attach and the daemon exits and gets replaced. Every process the harness starts carries `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0`, which is the documented "always on" production value rather than a test hook.
3. **`Ctrl+C` in `PaneInput` mode goes to the pane.** With a role pane focused it kills that role's process and the orchestration comes back one role short. The harness sends `Ctrl+D` first, presses Enter only once the quit dialog shows `> Detach` selected (never `Stop`), and then re-asserts the three-role list — this one fails loudly rather than silently, and the re-assert is what makes it loud here.
4. **Teardown by an unscoped `pkill`.** See [Teardown](#teardown-is-by-verified-identity) below.
5. **`XDG_RUNTIME_DIR` being set, for a change that moves the endpoint path in the fallback case only.** On a normal desktop session it is set, both builds resolve byte-identical endpoints, and the run exercises the arm such a change did not touch. `--unset-xdg-runtime-dir` is the lever, and it is an option rather than a hardcode because for every other kind of change the ordinary desktop configuration is the faithful one.

## The reverse direction

### The gap in rule 12's pairing

Rule 12 prescribes exactly one pairing: a **previous-release daemon** with the **branch TUI**. That proves the branch client can drive an old daemon, which is a real property and the one the rule asks for. But most of what rule 12's trigger list names — daemon, orchestration, hooks — is code that runs **in the daemon**, and in the forward pairing the daemon is the previous release. For a daemon-side change the forward run therefore executes **none of the changed lines**: it passes against the old daemon's code and says nothing about the new. This is a gap in the rule as written, not in any one run.

`--direction reverse` is the pairing that does execute them: the **branch daemon**, with the orchestration stood up under it by the branch's own TUI, then the **previous release's TUI** attached with the mismatch prompt declined, and the **previous release's CLI** issuing every pane command. It models a downgrade, or a stale binary left on disk beside a newer daemon. Everything else about a run is unchanged: the same namespace, environment allowlist, pre-connect assertion before every client, teardown by verified identity and postconditions, in both directions.

For the eight branches this was first run on, the direction that executes the changed code was established from their diffs:

| PR | issue | changed code runs in | direction that executes it |
| --- | --- | --- | --- |
| #1161 | #1109 | the daemon's teardown paths | reverse |
| #1168 | #1031 | the daemon's delegate delivery | reverse |
| #1169 | #1082 | the daemon's logging, after decode | reverse |
| #1179 | #1121 | the client's resolver, and the daemon's bind | both |
| #1183 | #1182 | the hook CLI's normaliser, and the daemon's matcher | both |
| #1187 | #925 | the daemon's `AppState::apply_event` | reverse |
| #1188 | #1129 | the CLI's acknowledgement wait, and the daemon's acknowledgement write | both |
| #1190 | #1181 | git children the daemon spawns | reverse |

Reverse does not replace forward. Rule 12 asks for the forward run for every one of them; reverse is the additional run that reaches the daemon half.

### What is genuinely different in reverse

- **The prompt and its decline key are the OLD build's.** Both are established from that build rather than assumed: `src/build_version_handshake.rs` is byte-identical between v0.41.0 and every branch this was written against, the published v0.41.0 binary carries the prompt's strings, and in both builds `s`/`S` without Ctrl is the only affirmative key and every other key declines. The run sends `n`, records the prompt exactly as the old TUI printed it, and tells 1 and 2 then prove the old TUI stayed on the same daemon rather than restarting it.
- **The old client may not find the branch daemon at all.** That is a measured outcome with its own verdict, `OLD CLIENT CANNOT DISCOVER THE BRANCH DAEMON`, distinct from `FAIL` (the two builds reached each other and did not interoperate) and from `INCOMPLETE` (the harness could not tell). It needs positive evidence: a listener the run did not start, held by a process whose full identity is the old build running `daemon serve` with the run's marker — the daemon the old client lazy-spawned. The run records that second daemon by identity, lets the pre-connect assertion account for its listeners at exactly the paths it bound, and then asserts three collateral tells: the branch daemon is untouched, it still runs every role, and the old client's own daemon runs **none** of those roles. A collateral failure makes the run `FAIL`, because it means the fallback damaged something beyond not finding the daemon; an unmeasured one makes it `INCOMPLETE`. Inside the namespace an old client that lazy-spawns does so into the private `/tmp`, so this is measurable without risk to the host. The second daemon is stopped by verified identity before the branch daemon, through `proc::terminate_identity` — it is not the harness's child, so there is no un-reaped handle, and the start-time field and the private PID namespace stand in for one.
- **Tell 2 pins to the branch daemon.** Same pid, and `/proc/<pid>/exe` the staged **branch** binary, at both ends.
- **Tell 1 still means "one daemon".** Exactly one `Attach protocol listening` line is the branch daemon's. Two in reverse mean a daemon of another build started: the old client lazy-spawned its own (it did not find the branch daemon), or an idle window swallowed the branch daemon and a client replaced it. In the cannot-discover outcome the count is recorded as part of the discovery evidence, where two is the expected consequence rather than a tell.
- **The endpoint matrix is learned, not predicted, in one configuration.** With `--endpoint-mode resolved --unset-xdg-runtime-dir` the daemon is a branch build, and whether it binds the flat fallback (every build before #1121) or the per-uid directory (#1121) is that branch's own behaviour. Both are candidates; the run waits until the daemon holds one pair and from then on requires every other candidate absent before every client — the same strength as a predicted matrix. The evidence says which pair it was. Every other mode has one candidate.
- **Teardown identity matters more, not less.** The daemon is a branch build whose command line looks even more like a production deck's. Every identity check is kept: start time, exe, exact cmdline, cwd, whole environment and mount namespace, re-read before its one SIGTERM and again before any SIGKILL.
- **`PATH` models the downgrade.** In reverse `$S/bin/dot-agent-deck` — first on `PATH` — is the previous release, and the branch build is staged at `$S/branch/dot-agent-deck`. Pane commands in reverse are typed with the old build's absolute path, so the evidence names exactly which binary sent each stimulus.

### Branch-specific probes

The four tells are rule 12's and reverse asserts them. They are not enough on their own, because a delegate and two hooks never reach, say, the line that escapes a hostile id in a log record. A reverse run therefore also carries a **probe**: the minimal stimulus that reaches the branch's changed arm, with an assertion on what the branch daemon then did. It is selected by the branch name's issue number (`--probe auto`, the default), or named with `--probe`; `--probe generic` runs the four tells only, and a branch no probe is known for gets `generic`. The evidence file says which. Probes run only in reverse, and an explicit probe with `--direction forward` is refused rather than dropped.

Every stimulus is issued by the previous release's CLI from **inside** a pane the branch daemon spawned, so it carries that pane's genuine identity and capability rather than one the harness copied, and each is preceded by the pre-connect assertion. No probe needs an agent credential: where a real agent's producer behaviour matters, the exact payload is injected through `dot-agent-deck hook` on stdin, or produced by a synthetic stand-in.

| probe | branch | stimulus | pass requires |
| --- | --- | --- | --- |
| `teardown-inventory` | #1161 | old-TUI `Stop` (`Ctrl+D`, `Ctrl+C`, `Down`, `Enter`, `y`), sending `KIND_SHUTDOWN`; only after the pre-connect assertion re-proves the daemon, and after tells 1 and 2 because it ends the daemon | exactly one `shutdown-frame` inventory record naming the agent and role counts and every captured pane, role and agent, and no `signal` record, before the daemon's exit |
| `late-session-start` | #1168 | old-CLI `delegate` to `lateboot`, a `clear = true` worker whose command is the synthetic `claude` stand-in; it swallows the first submit CR, and after a 2 s quiet control runs the old `hook` CLI with a late `SessionStart` from its own process | exactly one submission afterwards, of the complete, unmodified pointer |
| `log-escaping` | #1169 | old `hook` CLI from the reviewer pane with a `UserPromptSubmit` whose session id decodes to contain a newline, then an ordinary `Notification` | one physical `Received event` record with the escaped id, no physical line starting `FORGED-LINE`, the daemon alive, and the reviewer's card `WaitingForInput` |
| `discovery-fallback` | #1179 | none beyond the attach: the old TUI started with no XDG and no override against a branch daemon in the per-uid directory | the cannot-discover classification above; refused (`INCOMPLETE`) if the branch daemon bound the flat pair |
| `paste-envelope` | #1183 | old-CLI `dispatch --single` to a Claude-typed stand-in, which reports the exact pasted payload through the old `hook` CLI inside `<pasted_content id="57b9">` | one `confirmation="paste-envelope"` record, one paste and one report, and no retry, probe, abandonment or unconfirmable record through 20 s past it |
| `cross-pane-session-key` | #1187 | old `hook` CLI from two extra shells, `alpha` and `beta`, under one session id: alpha `SessionStart` + `PreToolUse(Bash)`, beta `SessionStart` + `Notification`, then beta `SessionEnd` | one `alpha` card `Working` with tool `Bash` and one `beta` card `WaitingForInput`, on distinct panes, and alpha unchanged after beta's `SessionEnd` |
| `signal-ack` | #1188 | tell 4's old `work-done`, plus an old-CLI `dispatch --single`, then an old `agent-event --type waiting` | the unit up in a listed worktree holding the task, and the status event landing afterwards |
| `git-env` | #1190 | every process carries #1181's eight git location variables pointed at a decoy repository; old-CLI `dispatch --single` | raw `git` resolving the decoy first (the control), then the unit's worktree rooted in the intended repository and the decoy byte-for-byte unchanged |

Where a probe's own precondition does not hold — the pointer never parked, the hostile variables not live, the branch daemon not in the per-uid layout — its tell is **not checked** and the run is `INCOMPLETE`, because a pass there would have measured nothing.

A probe is evidence of a fix only if it would fail without it. `teardown-inventory`, `late-session-start` and `paste-envelope` assert on a log record only the fixed build writes, and `log-escaping` on the escaped form of one. `cross-pane-session-key` and `git-env` assert on state a pre-fix daemon also produces, so they were run once against `main`, which carries neither fix, with `--branch main --probe <name>`: both FAILED there, `main`'s daemon losing alpha's status and cutting the dispatch's worktree from the decoy. Re-run that control after changing either probe.

Three probes change the run's inputs, each narrowly, and the evidence file names each change. `late-session-start` adds `RUST_LOG=dot_agent_deck=debug` and the three delegate timer knobs set to `0`; `git-env` adds the eight location variables. Those are admitted by `sandbox::check_env` by exact name **and** value, never as a base allowlist entry, never with a credential-shaped name, and never replacing a base entry. The dispatch probes (`paste-envelope`, `signal-ack`, `git-env`) create `$S/config/config.toml` holding one key, `default_command` — what `dispatch --single` runs — and commit the fixture so a worktree has a `HEAD` to branch from. `git-env` also creates the decoy, a second standalone repository under `$S`, with an empty environment before any hostile variable exists.

### What reverse covers, and what it does not

It covers the branch daemon's handling of **the stimuli it sends**: the same rule-12 flows, and each probe's one stimulus, from a v0.41.0 client, with the assertions above. Stated narrowly:

- **Each probe covers one stimulus, not the changed surface.** `log-escaping` sends one control character in one field; `cross-pane-session-key` one two-pane collision; `git-env` one dispatch under one coherent set of hostile variables; `late-session-start` one swallowed CR. The probe spec's per-branch residual-risk notes list what each leaves out.
- **Stand-ins model one producer behaviour each.** The synthetic `claude` reproduces #1031's swallowed CR and #1182's envelope exactly as those issues measured them. It says nothing about when a real Claude boots or which envelope id it picks; the branches' own lane-2 tests are where a real agent is measured.
- **One previous release.** A client older or newer than `--previous` is not exercised.
- **The desktop GUI's handshake** (`classify_handshake`) is not exercised in either direction.
- **The three dispatch probes use `dispatch --single`'s defaults** — a `default_command` from the global config, a worktree beside the project — and do not cover an orchestration dispatch.
- **`signal-ack` does not discriminate the fix, by design.** An old fire-and-forget client never reads the acknowledgement, so a daemon that writes none passes it too; it asserts that the acknowledgement's write to a closed peer does not break the verbs or wedge the listener, which is the reverse half of #1129's compatibility argument, not proof the write happens (nothing logs it).

## Isolation: what a run guarantees

The first version of this harness isolated a run with environment variables alone, and in `resolved` mode that bound **real host addresses**: a run's published-v0.41.0 daemon, with `TMPDIR` pointing inside its sandbox, owned the host's `/tmp/dot-agent-deck-1000.sock` and `/tmp/dot-agent-deck-attach-1000.sock`, so for the length of that run any production client resolving the flat fallback would have reached a sandbox daemon. `TMPDIR` moves only a post-#1121 build's new primary fallback; the branch's compatibility root and every published build before it spell a literal `/tmp`. An independent safety audit then found the environment incomplete in every mode, the pre-connect check unable to tell who owned a socket, and a cmdline substring standing in for teardown identity. What follows is what replaced each of those, stated as narrowly as it was measured.

### The namespace

The daemon, both TUIs and every deck CLI call of a run execute inside one `bwrap` namespace, started with `--unshare-all --unshare-user --disable-userns --die-with-parent --new-session`, in which:

- `/tmp` is `$S/fallback-tmp` and `/run/user/<uid>` is `$S/run-user`. **Both** endpoint roots are masked because masking `/tmp` alone is not enough: under a read-only view of `/`, the production XDG sockets stay connectable — a read-only mount does not stop `connect(2)` on a Unix socket.
- `/var/tmp` is `$S/var-tmp`. No deck resolution rule names `/var/tmp`; other sandboxes park sockets there, and this keeps them out of reach as defence in depth.
- The operator's home is an empty tmpfs with only `$S` bound back into it, so `~/.local/bin` (where the production deck lives on the box this was written on), `~/.config/dot-agent-deck`, `~/.dot-agent-deck.toml` and every neighbouring sandbox under the home are not visible inside.
- The rest of `/` is bound read-only. **Not every submount honours that**: measured on the box this was written on, the submounts bwrap cannot remount stay `rw` — Docker's per-container `nsfs` and overlay mounts under `/run/docker/netns` and `/var/lib/docker/rootfs`, and `/proc/sys/fs/binfmt_misc` — all root-owned, so the run's uid has no write permission there. `awk '$6 ~ /(^|,)rw(,|$)/ {print $5}' /proc/self/mountinfo` run inside the same `bwrap` invocation lists them on another host.
- The PID, network, IPC, UTS and user namespaces are private, and creating a nested user namespace is disabled.

Inside it a run resolves endpoints the way a real host does: with `XDG_RUNTIME_DIR` unset and `TMPDIR=/tmp`, a v0.41.0 daemon binds `/tmp/dot-agent-deck[-attach]-<uid>.sock` and a post-#1121 build looks first in `/tmp/dot-agent-deck-<uid>/` and then at those legacy paths — the same spelling as on the host, landing in `$S/fallback-tmp`. With `XDG_RUNTIME_DIR` kept, it is pinned to the host's spelling `/run/user/<uid>`, which inside is `$S/run-user`.

### The environment

Every process the harness starts gets an environment built from an allowlist — `env -i`, not an overlay on the caller's. Processes the daemon starts — the panes — inherit the daemon's, plus the deck's own pane variables. The allowlist is `sandbox::ALLOWED_ENV`; the inner half requires its own environment to be exactly the plan's entries (which proves `--clearenv` held), the daemon's `/proc/<pid>/environ` is checked against the same map and recorded in the evidence, and a credential-shaped name is refused even if someone adds it to the allowlist.

| variable | why |
| --- | --- |
| `PATH` = `$S/bin:/usr/local/bin:/usr/bin:/bin` | the staged **client-side** build first: in forward the branch build, so a pane that shells a bare `dot-agent-deck` reaches the branch binary while the daemon in memory is still the old one — the upgrade this test is about; in reverse the previous release — the downgrade. The operator's `~/.local/bin` is deliberately absent. (The daemon then applies a login shell's `PATH` to what it spawns, which is why a probe's stand-in is always named by absolute path) |
| `HOME`, `TMPDIR=/tmp`, `TERM`, `LC_ALL`, `COLORTERM`, `SHELL`, `USER`, `LOGNAME` | ordinary settings; `USER`/`LOGNAME` come from the password database, not from the caller |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` | `HOME` alone does not cover them: `schedules_path()` consults an inherited `XDG_CONFIG_HOME` before `HOME`, so a sandbox daemon could otherwise read the operator's real schedules and fire them |
| `DOT_AGENT_DECK_CONFIG`, `_SESSION`, `_SCHEDULES` | pinned to files under `$S/config`. The harness creates none of them except `config.toml` for a dispatch probe (one key, `default_command`; see [Branch-specific probes](#branch-specific-probes)); an absent schedules file is "no schedules". A TUI writes `session.toml` itself as it runs |
| `DOT_AGENT_DECK_STATE_DIR`, `_LOCK_DIR` | per-user state and the per-endpoint lock |
| `DOT_AGENT_DECK_LOG` | **resolved separately from the state dir.** Without it an otherwise-isolated daemon appends into the operator's real `~/.local/state/dot-agent-deck/deck.log` |
| `DOT_AGENT_DECK_EXPERIMENTAL` | pinned explicitly (off by default, `--experimental` to turn it on) rather than inherited |
| `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` | false green 2 above |
| `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` | a second backstop for a stand-in that escapes its process group; the PID namespace is the first |
| `GIT_CONFIG_NOSYSTEM=1` | git inside the run reads no system config |
| `DAD_XVER_SANDBOX=$S` | the run's unique marker: part of every sandbox process's identity, and what the post-run census looks for |
| a probe's additions | `RUST_LOG` and three delegate timers (`late-session-start`), #1181's eight git location variables (`git-env`) — each admitted by exact name and value, never replacing a base entry and never credential-shaped; see [Branch-specific probes](#branch-specific-probes) |
| `XDG_RUNTIME_DIR` | `/run/user/<uid>` (private inside) or, with `--unset-xdg-runtime-dir`, **absent** |
| `DOT_AGENT_DECK_SOCKET`, `_ATTACH_SOCKET` | `$S/hook.sock` / `$S/attach.sock` in `sandbox-sockets` mode; **absent** in `resolved` mode |

The fixture `$S/project/.dot-agent-deck.toml` is created fresh, must be a regular file owned by the run's uid, and is re-verified before launch: the deck's project-config discovery walks up from **cwd** and stops at the first such file, so this is what keeps the walk from reaching the operator's own config. The masked home makes that doubly true, since nothing above `$S` is visible to walk into.

### The pre-connect assertion

Immediately before every client process — each TUI, each `daemon status` poll — and before every deck command typed into a pane, the inner half re-proves, and aborts before the client exists if any of it fails or cannot be evaluated. The harness's own clients are spawned directly, with no shell between the last check and their `execve`. A command typed into a pane is started by that pane's stand-in shell instead, with the environment the daemon gave the pane — the daemon's own, verified against the plan as part of its identity, plus the deck's pane variables — so check 2 covers a pane command only through the daemon's environment.

1. its mount namespace is the one it recorded at startup and not the outer half's, and each mask's `dev:ino` still matches;
2. the client's environment is exactly the plan's and passes the allowlist policy;
3. the daemon still matches its recorded identity (below);
4. each endpoint the matrix says the daemon owns is listening **in the kernel's table**, every listening inode there is held in `/proc/<daemon pid>/fd`, and the path is an owner-only socket of the run's uid;
5. each candidate the matrix says must be absent has no file and no listener;
6. every listening Unix socket in the run's network namespace is held by the daemon — which is what rules out a second daemon, because only a listening one is reachable. A command-line count could not: with a lifetime cap set, the daemon forks (not execs) a reaper per pane that keeps its exe, command line and environment and never listens;
7. the outer half confirms the host's endpoint candidates are exactly as at baseline — same file identity or still absent, same listening inodes, every baseline owner still the same process holding its socket — and that no host listener lies under `$S`.

The endpoint matrix for `resolved` with `XDG_RUNTIME_DIR` unset — the #1121 configuration — is: the old daemon owns the flat `/tmp/dot-agent-deck[-attach]-<uid>.sock`; the per-uid `/tmp/dot-agent-deck-<uid>/{hook,attach}.sock` and the XDG pair must be absent. That absent half is what proves a run reached the **fallback arm** rather than one #1121 did not touch: of the endpoint candidates a branch client resolves, the only one that answers is the legacy flat one, reached through the compatibility read. The evidence records that proof before the branch TUI starts, and confirms it afterwards from tells 1 and 2.

**What the assertion does not have to win is the race.** Between the last check and the client's `connect`, the expected socket could vanish. A client that then resolves onward can only land on another path inside `/tmp` or `/run/user/<uid>` — both private — because its environment names no host path. There it would lazy-spawn a daemon inside the namespace, which tell 1's count and the next pre-connect's second-listener check both catch. So the TOCTOU window can turn a run into a FAIL; it cannot reach a host endpoint.

### Teardown is by verified identity

On 2026-09-15 an agent finishing with a cross-version sandbox ran `pkill -f "daemon serve"`. That pattern also matches the **production** daemon's own command line, which took the SIGTERM 1.42 s later and gracefully stopped nine panes across three dispatched units ([#428 occurrence #5](https://github.com/vfarcic/dot-agent-deck/issues/428#issuecomment-5688814518)). `pkill` sends SIGTERM, so it goes *around* issue #770's `daemon stop` refusal rather than having to defeat it.

Nothing in this harness signals by name, and a cmdline substring no longer counts as identity. Each process the inner half spawns has, captured right after spawn and re-read immediately before any signal: its start time (field 22 of `/proc/<pid>/stat`), `/proc/<pid>/exe`, its exact NUL-separated command line, its cwd, its **whole** environment (which carries `DAD_XVER_SANDBOX`), and its mount namespace. A single difference refuses the signal and is recorded. The daemon gets exactly **one** SIGTERM — a second one force-exits it past its graceful teardown — then 20 s, well past its 3 s agent grace, and SIGKILL only after a second full re-verification. Two structural properties sit underneath the checks rather than replacing them: the process is inside the run's private PID namespace, where no host process has a pid at all; and its `Child` handle stays un-reaped until after the last signal, so the kernel cannot give its pid to anyone else in between. The TUIs are stopped through their own un-reaped handles after the same re-verification.

After the daemon, the inner half censuses the PID namespace: each survivor that carries the run's marker is signalled individually after its identity is recorded; one that does not is left alone. When the inner half exits, bwrap's init exits with it and the kernel kills every process left in the PID namespace, `setsid`'d or not; the outer half's census then requires that namespace empty. The outer half itself signals nothing but its own `bwrap` child, and only when `--run-timeout-secs` (default 1200) expires — through that child's un-reaped handle, which takes the whole namespace down with it.

### What the isolation does not cover

- **The build runs outside the namespace.** The branch's build scripts execute under `cargo build` in the build clone with the caller's toolchain environment — the devbox/nix compiler wrappers need dozens of variables — minus credential-shaped names and the deck's own pane variables. That is a **denylist**, narrower than the run's allowlist, and a build script can still read any file the operator can.
- **`git clone`/`git fetch` and `gh release download` run with the operator's environment**, because they may need its credentials. They are not deck processes and do not run in the namespace.
- **Reads are not confined to `$S`.** Inside the namespace the rest of `/` is readable (read-only), apart from the masked home, `/tmp`, `/run/user/<uid>` and `/var/tmp`. Pathname sockets elsewhere under `/run` — the system D-Bus socket, for one — stay connectable; no deck resolution rule names any of them.
- **The sandbox's own sockets are reachable from the host.** A pathname socket under `$S` is a file on the host's filesystem, and pathname sockets do not respect network namespaces. A host process that deliberately connects to `$S/…` reaches the sandbox daemon; no host deck resolves an endpoint under `$S`.
- **The host checks cover the endpoint candidates and the host network namespace.** They prove the candidate paths and their listeners unchanged and no host listener under `$S`. They do not diff the rest of the host's filesystem; the read-only root and masked home are what keep writes out of it, subject to the `rw` submounts listed above.
- **The deck census is an observation.** A baseline deck process that exits during a run is reported, not attributed — other units share the box. The production endpoints' owners are the exception: an owner that changes fails the run (INCOMPLETE), because that is exactly what an escape would look like.
- **The real-log check reads one file**: `DOT_AGENT_DECK_LOG` if the caller has it set, otherwise `~/.local/state/dot-agent-deck/deck.log`.
- **It needs bubblewrap and unprivileged user namespaces.** On a host where the smoke test fails the run refuses before building anything; there is no un-isolated fallback.

## The two endpoint modes

`--endpoint-mode sandbox-sockets` (the default) pins `DOT_AGENT_DECK_SOCKET` and `DOT_AGENT_DECK_ATTACH_SOCKET` to `$S/hook.sock` and `$S/attach.sock`. This is the right choice for any change that does not touch endpoint resolution. The matrix then requires every resolved candidate — flat, per-uid and XDG, all private — absent.

`--endpoint-mode resolved` sets neither override, so both builds resolve their endpoint the way they would on a real host — into the namespace's private `/tmp` and `/run/user/<uid>`. **Use it only when the change under test *is* endpoint resolution**: those overrides short-circuit resolution before the logic under test runs. Its deck processes can no longer bind a host endpoint, so a `resolved` run is no longer exclusive for the host; the outer preflight still refuses to start one while the host's flat or per-uid endpoint sockets exist, which keeps the before/after comparison unambiguous.

For issue #1121 the invocation is both levers together:

```sh
cargo xver --branch agent/dispatch-issue-1121 --endpoint-mode resolved --unset-xdg-runtime-dir
```

In that mode a run also records whether the branch build created an endpoint directory of its own (`/tmp/dot-agent-deck-<uid>` inside, `$S/fallback-tmp/dot-agent-deck-<uid>` outside) while it was attached to the old daemon at the legacy address. That is an observation rather than a fifth tell: it checks one change's claim about itself — that its compatibility read is read-only — where the four tells are what rule 12 asks of every change.

## The build clone

The branch is built in a **standalone clone** — its own `.git`, cloned from `https://github.com/<--repo>.git` on first use — not in a linked worktree of the operator's repository. The first version used a linked worktree, and a linked worktree writes its registration, HEAD, index and reflogs into the source repository's common `.git`, outside anything the run owns, where another session's `git worktree prune` can reach it; its stash is shared with every other worktree too. The harness refuses a `--source-clone` whose `.git` is a file, requires `git rev-parse --absolute-git-dir` and `--git-common-dir` to both be the clone's own `.git`, refuses to build over tracked modifications, and runs every git command with the eight location variables removed (issue #834's shape).

It stays at a fixed path **outside** `$S`, which is a deliberate trade: a per-run clone under `$S` would give each run a fresh workspace path, and cargo would rebuild the whole workspace crate into the shared target dir every time — or, with a per-run target dir, compile everything from cold, fourteen times over on a box whose I/O has already saturated under concurrent builds (issue #863). At its default path the clone is under the masked home, so nothing inside the namespace can see it (a `--source-clone` outside the home would be visible, read-only); the run executes the staged copy of the binary in `$S`. The runtime repository the run does touch, `$S/project`, is a standalone `git init` under `$S`.

What it does to git is bounded: a `git clone --no-checkout` once, then per run `git fetch <url> <branch>` (which writes `FETCH_HEAD` and objects in the clone and nothing else) and `git checkout --detach FETCH_HEAD`. It never commits, amends, rebases, stashes or pushes, it writes nothing into the operator's repository, and it posts nothing to GitHub.

## Running several at once

Each run mints its own sandbox, namespace and endpoints, so neither mode and neither direction collides with another run at the endpoint level. What two concurrent runs do share by default is the **build clone and the target dir** — two runs would check out different commits into the same clone and build into the same target at once. Give each concurrent run its own `--source-clone` and `--target-dir`; a fixed set of lanes (`-xver-src-a`, `-xver-target-a`, …) reused across branches keeps the cache benefit. The release cache (`--releases-dir`) is read-mostly once populated; pre-warm it with one run rather than racing the first download.

## Reading the evidence file

`.dot-agent-deck/xver-evidence/<branch slug>.md` (override with `--evidence`) carries: both builds' `daemon hello` output verbatim, the sandbox and staged binary paths, the daemon pid, each tell with the measured value it was decided on, an **Isolation** section listing every isolation check that was measured and held (namespace ids, mask identities, the daemon's recorded identity, the first pre-connect assertion per client kind with its kernel listener map, the fallback-arm proof in `resolved` mode, the survivor census), a **Postconditions** section with the outer half's post-run checks, a numbered run log, and raw excerpts — the daemon's environment read back from `/proc`, the mismatch prompt as printed, the target pane's screen after the delegate, the orchestrator's screen after the hook, and the tail of the sandbox log. It is written on **every** path once the sandbox exists, including a run that broke down partway. A `--skip-build` run says so directly under the verdict, because the checked-out HEAD then says nothing about the binary under test: the line names the commit the binary's own build id names — flagged **STALE** when that is not the HEAD — or states that no commit is knowable from it. The build id is the binary's own stamp, from `git` when its `build.rs` last ran or from an injected `DAD_BUILD_ID`, and the line says that too.

The verdict is **PASS**, **FAIL** (a tell failed or the scenario broke down — the branch and the previous release did not interoperate, or the scenario could not be stood up, which the run log says), **INCOMPLETE** (a tell could not be measured, or an isolation check failed or could not be evaluated — so the run measured nothing either way), or, in reverse only, **OLD CLIENT CANNOT DISCOVER THE BRANCH DAEMON** (measured: the old client started a daemon of its own instead of finding the branch's, and the collateral tells proved nothing else happened). An isolation failure dominates: a tell measured inside a namespace that did not hold is not a measurement of the branch. A failed tell outranks a measured non-discovery, and an unmeasured one does too — "nothing else happened" is only a claim when it was measured. A reverse evidence file carries a **Discovery** section when that outcome occurred, the probe it ran, and each probe tell.

The sandbox is removed after a clean pass and kept after anything else; `--keep-sandbox` keeps it either way. Its `artifacts/` holds both PTY streams verbatim, the old daemon's stdio and the inner half's evidence JSON.

## What it covers, and what it does not

Forward covers the pairing rule 12 and [`versioning.md`](versioning.md) exist for: a **newer TUI against an older daemon**, over the real attach protocol, with real orchestration state in the old daemon's memory, asserting on payloads rather than on exit codes. Reverse covers the opposite pairing for the same flows plus one branch-specific stimulus each; [What reverse covers, and what it does not](#what-reverse-covers-and-what-it-does-not) states that narrowly.

It does not cover:

- **A real agent.** Every role is a stand-in (`sh`, `cat`), deliberately: this check is about the TUI↔daemon wire, so no *agent* credential is used or needed. (`gh release download` uses your GitHub credential, and `--old-binary` avoids even that.) It therefore says nothing about how a real agent behaves across the version boundary, and it is not a substitute for the lane-2 real-agent tests CLAUDE.md rule 4 asks for.
- **A daemon-side change, in the forward direction.** Forward runs the previous release's daemon, so a change that lives in the daemon executes nowhere in a forward run; `--direction reverse` is the pairing that runs it.
- **The desktop GUI.** `classify_handshake` compares `PROTOCOL_VERSION` and `CONTRACT_BREAKS` and is a different code path from the TUI's build-version handshake; nothing here exercises it.
- **More than one previous release per run.** `--previous` takes one tag.
- **Anything that is not Linux.** `/proc`, bubblewrap, `ss(8)` and `statvfs` are all assumed, and only the `linux-amd64` release asset is fetched.
- **Flows other than the two rule 12 names, in forward.** A delegate and the two hook kinds are what a forward run drives; a contract break that touches neither would not be seen. A reverse run adds its probe's one stimulus and nothing more.

## Options worth knowing

| flag | |
| --- | --- |
| `--branch` | required; the branch under test, fetched from `--repo` |
| `--previous` | the previous release tag (default `v0.41.0`) |
| `--old-binary` | use a binary already on disk instead of downloading a release (its version is recorded, not enforced) |
| `--endpoint-mode` | `sandbox-sockets` (default) or `resolved` |
| `--unset-xdg-runtime-dir` | false green 5 |
| `--experimental` | turn the experimental feature flag on for the run |
| `--direction` | `forward` (default, rule 12's pairing), `reverse` (branch daemon, previous-release TUI and CLI) or `both` |
| `--probe` | the reverse run's branch-specific stimulus: `auto` (default, from the branch's issue number), `generic` (the four tells only), or a probe's name |
| `--skip-build` | reuse whatever is already in the target dir; for iterating on the harness itself. The evidence file then records that the binary was not rebuilt, and which commit its build id names, if any |
| `--keep-sandbox` | keep the sandbox even on a pass |
| `--min-free-gib` | the free-space floor (default 100) |
| `--run-timeout-secs` | kill the whole namespace if the inner half has not finished (default 1200) |
| `--source-clone`, `--target-dir`, `--runs-root`, `--releases-dir`, `--evidence` | override the default paths |

## When a run finds a real break

Report it and do not paper over it. A failing tell, or a missing mismatch prompt, is the harness saying the branch and the previous release did not interoperate — which is precisely the question rule 12 asks, and a `FAIL` here is the most valuable outcome a run can have.
