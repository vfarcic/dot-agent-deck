# RWX: the Linux CI backend switch and sandboxed test runs

This repository can use [RWX](https://www.rwx.com) in two places, each behind its own switch. Both are off unless someone turns them on, and turning either off restores the plain GitHub Actions and local setup without touching anything else.

| switch | where | on | off |
| --- | --- | --- | --- |
| Linux CI backend for pull requests | the file `.github/ci-backend` | `rwx` | `gha`, or delete the file |
| test commands of `cargo xtask affected-checks --run` in an RWX sandbox | the environment variable `DAD_REMOTE_EXEC` | `rwx` | unset |

## The CI backend switch

**Where each job runs.** `.github/ci-backend` decides which system runs a **pull request's** Linux jobs. Pushes to `main` run everything on GitHub Actions whatever the file says, so `main`'s post-merge verification and the cache saves from #1743 never depend on RWX.

| job | pull request, `ci-backend=rwx` | pull request, `ci-backend=gha` | push to `main` |
| --- | --- | --- | --- |
| every Linux job in `ci.yml` (`fmt`, `clippy`, `build-release`, `test`, `e2e-build`, the `e2e` shards, `desktop-web`, `desktop-browser`, `desktop-driver`, `windows-cross-check`, `security`, `nix`, `devbox`) and `aarch64-crossbuild-check.yml` | RWX | GitHub Actions | GitHub Actions |
| `release-lib` in `ci.yml` (advisory) | RWX, as `.rwx/release-lib.yml` (status `RWX: release-lib`) | GitHub Actions | GitHub Actions |
| `cross-version` | GitHub Actions | GitHub Actions | GitHub Actions |
| `build-macos` | not run | not run | GitHub Actions |
| `build-windows` | `if: false` until Windows ships | | |
| `changes`, `notify-main-red`, `docs.yml`, the reviewer, labeling, releases | GitHub Actions | GitHub Actions | GitHub Actions |

The exceptions, and why:

- **`cross-version` stays on GitHub Actions.** Its harness contains the builds in unprivileged bubblewrap namespaces, and inside an RWX task bwrap cannot set up the network namespace (measured 2026-10-22: `bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`).
- **`release-lib` moves to a run definition of its own, `.rwx/release-lib.yml`.** RWX reports one status per run and any failing task fails it, so a task in `.rwx/ci.yml` would be part of the required `RWX: linux`; the job is advisory ([release-lib-tier.md](release-lib-tier.md)).
- **`docs.yml` stays.** `docs-preview.yml` deploys the preview from its uploaded artifact through `workflow_run`, which only a GitHub Actions run can feed.
- **`build-macos` runs on `main`, not on pull requests.** RWX has no macOS runners, and on a pull request it was the slowest required check (5 to 18 minutes) for a 3% rate of macOS-only failures (545 runs, 2026-10-06 to 10-21). A macOS-only defect now surfaces on `main`: whoever merged watches that run and owns a red (the `merge-pr` skill), `notify-main-red` opens an issue, and `tag-release.yml` refuses to release a `main` whose run is not green.

**How the switch works.** Both systems trigger on every pull request and both read the file at the **PR's head commit**, so they always make the same choice (GitHub Actions tests the merge commit, but reading the switch there would let a PR cut before `main` switched leave both sides skipping):

- **GitHub Actions** (`.github/workflows/ci.yml`): the `changes` job reads the file through the API and exposes `linux_backend`. When it says `rwx`, each Linux job's `if:` is false, so the job is skipped, and a job skipped by its own `if:` satisfies a required check. `aarch64-crossbuild-check.yml` has its own copy of that read.
- **RWX** (`.rwx/ci.yml`): the `backend` task reads the file. Unless it says `rwx`, every check task is skipped and the run passes in seconds, so RWX's check, `RWX: linux`, can be required whichever side is active.
- **Both skip the same jobs for the same change**: `scripts/ci-changes.sh` holds the path classification (a Renovate PR touching only the devbox or flake files, whether the change feeds the binary, whether the manifests changed), and both call it. GitHub Actions takes the PR's files from the API (a renamed file by both its paths); RWX computes them against the merge-base with the PR's base with `git diff --no-renames`, which is the same list.

Flipping the switch is a one-line pull request, and it takes effect on that pull request itself.

### What to know before turning it on

- **The ruleset has to require `RWX: linux`.** With the switch on, `build` and `e2e-deterministic` pass as skipped, so nothing blocks a merge on the Linux verdict until RWX's check is required too. `scripts/apply-branch-protection.sh`'s default list carries it, and the agent reviewer reads commit statuses (which is how RWX reports) as well as check runs. Applying the ruleset needs an admin, and only after `.rwx/ci.yml` is on `main`: a pull request whose branch lacks it gets no RWX run, so `RWX: linux` never reports and it cannot merge until it merges `main`.
- **Switch back before the credits run out.** A pull request that flips the file back to `gha` still runs on RWX until it merges; if RWX can no longer run, that PR's required RWX check never passes, and merging it needs an admin bypass. The same holds for removing `RWX: linux` from the ruleset.
- **RWX tests the PR's head commit; GitHub Actions tests the PR merged into its base.** A PR that is green on its head and red once merged with a moved `main` is caught by the overlap check before a merge (CLAUDE.md rule 8) and by `main`'s push run.
- **A pull request from a fork needs someone to start its RWX run by hand** in the RWX UI.
- **Tool caches live in the vault `dot_agent_deck_rwx_eval`, which must be LOCKED with `main` as its only permitted branch** (RWX UI: Vaults, then the vault, then Access Control), and whose "Allow public repositories" setting must stay on for GitHub-triggered runs on this public repository. RWX lets a run write a tool cache only when the vault is unlocked for it, so with that setting a pull request's run reads the caches and cannot write them, and a PR's build code cannot plant files that a later PR restores (the counterpart of `ci.yml` saving Rust caches only from `main`, #1743). The caches are written by `.rwx/ci.yml`'s daily cron (`main-tool-caches`, 04:00 UTC), which RWX runs on `main` and which keeps them inside their 48-hour lifetime. Until that first cron run after a lock, PRs build without caches (slower, not broken).
- **Versions stay in step with local.** `.rwx/ci.yml` and `.rwx/sandbox.yml` pin the toolchain a third time, after `devbox.json` and the workflows. renovate.json's RWX customManagers bump them in the same grouped PRs as the other copies (every RWX bump is held for a human), and `scripts/check-pin-lockstep.sh` fails `cargo test-fast` when the Rust toolchain, nextest, pnpm or go-task in `.rwx/` disagree with `devbox.json`. RWX's own package versions (`git/clone`, `nodejs/install`, `rust-lang/install`) have no Renovate datasource and are bumped by hand.
- **Reading a red:** RWX's failures are not in `gh run view`. `gh pr checks <n>` lists `RWX: linux` with a link to the run, and `rwx results <run-id>` (or `rwx logs <task-id>`) shows what failed.

## Sandboxed runs for agents

The point is twofold: the checks finish sooner, and the machine the agents run on stays free for them. With `DAD_REMOTE_EXEC=rwx`, `cargo xtask affected-checks --run` runs **every `cargo` command except `cargo fmt`** (so clippy and the test tiers, every compile) through `rwx sandbox exec --`, in the sandbox `.rwx/sandbox.yml` defines. `cargo fmt` stays local because it compiles nothing.

Measured on 2026-10-21, the full gates for a one-line Rust change, from an M-series Mac:

| | wall | CPU used locally |
| --- | --- | --- |
| all local | 512 s | 1,168 s |
| sandbox, cold (new sandbox) | 270 s | 15 s |
| sandbox, warm (kept from the previous run) | 143 s | 15 s |

The local run also failed ten tests that pass serially and on Linux, which is the machine being contended rather than the code.

It runs locally instead, and prints a note saying why, when the `rwx` CLI is missing, `rwx whoami` fails, `.rwx/sandbox.yml` is absent, or the variable names anything other than `rwx`. An unset variable prints nothing and changes nothing.

**Setting it on a box.** Every agent a deck daemon starts inherits the daemon's environment, so the variable has to be in the environment the daemon starts from, not only in one shell. On a box set up with `scripts/box/bootstrap.sh`, `--remote-exec` exports `DAD_REMOTE_EXEC=rwx` in the same three places `--use-vals` exports `USE_VALS`: a marked block at the top of `~/.bashrc`, `~/.config/environment.d/999-dad-box.conf`, and the running systemd `--user` manager. Like `--use-vals`, it sticks across re-runs while its block is in `~/.bashrc`, and a daemon that is already running gets it only once it is stopped and started from a fresh login. To turn it off, delete the block and re-run. The flag is opt-in because the sandbox's compute is billed to the account the `rwx` CLI is logged in as. The script installs the CLI either way, and logging it in (`rwx login`) is the operator's step. Until then the helper runs locally with the note above, and the run's closing list of logins still to do names `rwx`. Before the flag existed, the variable reached the agents only when whoever started the daemon had set it. In #1980's window it had been set, but nothing in the repository made sure of that (#2097).

With the standard `OTEL_*` variables set, as they are in an agent's shell under a deck with telemetry on, each `--run` also sends a trace recording which runner each command used, why a requested runner fell back, and how long each command took, so time with and without the sandbox can be compared across many runs rather than from one table: [gate-telemetry.md](gate-telemetry.md).

### For agents

When `DAD_REMOTE_EXEC=rwx` is set, run the commit gates through `cargo xtask affected-checks --run` rather than command by command, and run every other `cargo` command that compiles as `rwx sandbox exec -- <command>`: a build, a filtered `cargo test-e2e`, and a `cargo test-fast` or `cargo nextest` run filtered to a single test, since a filter narrows what runs, not what compiles. Unlike the helper, `rwx sandbox exec` does not fall back to a local run, so when `rwx whoami` fails run the command locally and say so. That includes `cargo mutants`, which `.rwx/sandbox.yml` installs at the version `/code-cleanup`'s `find-targets.sh` pins (until 2026-10-31 it was not installed, so mutation runs stayed on the shared box, and on 2026-10-27 one with `-j 5` drove its load to 80, mostly disk wait); still run it without raising `--jobs`, since each job is a separate full rebuild and link. Leave the sandbox running when the command is done: a team's agents share their worktree's sandbox, and a stop kills a command a teammate is running in it. These instructions are repeated where agents read them first: the coder and tester role templates in `.dot-agent-deck.toml` and the gates that `/issue-queue` and `/prd-queue` put into a dispatched unit's task, since units given them only through CLAUDE.md rule 5 ran heavy commands locally with the switch on (on 2026-10-25 one ran `cargo nextest` and `cargo test-e2e` locally). Those places said "heavy" until 2026-10-30, when two units given the instruction ran a single-test `cargo nextest run <filter>` locally, not counting it as heavy, and took the shared box from a load of 2 to 26 in minutes; they now name the filtered runs. In #1980's window (2026-10-30 to 11-03) every gate run had the switch on and sent its compiles to RWX, yet Codex testers, whose role template named only the test commands, ran `cargo clippy` locally 42 times, and a coder ran a nine-minute `cargo check` locally (#2097); the templates and the queue skills now name `cargo check` and `cargo clippy` as well, and the tester's template names `cargo clippy`, `cargo check`, `cargo build` and `cargo mutants` alongside the test commands. **Never run lane 2 (`cargo test-e2e-live`, or any command with the `e2e-live` feature) or anything else that reaches a real agent in the sandbox**: no agent credential is placed there, and CLAUDE.md rule 5's reasons for keeping credentials off remote runners apply to it as much as to CI.

### Stopping

A sandbox is billed per second while active, idle included, and it stops itself after 5 minutes without activity (`--inactivity-timeout` in `.rwx/sandbox.yml`); a running command counts as activity, so that cannot cut a long run off. Nothing else stops it by default, `--run` included: one unit's coder and tester work in the same worktree and so share its sandbox (RWX keys it by branch and config path), and until 2026-10-31 `--run` stopped the sandbox when it finished and the templates told agents to stop it after each command, so one agent's stop killed the command the other was running in it ("container does not exist", "cannot exec in a stopped container"), and every run after a stop started cold (the table's cold row rather than the warm one). Leaving it running costs at most five idle minutes after the last command. `DAD_REMOTE_EXEC_KEEP=0` makes `--run` stop it when it finishes, for a run you know nobody shares; that stop checks `rwx sandbox list` and stops it a second time, because `rwx sandbox stop` can time out and leave the sandbox active.

The sandbox sees uncommitted changes in the worktree, so it stands in for a local run before a commit.

### The version the sandbox's builds report

The sandbox's checkout is a shallow clone with no tags, so `build.rs`'s `git describe` finds no version there, and a build with neither `DAD_VERSION` nor a tag reports `0.1.0+unknown` (issue #326), which `remote upgrade` without `--version` refuses. So `.rwx/sandbox.yml`'s `version` task reads the newest `vX.Y.Z` tag at origin with `scripts/latest-release-version.sh` when the sandbox starts and sets it as `DAD_VERSION` for every command the sandbox runs, the way `.rwx/ci.yml`'s `backend` task and `ci.yml`'s `changes` job give CI's builds theirs, with the same script. That is the version a developer's checkout of `main` reports, and without it 11 lane-1 tests in `tests/e2e_remote_upgrade.rs` (`remote_upgrade_001` to `007`, `remote_connect_001` to `004`) fail in the sandbox and pass everywhere else (issue #2052). The task fails sandbox setup when origin has no such tag, rather than letting every build fall back to the placeholder. A sandbox keeps the value it started with, so a release tagged while it is running reaches it only after `rwx sandbox stop` or `rwx sandbox exec --reset`. To test a build whose version is unknown in the sandbox, unset the variable for that command: `rwx sandbox exec -- env -u DAD_VERSION <command>`.

### The export chain

The composer probe's export path rule (`verify_export` in `src/composer_probe/path_rule.rs`) walks the export's path from `/` and refuses unless every directory on it, `/` included, is on ext2/3/4, XFS, Btrfs or tmpfs. An RWX task's `/` is overlayfs (`f_type` `0x794c7630`), so in a task as RWX starts it every export is refused, the deck never claims an input box empty, and every e2e test that needs an empty check takes the unsupported branch (`probe_026` and `probe_046` print which branch ran). That is the rule working, and it is not to be relaxed for CI.

So `.rwx/ci.yml`'s `e2e` task runs its tier under `scripts/rwx-export-root.sh`, which builds a root the rule accepts for that one command. It makes a directory on the task's XFS mount (the parent of `$RWX_ARTIFACTS`, `/opt/rwx/task-data/<task id>`, which RWX backs with the same disk as the task's overlay), and in a private mount namespace builds a new root there: `/` and `/var` are plain directories (root's, 0755), `/tmp` and `/var/tmp` empty ones (root's, 1777), and every other entry of `/` and `/var` stays the task's own, bind-mounted or re-created as the same symlink. It then chroots into that root as the calling user, with that user's groups, working directory and environment variables (those bash can export by name; an exported function is not carried) and no capabilities. The environment never touches the disk: the script reads it as root from `/proc/<pid>/environ` of the `sudo` it execs into and hands the command an open descriptor to it. A variable that points into the task's own `/tmp` or `/var/tmp` (`TMPDIR`, `SSH_AUTH_SOCK`) now points into the new, empty directories, so whatever it named there is not visible. When the command exits, or the script gets `TERM`, `HUP` or `INT` (which it forwards to the command, then waits for the command to exit), it unmounts the binds and deletes only what it made: the contents of the new `/tmp` and `/var/tmp`, the entries it created, and the directory. It removes nothing at all unless the mount table shows nothing still mounted under the root; when a mount will not detach, or the table cannot be read, it leaves the directory and prints `rwx-export-root: left <dir> in place: ...`. The harness's default temp base, `/var/tmp/dad-e2e-<uid>`, and each fixture's `TMPDIR` under it, then sit on a chain the rule accepts. The script needs `sudo` with `CAP_SYS_ADMIN` and `CAP_SYS_CHROOT`, which an RWX task's has, and refuses to run unless `RWX=true`. `ci.yml` does not use it: a GitHub runner's root is ext4.

To run e2e tests the same way in the sandbox: `rwx sandbox exec -- scripts/rwx-export-root.sh cargo test-e2e <filter>`.

A green run under the script does not show that the tests took the supported branch: lane 1 passes on the unsupported one too. The check is the lines `probe_026` and `probe_046` print, which read `export-chain branch: supported / Empty ...` on the supported branch.

What is different inside the chroot, and what can be left behind:

- `/proc/mounts` and `/proc/self/mountinfo` list the new root and every bind under it as mounts of their own, nested under the `dad-export-root.*` directory's path on the task-data mount. A test that reads the mount table sees those entries, not the task's original table.
- A `SIGKILL` of the script, or the task or sandbox dying under it, skips the cleanup and leaves `/opt/rwx/task-data/<task id>/dad-export-root.*` behind, with whatever the run wrote to its `/tmp` and `/var/tmp` (several GB for a full shard). So does the `left <dir> in place` warning. `cargo xtask clean-e2e-tmp` does not look there, since it scans `/var/tmp/dad-e2e-<uid>` and the system temp dir. In a CI task the directory goes with the task; in the sandbox it stays until the sandbox stops. To remove one by hand, first confirm nothing is mounted under it (`findmnt -R <dir>` prints nothing, which holds once the script's processes are gone, since the binds lived in their own mount namespace), then `sudo rm -r --one-file-system -- <dir>`.

Routes that were measured and do not work (2026-11-03): pointing `TMPDIR` or `DAD_E2E_TMPDIR` anywhere, since `/` itself is refused; an unprivileged user namespace (`unshare --user` is denied); an ext4 image, since the task has no loop devices; and a tmpfs root, the script's first version (RWX run `01a1a2c522be782eb2580f0d1d1e6f3b`): one shard leaves about 37 test roots of about 150 MB each under `/var/tmp`, which filled an 8 GB tmpfs until the harness's free-space check refused the rest of the shard, and `cargo xver`'s scratch check refuses a RAM-backed `/` (CLAUDE.md rule 14).

What it relies on: the task-data mount is RWX's, undocumented, and also holds `RWX_ARTIFACTS` and RWX's other control directories, beside which the script adds its own. If RWX moves it or takes it off a disk-backed filesystem, the script stops with the filesystem it found rather than running the tier on the unsupported branch; `DAD_EXPORT_ROOT_BASE` names another directory.
