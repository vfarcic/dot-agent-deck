# The cross-version harness — running rule 12 as a command

CLAUDE.md rule 12 requires, for any change touching the daemon, the TUI↔daemon protocol, orchestration or hooks, a **cross-version manual test**: build the branch, start a daemon from the previous release with an agent under it, run the branch TUI against that older daemon, and confirm a delegate still routes and hooks still arrive. `cargo xver` does that, scripted.

```sh
cargo xver -- --branch agent/dispatch-issue-1121
```

## Why this exists

Rule 12's procedure is written in keystrokes because a person was always going to run it, and four of its paragraphs are about ways a person silently gets it wrong. That had a consequence nobody intended: a dispatched agent reading rule 12 concludes it cannot type into two full-screen TUIs, declines the check, and leaves the review thread open. Eight green, mergeable pull requests were blocked on exactly that one thread at the time this was written.

**Rule 12's requirement is the scenario, not human fingers.** A driver that reproduces the same scenario and asserts the same tells discharges the same obligation — and unlike a person's recollection, it leaves a file a reviewer can read afterwards.

## What a run does

1. Builds the branch in a **reusable, disk-backed worktree** at `../dot-agent-deck-xver`, with a `CARGO_TARGET_DIR` at `../dot-agent-deck-xver-target` that is reused across branches so the cargo cache survives. The checkout is **detached at `FETCH_HEAD`**, so nothing in that worktree can commit, amend, rebase or push to the branch it is verifying — and a branch already checked out in somebody else's worktree still works.
2. Downloads the previous release's published `dot-agent-deck-linux-amd64` with `gh release download`, caches it under `../dot-agent-deck-xver-releases/<tag>/`, and asserts it really reports that version before trusting it.
3. Creates a fresh sandbox under `../dot-agent-deck-xver-runs/<branch slug>-<epoch>/` and writes a three-role `.dot-agent-deck.toml` into it.
4. Starts the **old** daemon there, capturing its pid.
5. Attaches the **old** TUI over a PTY and opens the orchestration through the production `Ctrl+N` flow, then confirms through `daemon status --json` that all three role panes and their processes are genuinely live.
6. Closes that TUI with `Ctrl+D`, `Ctrl+C`, **Detach** — then re-checks that the daemon pid is alive and still lists three roles.
7. Attaches the **branch** TUI over a PTY, waits for the build-version mismatch prompt, records it, and **declines** it with `n`.
8. Delegates from inside the orchestrator pane, then issues `work-done` and — last — `agent-event --type running` from inside a worker pane.
9. Tears the sandbox daemon down **by pid**.
10. Writes a markdown evidence file to `.dot-agent-deck/xver-evidence/<branch slug>.md`.

## The four tells

A run asserts these and prints each one with the value it was decided on, because the evidence file is what resolves a review thread and a reader who did not watch the run has to be able to check it.

| tell | what it rules out |
| --- | --- |
| exactly one `Attach protocol listening` line in the sandbox log | two means the branch TUI lazy-spawned its own daemon — the tell for *both* the no-agents cause and the 30-second idle-window cause, whichever swallowed the run |
| the same daemon pid, and `/proc/<pid>/exe` still the old binary, at the start and at the end; plus `ss -xlp` naming that pid as the endpoint's owner at both ends | the daemon was replaced partway |
| a delegate still routed | the payload really landed in the target pane |
| hooks (work-done, status) still arrived | the daemon's feedback really reached the orchestrator's pane, and the status change really reached its own state |

A tell the harness could not measure is reported as **not checked** and the run is **INCOMPLETE**, not a pass. Tell 2's ownership half is the one branch that reaches that today: on a host where `ss -xlp` is absent or unparseable, who owns the endpoint is not measured, and the harness says so rather than counting it either way.

## The five false greens, and where each one is handled

Rule 12 documents four ways this procedure silently measures nothing; PR #1179 added a fifth. A false green here is worse than not running the check at all, because it clears a gate with nothing behind it.

1. **No agents under the old daemon.** With zero agents the branch TUI takes `MismatchAction::SilentRestart` — it SIGTERMs the old daemon and lazy-spawns its own, with no prompt and no output. Step 5 above is what avoids it, and the missing mismatch prompt is what catches it: the harness treats "no prompt" as a hard failure of the run rather than as a pass, because the prompt appearing *is* the proof the scenario was reached.
2. **The 30-second idle window.** `DEFAULT_IDLE_SHUTDOWN_SECS` is 30, so more than 30 s between `daemon serve` and the first attach and the daemon exits and gets replaced. Every process in the run carries `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0`, which is the documented "always on" production value rather than a test hook.
3. **`Ctrl+C` in `PaneInput` mode goes to the pane.** With a role pane focused it kills that role's process and the orchestration comes back one role short. The harness sends `Ctrl+D` first, takes the default `Detach` (never `Stop`), and then re-asserts the three-role list — this one fails loudly rather than silently, and the re-assert is what makes it loud here.
4. **Teardown by an unscoped `pkill`.** See [Teardown](#teardown-is-by-pid-and-refuses-a-recycled-one) below.
5. **`XDG_RUNTIME_DIR` being set, for a change that moves the endpoint path in the fallback case only.** On a normal desktop session it is set, both builds resolve byte-identical endpoints, and the run exercises the arm such a change did not touch. `--unset-xdg-runtime-dir` is the lever, and it is an option rather than a hardcode because for every other kind of change the ordinary desktop configuration is the faithful one.

## The two endpoint modes

`--endpoint-mode sandbox-sockets` (the default) pins `DOT_AGENT_DECK_SOCKET` and `DOT_AGENT_DECK_ATTACH_SOCKET` inside the sandbox. This is the strongest isolation available and the right choice for any change that does not touch endpoint resolution. Runs in this mode have their own socket paths, their own sandbox and their own log, so several can execute at once.

`--endpoint-mode resolved` sets neither override, so both builds resolve their endpoint the way they would on a real host. **Use it only when the change under test *is* endpoint resolution** — those overrides short-circuit resolution before the logic under test runs, so a run that sets them measures the wrong thing. Two things follow and both are the caller's to respect:

- A pre-#1121 build hardcodes `/tmp/dot-agent-deck-{uid}.sock` and `/tmp/dot-agent-deck-attach-{uid}.sock` and ignores `TMPDIR`, so in this mode the old daemon binds **process-global** paths in the real `/tmp`. The harness refuses to start when either already exists. On teardown it asks `ss -xlp` which of them **this run's daemon pid** is the listener on, *before* signalling it, and removes only those — a socket it cannot attribute to its own pid is left alone and named in the run log, because deleting both on the strength of the preflight would remove a stranger's entry in exactly the case that matters.
- **Two `resolved` runs cannot execute concurrently**, and neither can a `resolved` run and anything else on the host already using the fallback case. A `sandbox-sockets` run is unaffected either way.

For issue #1121 the invocation is both levers together:

```sh
cargo xver -- --branch agent/dispatch-issue-1121 --endpoint-mode resolved --unset-xdg-runtime-dir
```

In that mode a run also records, in its log, whether the branch build created an endpoint directory of its own (`$TMPDIR/dot-agent-deck-{uid}`) while it was attached to the old daemon at the legacy address. That is an observation rather than a fifth tell: it checks one change's claim about itself — that its compatibility read is read-only — where the four tells are what rule 12 asks of every change.

## Running several at once

Each run mints its own sandbox directory, so the sandbox is never the thing that collides. Two other resources are shared by default and are what to override:

- **The worktree and the cargo target dir.** Two runs would check out different commits into `../dot-agent-deck-xver` and build into `../dot-agent-deck-xver-target` at the same time. Give each concurrent run its own `--worktree` and `--target-dir`. The cache benefit is per-target-dir, so a fixed set of lanes (`-xver-a`, `-xver-b`, …) reused across branches keeps it.
- **The release cache.** `--releases-dir` is read-mostly once populated, and a run reuses an existing binary rather than re-downloading. Two runs racing the *first* download of the same tag would both write into that directory; pre-warm it with one run, or give each lane its own.

And the mode decides the rest: any number of `sandbox-sockets` runs can execute together, while a `resolved` run is exclusive for the whole host — its addresses are in the real `/tmp` and are not per-run.

## What the sandbox isolates, and why each variable is on the list

Every process in a run — the daemon, both TUIs and every CLI call — gets a complete environment built from scratch rather than overlaid on the caller's, because two of the things a run has to pin are *absences* and an absence cannot be expressed by adding a variable.

| variable | why |
| --- | --- |
| `HOME`, `XDG_CONFIG_HOME`, `TMPDIR` | ordinary isolation |
| `DOT_AGENT_DECK_STATE_DIR`, `DOT_AGENT_DECK_LOCK_DIR` | per-user state and the per-endpoint lock |
| `DOT_AGENT_DECK_LOG` | **resolved separately from the state dir.** Without it an otherwise-isolated daemon appends into your real `~/.local/state/dot-agent-deck/deck.log`, and two interleaved daemons in one file are genuinely hard to attribute afterwards |
| `DOT_AGENT_DECK_SESSION` | otherwise the sandbox deck restores your real saved session |
| `DOT_AGENT_DECK_SCHEDULES` | otherwise the sandbox daemon fires **your real scheduled tasks** a second time at startup, and a registered schedule also keeps it from ever idling out |
| `DOT_AGENT_DECK_EXPERIMENTAL` | project-config discovery walks up from **cwd**, not from `$HOME`, so a sandbox at a sibling path still reads your real `.dot-agent-deck.toml` feature flags. Pinned explicitly (off by default, `--experimental` to turn it on) rather than inherited |
| `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` | false green 2 above |
| `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` | a stand-in agent that escapes its process group self-exits rather than leaking to PID 1 |
| `PATH` | the branch build's directory first, so a pane that shells a bare `dot-agent-deck` reaches the **branch** binary while the daemon in memory is still the old one. That models the upgrade this test is about rather than being a convenience |

The sandbox root and the cargo target dir are both checked against `/proc/mounts` and refused if they are on a tmpfs, and refused below a free-space floor (`--min-free-gib`, default 100). CLAUDE.md rule 14 is why: a build on a tmpfs dies at link time with a misleading `error: linking with 'cc' failed`, or gets its `rustc` OOM-killed, and nothing in either message points at the filesystem.

## Teardown is by pid, and refuses a recycled one

On 2026-09-15 an agent finishing with a cross-version sandbox ran `pkill -f "daemon serve"`. That pattern also matches the **production** daemon's own command line, which took the SIGTERM 1.42 s later and gracefully stopped nine panes across three dispatched units ([#428 occurrence #5](https://github.com/vfarcic/dot-agent-deck/issues/428#issuecomment-5688814518)). `pkill` sends SIGTERM, so it goes *around* issue #770's `daemon stop` refusal rather than having to defeat it: a compliant daemon shuts its agents down cleanly and names none of them.

Nothing in this harness takes a name. It signals only a pid it spawned itself, captured at spawn; and immediately before signalling it re-reads `/proc/<pid>/cmdline` and requires it to still contain the path of the binary it launched. A pid that no longer passes that check is **not signalled** — it is recorded in the evidence file as refused — which is what stands between a pid recycled since the capture and somebody else's process.

One thing to know if you pass `--old-binary`: that path becomes the string the re-check looks for. Point it at a build you own rather than at an installed one, so the check stays as specific as it is with the cached release binary.

## What it covers, and what it does not

It covers the pairing rule 12 and [`versioning.md`](versioning.md) exist for: a **newer TUI against an older daemon**, over the real attach protocol, with real orchestration state in the old daemon's memory, asserting on payloads rather than on exit codes.

It does not cover:

- **A real agent.** Every role is a stand-in (`sh`, `cat`), deliberately: this check is about the TUI↔daemon wire, so no *agent* credential is used or needed. (`gh release download` uses your GitHub credential, and `--old-binary` avoids even that.) It therefore says nothing about how a real agent behaves across the version boundary, and it is not a substitute for the lane-2 real-agent tests CLAUDE.md rule 4 asks for.
- **The reverse direction.** An *older* TUI against a *newer* daemon is a downgrade, and this harness does not stand one up.
- **The desktop GUI.** `classify_handshake` compares `PROTOCOL_VERSION` and `CONTRACT_BREAKS` and is a different code path from the TUI's build-version handshake; nothing here exercises it.
- **More than one previous release per run.** `--previous` takes one tag.
- **Anything that is not Linux.** `/proc`, `ss(8)` and `statvfs` are all assumed, and only the `linux-amd64` release asset is fetched.
- **Flows other than the two rule 12 names.** A delegate and the two hook kinds are what a run drives; a contract break that touches neither would not be seen.

What it does to git is bounded and worth stating exactly, because the branches it runs against are being verified rather than changed: it runs `git worktree add --detach` once for the reusable worktree, `git fetch origin <branch>` (which moves remote-tracking refs and nothing else), and `git checkout --detach FETCH_HEAD`. It never commits, amends, rebases or pushes, and it posts nothing to GitHub.

## Reading the evidence file

`.dot-agent-deck/xver-evidence/<branch slug>.md` (override with `--evidence`) carries: both builds' `daemon hello` output verbatim, the sandbox and binary paths, the daemon pid, each tell with the measured value it was decided on, a numbered run log, and raw excerpts — the mismatch prompt as printed, the target pane's screen after the delegate, the orchestrator's screen after the hook, and the tail of the sandbox log. It is written on **every** path, including a run that broke down partway, because a run that broke down is a useful result and the file is where it is legible.

The sandbox directory is removed after a pass and kept after anything else; `--keep-sandbox` keeps it either way. Its `artifacts/` holds both PTY streams verbatim and the old daemon's stdio.

## Options worth knowing

| flag | |
| --- | --- |
| `--branch` | required; the branch under test, taken from `origin/<branch>` |
| `--previous` | the previous release tag (default `v0.41.0`) |
| `--old-binary` | use a binary already on disk instead of downloading a release |
| `--endpoint-mode` | `sandbox-sockets` (default) or `resolved` |
| `--unset-xdg-runtime-dir` | false green 5 |
| `--experimental` | turn the experimental feature flag on for the run |
| `--skip-build` | reuse whatever is already in the target dir; for iterating on the harness itself |
| `--keep-sandbox` | keep the sandbox even on a pass |
| `--min-free-gib` | the free-space floor (default 100) |
| `--worktree`, `--target-dir`, `--runs-root`, `--releases-dir`, `--evidence` | override the default paths |

## When a run finds a real break

Report it and do not paper over it. A failing tell, or a missing mismatch prompt, is the harness saying the branch and the previous release did not interoperate — which is precisely the question rule 12 asks, and a `FAIL` here is the most valuable outcome a run can have.
