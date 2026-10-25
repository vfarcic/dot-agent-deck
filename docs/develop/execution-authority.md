# Execution isolation and agent authority: discovery record (PRD #634)

This is the evidence behind PRD #634's decision: what authority an agent started by the deck holds today, what each candidate isolation mechanism actually constrained when it was tried, the posture names that survive that evidence, and the `Revise` decision with its follow-up PRDs. The PRD itself, its progress notes and the decision record live in [issue #634](https://github.com/vfarcic/dot-agent-deck/issues/634). This page is the long-form evidence those comments link to.

## Read this first: what this page describes and how long it stays true

- **Snapshot.** The code inventory is of `origin/main` at `e754ea08` (v0.48.1), read on 2026-10-25. The measurements were taken the same day on one Linux host (kernel 7.0, Ubuntu with `kernel.apparmor_restrict_unprivileged_userns=1`), from a pane the deck had dispatched into a linked worktree. Agent versions: Claude Code 2.1.310, codex-cli 0.164.0, OpenCode 1.18.35. **Every `file:line` citation on this page refers to `e754ea08`**, not to a later `main`: #1898 alone moved hundreds of lines in the cited files the same day. Read a citation with `git show e754ea08:<path>`, or follow the symbol named beside it.
- **PRD #1666 part 3 is rewriting much of what this inventories.** It owns process spawn (`src/spawn.rs`, `src/agent_pty*.rs`), agent messaging (delegation, `work-done`, inbox, doorbell, dispatch returns), every agent's hooks, the client↔daemon protocol and `close`. The findings most likely to move when it merges are marked **[moves with #1666]** below. The ones that follow from git, POSIX, the agents' own designs or the config loader are not expected to move with it, and are the ones the decision leans on.
- **Two things changed on `main` the day this was written, after the snapshot.** #1898 (PRD #1666 part 3a) made `work-done` task-scoped (`work-done --task-id <id>` completes exactly that delegated task) and changed how a pane's messages are admitted. #1899 fixed #1888, so the deck's own git commands now run with hooks and `core.fsmonitor` off. The page keeps the snapshot as measured and marks the statements these two change as *fixed since*.
- **Nothing here changes behaviour.** The prototypes were scratch scripts outside the repository and were not committed. Every implementation step is a follow-up issue (see [Follow-ups](#follow-ups)).

## The model today in one sentence

The deck has exactly one principal, the OS user the daemon runs as. Every agent, hook, CLI call, attach client and remote ssh session acting as that user holds that user's full authority, and the deck's own checks (socket mode `0600`, hook capability tokens, orchestrator role checks) are integrity and mistake guards inside that principal, not boundaries between agents. The code says so where it matters: `StartAgent` "deliberately do[es] **not** sandbox `command`, `cwd`, or `env`" because the attach socket's peers "can already exec arbitrary code as that user" (`src/daemon_protocol.rs:2032-2042`), and hook provenance "is not a boundary against a deliberate same-uid adversary, and it cannot be" (`src/hook_provenance.rs:38-46`). That is coherent for one person on one machine. The rest of this page is about where it stops being coherent.

## 1. Inventory: authority from configuration load to cleanup

### 1.1 Project configuration is trusted input that executes

- **Loading.** `load_project_config(dir)` reads `dir/.dot-agent-deck.toml` with no trust check, size cap or validation (`src/project_config.rs:1033-1061`). Only the `[features]` lookup walks up from the launch directory, stopping at the first regular file owned by the current uid (`src/config.rs:1782-1830`). A repository you cloned is owned by you, so that ownership check does not separate a cloned repository from your own.
- **Execution.** A role's `command` is a free string run as `<$SHELL> -c <command>` when it contains whitespace, or exec'd directly when it is one word (`src/agent_pty.rs:1736-1743`, `src/platform/shell.rs:30-41`). `validate_config` runs only in `dot-agent-deck validate` (`src/main.rs:2493`), never before a spawn.
- **When it runs**, with no prompt, preview or allowlist at any of these points:
  - choosing an orchestration in the New Agent form (TUI or desktop), which spawns every role (`src/tab.rs:803-831`); the desktop is never sent the commands at all (`src/event.rs:2493-2508`);
  - session restore at startup, which re-reads the config and checks role *names* and the directory, not commands (`src/ui.rs:11749-11826`);
  - every delegate to a `clear = true` role, restart-role and spawn-role, each of which re-reads the file (`src/state.rs:5099-5111`, `5858-5914`);
  - every scheduled fire, where the target directory's orchestration wins over the task's own command (`src/spawn.rs:492-510`);
  - every issue-dispatch fire, which spawns roles from the config on the **remote repository's default branch** after `gh repo clone` and `git pull --ff-only` (`src/issue_dispatch_run.rs:1049-1075`, `1253-1287`), and runs that branch's `devbox` warm-up first (`src/issue_dispatch_run.rs:2448-2456`).
- **Prompt channel.** The start role's `prompt_template` is copied verbatim into the orchestrator's context, followed by every role's `name` and `description` (`src/orchestrator_context.rs:67-90`), and each worker's `prompt_template` is prepended to every task delegated to it (`src/state.rs:5091-5096`). A repository's config therefore writes instructions into agents as well as commands into shells. **[moves with #1666]** for the exact assembly; the channel itself is the config's purpose.
- **What is not there.** No trusted-projects list, no hash pinning, no "this project wants to run X" confirmation. `config_write_guard` and `docs/develop/agent-config-containment.md` protect your real agent configuration from *tests*, and are inert for users (`src/config_write_guard.rs:1-80`). `src/untrusted_text.rs` makes repository text safe to *display*, which is a different property from safe to *run*.

### 1.2 Spawn: same uid, the daemon's whole environment

- **Process.** Every agent reaches `spawn_with_dir` (`src/agent_pty.rs:1638`) and portable-pty's `spawn_command` as the daemon's uid. `setsid` there exists for teardown, not containment. A search of `src/` for `unshare`, `setrlimit`, `seccomp`, `landlock`, `sandbox-exec`, `bwrap`, `firejail`, `chroot`, `cgroup`, `docker` and `podman` finds nothing applied to agents. **[moves with #1666]**
- **Environment.** The child inherits the daemon's full environment snapshot. The daemon, when lazily started, inherits the environment of the client that started it, usually the first TUI (`src/platform/detach/unix.rs:61-69`); a service-managed daemon gets the service manager's environment instead, and its unit deliberately carries no credentials (`src/daemon_service.rs:46-60`). Before the overlay the deck removes only its own `DOT_AGENT_DECK_*` endpoints and identity (`src/agent_pty.rs:1766-1838`). `src/git_env.rs` scrubs eight git *location* variables, but only for git commands the deck runs itself, not for agents.
- **What the deck adds** to every agent: its pane and agent ids, a per-spawn 256-bit hook capability token, **both** the hook socket and the attach socket paths, the inbox spool and the deck binary path (`src/agent_pty.rs:9884-9931`, `10110-10143`), plus telemetry variables when telemetry is on, including the daemon's `OTEL_EXPORTER_OTLP_HEADERS`. **[moves with #1666]**
- **Agent permission flags.** The deck adds none: no `--dangerously-skip-permissions`, `--permission-mode`, `--allowedTools`, Codex `--sandbox` or `--ask-for-approval`. Registry defaults are bare binary names (`src/agent_registry.rs:798`, `874`, `914`, `957`, `1181`); otherwise the command is the user's, the role's or the client's. So an agent's own permission posture is whatever its command line and its own settings say.
- **Working directory.** It is checked for integrity (absolute, a directory, inode-verified for prepared starts; `src/agent_pty.rs:1291-1336`, `1670-1675`) and confines nothing once the agent runs.

### 1.3 Credentials

No credential variable is treated specially anywhere in production `src/`: a search for `ANTHROPIC_*`, `OPENAI_API`, `GITHUB_TOKEN`/`GH_TOKEN`, `AWS_*`, `SSH_AUTH_SOCK`, `GOOGLE_*` and `API_KEY` finds only test fixtures and one error string. Whatever the daemon has, every agent inherits; what then reaches the agent's tools and MCP servers is the agent's own policy (Codex, for example, has its own shell-environment filter). Measured from the dispatched pane this page was written in (names only, values never read): 177 environment variables, about twenty of them credential-named, among them `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`, `GOOGLE_APPLICATION_CREDENTIALS`, `SLACK_BOT_TOKEN` and `TELEGRAM_BOT_TOKEN`; and read access to `~/.ssh`, `~/.config/gh/hosts.yml`, `~/.claude/.credentials.json`, `~/.codex/auth.json` and OpenCode's `auth.json`. On this host, then, every agent can read every *other* agent's login, not only its own, because they are files in the one user's HOME.

This repository's own role commands go through `devbox run`, which is a Nix package environment and adds authority rather than removing it: its `init_hook` sources `.env` and, when `USE_VALS` is set, exports `vals` secrets and `GOOGLE_APPLICATION_CREDENTIALS` (`devbox.json`).

### 1.4 The control plane: two sockets, one principal

- **Transport.** Two Unix sockets, no TCP listener: the hook socket (`src/daemon.rs:713`) and the attach socket (`src/daemon.rs:955`), mode `0600` in a `0700` directory (`src/platform/fsperm/unix.rs:43-63`, `187-225`). The peer-uid check is client-side, protecting a client from a foreign listener (`src/platform/ipc/unix.rs:407-423`); on the server side any process of the user is accepted on both sockets.
- **Hook socket.** Status events and verbs are attributed by the pane id in the sender's environment plus the per-spawn token, classified token → record → pane so a valid token cannot name a sibling's pane (`src/hook_provenance.rs:443-469`, `502-539`). That closes "read a pane id off `daemon status` and forge it" and stale generations. It does not stop a sibling that reads the token from `/proc/<pid>/environ`, which the module documents. Panes the daemon never issued a token for (`Unattested`) are admitted on the sender's word, and `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` admits missing tokens. `delegate`, `restart_role` and `spawn_role` require an orchestrator caller (`src/state.rs:10897-10926`); `dispatch`, which cuts a worktree and spawns agents, has no role check (`src/daemon.rs:4452-4474`). `work_done` carries no task or delegation id (`src/event.rs:4016-4039`) and is routed by the worker pane's role; *fixed since*: #1898 added `work-done --task-id`. **[moves with #1666]** for every verb in this bullet.
- **Attach socket.** Unauthenticated beyond the socket mode, by design (`src/daemon_protocol.rs:2778-2784`). Any process of the user, which includes every agent since each is handed the path, can: `StartAgent` with any command, directory and environment; `StopAgent` any agent; write raw bytes into any live pane's PTY (`KIND_STREAM_IN`, liveness checked, caller not); snapshot any pane's scrollback; subscribe to every event; answer another agent's pending permission question (`AnswerQuestion`); post to any inbox; run a schedule now; and stop the daemon. `CloseAgents` is the one attach request that takes a caller claim, and an absent claim is treated as a person with full reach (`src/close_cli.rs:64`, `src/close_agents.rs:6`), so a same-uid process that drops its identity variables asks as a person (`src/close_agents.rs:28`). **[moves with #1666]**
- **Measured.** With the deck's pane identity variables unset, `dot-agent-deck daemon status` from the dispatched pane listed every pane in the deck: eleven agents across five units, with role, status and working directory.

### 1.5 Worktrees and git metadata

A dispatched unit gets a sibling worktree on its own branch (`src/dispatch.rs:400-416`), and no other isolation. Git's linked-worktree design shares the **common `.git` directory** with the main checkout: all refs and branches, objects, the stash, `config`, `hooks/` and `info/exclude`. The deck adds no `core.hooksPath`, `safe.directory`, push restriction or credential scoping. Measured from the dispatched pane: the common `config` and `hooks/` directory under the main checkout's `.git` are writable. So an agent in a worktree can:

- push with whatever credentials it inherited, and force-update or delete any local branch, `main` included;
- set `core.hooksPath`, `core.fsmonitor` or a remote URL in the shared `config`, or drop a script into the shared `hooks/`. Those then run in the main checkout, in later worktrees, and in the deck's own git commands: the daemon's `git worktree add` (which fires `post-checkout`) and its `git status` on a worktree (`src/issue_dispatch_run.rs:209-212`), which runs without the `core.fsmonitor` hardening the telemetry probe applies (`src/telemetry/run_context.rs:63-100`). *Fixed since*: #1899 (issue #1888) runs the deck's own git with hooks and `core.fsmonitor` off, so this item now applies to the main checkout and later worktrees only;
- write the deck's ownership marker into a sibling worktree's metadata directory (`docs/develop/worktree-ownership.md`), so `worktree reclaim` treats that worktree as deck-created.

The dispatch prompt itself tells the agent to write durable artifacts under the main checkout and gives the path (`src/dispatch.rs:697-706`), which is reasonable for a trusted agent and the opposite of containment. **[moves with #1666]** for the prompt text only.

### 1.6 The scheduler

A plain scheduled task spawns its command directly in its `working_dir`, with no worktree (`src/daemon.rs:1301-1316`). An issue-dispatch task clones the repository and spawns from that clone's committed config, as above, on a cron and unattended, with the signed-in `gh` account able to label and comment. `Unattended` on a dispatched unit changes the prompt, not permissions (`src/dispatch.rs:891`).

### 1.7 Remotes

Production code opens no TCP listener. `connect` is `ssh -t <host> …` (`src/connect.rs:806-845`), the desktop forwards the remote Unix socket over `ssh -N -L` with `StrictHostKeyChecking=yes` (`src/remote_tunnel.rs:1538-1551`, `2156-2196`), and management commands run the remote CLI over ssh. So a remote deck's whole gate is ssh authentication, and on the far side the model above repeats under that account. `ForwardAgent=no` is forced for `remote doctor` and the tunnel but not for `connect` (`src/remote.rs:811-821`). The user docs already state the consequence: "Agents on one host share that account's files and credentials; the deck adds no isolation between them" (`docs/remote-requirements.md`, "Credentials and one host per project") and "One user per host" (`docs/remote-environments.md`). A Kubernetes target parses and is refused (`src/connect.rs:42-45`); there is no container or VM placement.

### 1.8 Cleanup

Closing an agent sends `SIGTERM` then `SIGKILL` to its process group (`src/agent_pty.rs:1000-1019`). Claude Code's Bash tool `setsid`s its shells, so those leave the group and survive (`src/wrap.rs:1430-1438`, `src/platform/proc/scan.rs:17-19`); so do daemonised jobs, dev servers and their ports, and containers, which dockerd owns. The lifetime tag that would find escapees is set only under the test harness (`src/agent_pty.rs:1851-1857`), and the optional orphan reaper kills only orphaned MCP servers by default (`docs/develop/orphan-reaper.md`). Worktrees are removed on close (keep-if-dirty for dispatch, forced for issue dispatch); a unit's branch is not deleted on close (`docs/dispatcher-mode.md`); the list of worktrees to clean up is held in daemon memory only, so it is lost when the daemon restarts. **[moves with #1666]** for close and teardown paths.

## 2. Launch-configuration audit

The success criteria ask for at least five launch configurations, each tested for the seven resources below. "Measured" rows were probed from this host with a script that prints only booleans and variable *names* (`OK` = reachable, `no` = refused). "By code" rows were not probed: their spawn path is the same function as a measured row, and the cell says what the code makes them inherit. No remote is registered on this host, so the SSH row is from code and docs.

| Configuration | How established | Parent git metadata (common `config`/`hooks`) | Unrelated files (`~/.ssh`, other agents' logins) | Inherited credential env | Outbound network | Subprocesses | MCP tools | Sibling agents' state |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Worktree dispatch (this pane) | measured | writable | readable | all (5/5 probed names present) | OK | unrestricted; `setsid` children outlive close | the agent's own configured servers, launched with the same authority | full: attach socket lists, snapshots, types into and stops any pane |
| Local pane / orchestration role | by code: same spawn path and environment | writable when the repo has linked worktrees; it *is* the main checkout otherwise | readable | all | OK | same | same | same |
| Issue-dispatch schedule | by code | writable | readable | all, plus the `gh` account | OK | same | same | same; and the roles were chosen by the remote default branch |
| SSH remote | by code and docs | as above, under the remote account | as above, under the remote account | whatever the remote daemon's environment has | the remote host's | same | same | same, for every agent on that account |
| Codex sandbox, `workspace-write` | measured (`codex sandbox -c sandbox_mode="workspace-write"`) | refused (and so is the worktree's own git dir: commits fail) | readable | all | refused | confined with it | not measured | refused (the daemon socket is unreachable, so `work-done` and `delegate` fail too) |
| Claude Code sandbox (`sandbox.enabled`) | measured | n/a: see below | n/a | n/a | n/a | n/a | n/a | n/a |
| Deck-side `bwrap` posture (prototype) | measured | refused for shared `hooks/` and `config`; worktree git dir writable | refused, except the agent's own login | none (allowlist) | switchable, both measured | confined with it | the agent's own servers, inside the posture | refused (no socket mounted), or hook verbs only (hook socket mounted) |
| Container (`docker`, worktree mounted) | measured, git only | worktree-only mount: git does not work at all; same-path common-dir mount: writable unless mounted read-only | what is mounted | what is passed | Docker's default bridge | confined with it | not measured | what is mounted |

The Claude Code row has no values because the sandbox never ran a command: on this host it first reported `sandbox is enabled but dependencies are missing: socat not installed` and "Commands will run WITHOUT sandboxing"; with `socat` supplied it failed closed on every command (`apply-seccomp: … nested userns is capability-restricted`), because the host restricts unprivileged user namespaces. Both are findings in their own right; see [the prototypes](#4-prototypes-what-each-mechanism-actually-did).

## 3. Threat scenarios

Each scenario names what happens today, which posture from [section 5](#5-postures-names-capabilities-escalation) would contain it, and what remains.

**T1. An untrusted repository.** A user clones a repository whose `.dot-agent-deck.toml` defines roles, or schedules issue dispatch against a repository others can push to.
- *Today:* choosing the orchestration runs every role's `command` through the shell as the user, with no preview (the desktop never sees the command). For issue dispatch the commands come from the remote default branch on every fire, unattended. `prompt_template` and role descriptions reach agents verbatim. This is code execution, not a sandbox question: no agent is involved yet.
- *Contained by:* a **trust prompt** (#1885) that shows the commands and records approval of their exact text, invalidated when they change, before anything runs, and that is answered once per repository for schedules rather than per fire. No enforcement posture contains T1, because the commands are the user's own shell commands by design.
- *Residual:* an approved config can still run anything it says; the repository's other build inputs (`devbox.json`, `build.rs`, git hooks shipped by tooling) are the user's to judge, as with any build script.

**T2. A compromised or mistaken agent.** An agent follows injected instructions, or simply makes a mistake, with full tool permissions (the common configuration for unattended dispatch).
- *Today:* it can read and exfiltrate every inherited credential and every other agent's login, push with what it inherited, rewrite any local branch, plant a git hook or `core.fsmonitor` in the shared `.git` that later runs in the main checkout (and, until #1899, inside the daemon's own git commands), and, through the attach socket every agent is handed, type into sibling panes, answer their permission prompts, start an arbitrary unconfined process with `StartAgent`, or stop the deck.
- *Contained by:* **`scoped`** for inherited credential *variables* only: it leaves saved logins on disk (every agent's auth file, `~/.ssh`, the `gh` login) as readable as today; **`contained`** for those files, git metadata and network (Linux), or a separate account or host. Both depend on **control-plane scoping** (#1886): as long as a contained agent can reach the attach socket, `StartAgent` launches an unconfined process and the containment is a label. The same applies to the hook socket's `dispatch` verb, which spawns a new agent; a contained caller's children must inherit its posture.
- *Residual:* under `scoped` alone, every saved login and key the user's account can read; under `contained`, everything inside the worktree, the branch and the agent's own login; whatever network the agent is allowed; and any credential the work genuinely needs (pushing a PR needs a GitHub credential).

**T3. A stale or forged authority event.** A process sends an event or verb that the deck acts on as if it came from a particular agent or person.
- *Today:* hook provenance refuses stale generations and wrong-pane tokens. It does not stop a sibling that reads the token from `/proc/<pid>/environ`, `Unattested` panes are admitted on their word, `work_done` is bound to the pane rather than the task (*fixed since* for a `work-done --task-id`, by #1898), and an attach client without a claim is treated as a person. Remotely, ssh authentication is the whole gate.
- *Contained by:* within one user, only by **process separation** (a separate uid or a PID-namespaced posture, where `/proc` of siblings is not visible) plus control-plane scoping. Across people, by per-principal identity on the attach socket, which is #632's subject, not this PRD's.
- *Residual:* under `trusted`, T3 is not containable and the docs should say so.

**T4. Unattended exfiltration.** A dispatched or scheduled unit nobody is watching sends repository contents or secrets to the network.
- *Today:* the deck applies no network restriction. Only an agent's own sandbox restricts it, where that sandbox is enabled and actually enforced.
- *Contained by:* `contained` with network off (measured: refused), or an agent's own network allowlist where it has one. Note the systemd trap in section 4: a user-manager `IPAddressDeny=any` was **accepted and not enforced**.
- *Residual:* the agent's own model API is network by definition, so "network off" applies to tools and subprocesses, and an agent with network to its API can still put data into its prompts.

## 4. Prototypes: what each mechanism actually did

All prototypes were scratch scripts run outside the repository. The probe script and wrapper are summarised here rather than committed, because committing them would put a spawn wrapper into the tree that part 3 is rewriting.

| Mechanism | What was run | Result | Verdict |
| --- | --- | --- | --- |
| Minimal environment filtering | `env -i` with `HOME`, `USER`, `PATH`, `TERM`, `LANG`, then a real Claude Code (Haiku) task inside the posture below | Claude authenticated from its credentials file with no API key in the environment, created a sentinel file and committed it. Inherited credential names dropped from 5/5 to 0/5. | **Portable and cheap** (every OS has environment control). Breaks anything that authenticates only through an environment variable, and `gh` once `~/.config/gh` is also hidden; so it needs a per-role allowlist, not a fixed list. |
| Codex native sandbox | `codex sandbox` in `read-only` and `workspace-write` | `workspace-write`: worktree writable; worktree git dir, shared hooks, HOME and network refused; `~/.ssh` and other agents' logins still **readable**; credential variables still present; the deck's sockets unreachable. | **Real enforcement, for its own tool calls.** In a linked worktree it cannot commit unless the common dir is added as a writable root, which reopens the shared hooks. It cuts the agent off from the deck. Reads are not confined. |
| Claude Code native sandbox | `--settings '{"sandbox":{"enabled":true,…,"allowUnsandboxedCommands":false}}'`, Haiku, one Bash call | Without `socat`: a warning and **every command unsandboxed**. With `socat`: every command refused (nested user namespaces restricted by AppArmor). | **High-severity label risk.** "Sandbox enabled" in settings is not evidence it is enforced; on this host it was either silently absent or unusable. |
| OpenCode, Pi, Devin | `--help` only, not exercised | OpenCode's `--help` lists a permission auto-approve switch and no OS sandbox flag; Pi has tool allow/deny lists and a per-run "trust project-local files" switch; Devin has a research-preview `--sandbox` (macOS Seatbelt / Linux bwrap+seccomp) and permission modes. | Not established; recorded as each agent's declaration for the conformance matrix, not as measured enforcement. |
| Deck-side `bwrap` launcher | `bwrap` with the root read-only, HOME and `/run/user` on tmpfs, the worktree and the git common dir writable, the common `hooks/` and `config` re-mounted read-only, the agent's own login and `~/.gitconfig` mounted back, optional `--unshare-net`, allowlisted environment | Probe: worktree and its git dir writable; shared hooks refused; `~/.ssh` and other agents' logins refused; network on/off as asked; no credential variables. A real Haiku agent did its task and committed, and was refused when it tried to create `hooks/post-commit`. Hiding HOME first broke `git commit` (no identity) until `~/.gitconfig` was mounted back. | **Works on Linux, where it is enforceable and testable** by the same probe. Not portable as built: on macOS the command-line equivalent is the deprecated `sandbox-exec` (which Codex and Claude Code use for their own sandboxes), and this discovery found no comparable unprivileged mechanism on Windows. Refs in the common dir stay writable, so an agent can still move other branches; per-branch protection is not expressible as a mount. |
| Deck control plane inside the posture | the same, with nothing mounted, then with only the hook socket mounted and the pane's identity variables passed | Nothing mounted: hooks, `work-done` and inbox all fail. Hook socket only: `inbox list` worked; `daemon status` (attach socket) was refused. | **The split is feasible at the transport level.** It is not sufficient: the hook socket's `dispatch` still spawns unconfined agents, so posture inheritance must be enforced by the daemon. |
| Container | `docker run` with the worktree mounted | Worktree alone: `fatal: not a git repository` (its `.git` file names an absolute host path). Worktree plus common dir at the same paths: git works, and the shared hooks are writable unless mounted read-only. | **Composes with worktrees only by reproducing the host paths**, so it inherits exactly the `bwrap` mount questions plus an image that must contain the agent and its login. Membership of the `docker` group is root-equivalent on the host, per Docker's own documentation, so a container placement must not be described as reducing the *user's* authority. |
| systemd user scope | `systemd-run --user -p IPAddressDeny=any curl …` | **HTTP 200**: the property was accepted and nothing was blocked. | **Rejected.** The user manager accepts the property without enforcing it on this host, which is the label-versus-enforcement mismatch in its purest form. |
| Per-project host or account | not built; the remote feature already provides it | The deck runs a remote daemon under whichever account the ssh target names. | **Already available as placement**, and, unlike the `bwrap` posture as prototyped, it also separates `/proc`, the deck's sockets and every credential without per-path mount decisions. It needs guidance and disclosure, not new enforcement. |
| Policy declarations | analysis | A posture declared in `.dot-agent-deck.toml` comes from the very repository T1 distrusts. | A **project may only narrow**: request a stricter posture, never a broader one. Widening comes from the user's own config or an explicit human escalation. |

## 5. Postures: names, capabilities, escalation

These are the names the evidence supports. Each one promises only what its conformance probe can test on the host it runs on, and each card and dispatch report says which posture an agent runs under.

| Posture | Promise (what the probe checks) | Explicitly not promised | Portability |
| --- | --- | --- | --- |
| `trusted` | Nothing beyond running as you. This is today's behaviour, named. | Anything. Other agents, other logins, the shared `.git`, the network and the deck's whole control plane are reachable. | All |
| `scoped` | Only allowlisted environment variables reach the agent; it is not handed the attach socket; its hook verbs are limited to its own pane and its children inherit `scoped` or stricter. | Files, git metadata, network, other agents' `/proc`. | All (environment and socket handling are deck-side) |
| `contained` | `scoped`, plus: writes only to its worktree, that worktree's git metadata and declared state paths; shared git `hooks/` and `config` read-only; HOME hidden except the agent's own login and declared paths; network on or off as declared; sibling processes not visible. | Other branches' refs in the shared object store; anything the agent's own API connection carries; the agent's own login. | Linux only, and only where the probe passes (not, for example, where user namespaces are unavailable) |
| placement: separate account or host | Everything the OS separates between accounts: files, credentials, `/proc`, the deck's sockets. | That the account itself is safe; that the host is not shared with something else. | Any host the remote feature reaches |

Agent-native sandboxes are **not** deck postures. The deck reports them as the agent's declaration in the conformance matrix (#221's dimension), and never labels a pane safer because an agent claims a sandbox, since on this host one such claim was silently unenforced and another made every command fail.

**Escalation and override.**
- The default stays `trusted`. Nothing changes for an existing user without their action, so migration does not broaden or silently narrow anything.
- The user chooses a posture per role or per dispatch in their own config. A project's config may request a stricter posture for a role, never a broader one.
- Children inherit: a `delegate`, `dispatch` or `spawn_role` from a pane runs its targets at the caller's posture or stricter. This is the property that makes any posture other than `trusted` meaningful, and it is enforced by the daemon, not the prompt.
- Escalating a running unit (for example to give a `contained` unit network or a credential) is a human action recorded on the unit, never an agent's verb.
- If a posture's probe fails at launch, the launch fails, and does not fall back to a weaker posture with a warning. Claude Code's fallback is the counter-example.

**Evidence a remote or shared mutating action needs first.** Before a remote companion (#631) or a team control plane (#632) exposes an action that changes state (start, stop, answer a permission prompt, type into a pane, approve a dispatch):
1. a principal on the attach socket other than "the OS user", so the deck can tell the companion from an agent;
2. the action bound to that principal and to a specific target generation, as close already is, and a `work_done` bound to the delegation it answers;
3. agents unable to reach the verbs that principal uses (control-plane scoping); otherwise any agent can impersonate the companion;
4. the posture of every agent the action can reach, shown to the person authorising it.

## 6. Decision: Revise

The PRD's `Go` bar is not met today, and the evidence says why:

- *Every named posture has testable capabilities* — met by `trusted`, `scoped` and `contained` as defined above, but only on Linux for `contained`.
- *No high-severity label-versus-enforcement mismatch unresolved* — **not met.** Any per-agent confinement is defeated while the agent is handed the attach socket (`StartAgent` is deliberately unconfined) or can `dispatch` an unconfined child. Until the daemon scopes its control plane per agent, `contained` would be a label.
- *Controls reduce authority without breaking the workflows they claim to protect* — partly. Environment filtering and the `bwrap` posture kept a real agent working, provided the agent's own login and git identity were mounted back. The Codex and Claude sandboxes broke the deck's own workflow (git in worktrees, deck messaging, or every command).
- *Migration does not silently broaden access* — met by keeping `trusted` as the default.

So the decision is **Revise**: standardise disclosure (the `trusted` model stated plainly, agents' native sandbox declarations recorded as declarations) and host guidance (separate account or host per credential boundary), and build only the deck-side controls that proved portable or testable, in order: project configuration trust before execution, then control-plane scoping, then the opt-in `scoped` and Linux `contained` postures on top of it. **Stop** applies to two specific promises: no feature may describe a worktree, a container started by the user's `docker` group, or an agent-native sandbox as a security boundary; and remote or team features must not expose mutating actions until the evidence in section 5 exists.

This becomes `Go` when control-plane scoping has landed and a `contained` launch passes the probe with a real agent doing dispatch work end to end.

## Follow-ups

Filed with the decision on 2026-10-25. Their order matters: #1886 is a prerequisite for #1887's containment, and all three PRDs touch areas PRD #1666 part 3 owns, so their implementation starts after part 3 merges.

1. **#1885 — Project configuration trust before execution** (T1).
2. **#1886 — Scope the deck's control plane per agent**: agents not handed the attach socket, hook verbs limited to the caller's pane, children inheriting the caller's posture (T2, T3; prerequisite for 3).
3. **#1887 — Execution postures: disclosure first, then opt-in `scoped` and `contained`** (T2, T4).
4. **#1888**, a bug rather than a PRD: the deck's own git commands in agent-writable repositories ran without disabling hooks and `core.fsmonitor`. Fixed by #1899 the same day.
