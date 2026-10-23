# Schedules

A schedule runs a prompt on a cron timetable: when it comes due, the deck opens a tab in the schedule's working directory, starts an agent there (or an orchestration, if that directory defines one) and sends it the prompt. An issue-dispatch schedule instead starts one agent per open GitHub issue of a repository. This page shows how to create, check, change and troubleshoot schedules, and ends with a complete [reference](#reference) for the schedules file, the cron syntax and the `dot-agent-deck schedule` command.

Schedules run inside the deck's daemon, not inside the TUI or the desktop app. Each daemon runs the schedules in the schedules file on its own machine (`~/.config/dot-agent-deck/schedules.toml` by default; see [The schedules file](#the-schedules-file)), and only while it is running; see [Keep schedules running](#keep-schedules-running).

Which client can do what:

| What | TUI | Desktop app | CLI |
|---|---|---|---|
| Create a schedule with a guided authoring agent | yes: the Schedules manager, or the **schedule** mode in the New Agent form | yes: the **schedule** chip in **New agent** | — |
| Create or change a schedule directly | — | — | `dot-agent-deck schedule add` / `update` |
| List, pause, resume, run now, delete | yes: the Schedules manager | no schedule manager | `dot-agent-deck schedule …` |
| See the agents a run starts | yes, as cards and tabs | yes, on the Dashboard | `dot-agent-deck daemon status` |

The Schedules manager exists only in the TUI. In the desktop app, a schedule's runs appear on the Dashboard like agents you started yourself, but the schedules themselves are not listed there.

## Before you start

- **A running daemon.** Starting the TUI (`dot-agent-deck`) starts one if none is running. The `schedule` subcommands do not start one. Check with `dot-agent-deck daemon status`, which reports a missing daemon rather than starting it.
- **An agent command.** A plain schedule needs the command that launches your agent, for example `claude`, `opencode`, `pi`, `codex` or `devin`, or a wrapper that ends up running one of them (`devbox run agent`). Any other command runs, but the deck cannot show its status.
- **For issue dispatch only:** the GitHub CLI `gh`, installed and signed in (`gh auth status` succeeds), and `git`, both on the daemon's `PATH`. Read access to the repository is enough to dispatch. To [claim](#claims-on-github) the issues it dispatches, the account `gh` is signed in with also needs to add labels and comments to the repository's issues: write access to the repository covers the whole claim, including creating its label, and an account that can apply labels and comment but not create labels can claim once the `agent-deck/claimed` label exists.

## Schedule a prompt with the CLI

This creates a schedule that runs every weekday at 09:00 in `~/scheduled/morning-digest`:

```bash
dot-agent-deck schedule add \
  --name morning-digest \
  --cron "0 9 * * MON-FRI" \
  --working-dir ~/scheduled/morning-digest \
  --command claude \
  --prompt "Summarize the GitHub issues opened in the last 24 hours in vfarcic/dot-agent-deck."
```

On success the command prints nothing and exits 0. It validates the cron expression, expands `~` and `$VAR` in the working directory, writes the schedules file, and asks the running daemon to reload it. If no daemon is running, it still writes the file and prints ``note: wrote <path> but could not reload the daemon (…); it will load on next `daemon serve` ``; the schedule loads the next time a daemon starts.

Check each step:

1. **The file has it.** `dot-agent-deck schedule list` prints one line per schedule, for example `enabled   morning-digest  cron="0 9 * * MON-FRI"  next=2026-09-30 09:00:00 CEST  shape=config-derived  dir=/home/you/scheduled/morning-digest`. `next=` is the next time the cron matches, whether or not the schedule is enabled.
2. **The daemon has it.** `dot-agent-deck schedule reload` prints `reloaded; registered: <names>`, the enabled schedules the daemon is now running. If your schedule is missing from that list, it is disabled or the daemon rejected it; see [When a schedule does not run](#when-a-schedule-does-not-run).
3. **It works.** `dot-agent-deck schedule run-now --name morning-digest` runs it immediately and prints `ran morning-digest`. A new card (or tab) opens in the TUI and a new row on the desktop Dashboard, and the prompt is sent once the agent is ready.

The working directory is created (including missing parents) when the schedule runs, if it does not exist yet.

## Schedule a prompt with an authoring agent

Instead of writing the command yourself, you can describe the job to an agent that writes it for you. The authoring agent asks for each field, offers to try the prompt in its own session first, confirms the whole schedule with you, and then runs `dot-agent-deck schedule add` (or `schedule update` when editing). When it is done it tells you its pane can be closed.

The authoring agent runs the command in the form's **Command** field, which is pre-filled from [`default_command`](configuration.md#set-the-command-new-agents-start-with); if that is empty, `claude` is used.

It saves the schedule by running `schedule add` (or `schedule update` when editing) by the deck's full path, such as `/home/you/.local/bin/dot-agent-deck schedule add …`, so the schedule reaches this deck whatever the agent's own `PATH` holds. If you let the agent run commands through a permission rule, write the rule against the path shown in its pane: a Claude Code allow rule such as `Bash(dot-agent-deck schedule:*)` does not match it.

**TUI, from the Schedules manager.** Press `s` on the dashboard (`S` also works, and the key can be remapped as `open_scheduled_tasks`; see [Keyboard Shortcuts](keyboard-shortcuts.md)), or click **[Schedules s]**. Press `a` (**[Add a]**), pick a directory (it becomes the schedule's default working directory), confirm the **New Schedule** form's **Dir** and **Command** fields, and the authoring agent starts in that directory. `Esc` or **[Cancel]** returns to the manager.

**TUI, from the New Agent form.** Press `Ctrl+n`, choose a directory, and cycle the **Mode** field past your project's orchestrations to **schedule** (shown with the hint `authoring (one-off)`).

**Desktop app.** Open **New agent**, choose the daemon and a directory, and pick the **schedule** chip under **Mode**. The schedule is written on the machine of the daemon you chose. See [New Agent](desktop/new-agent.md).

Check the result the same way as for the CLI: `dot-agent-deck schedule list` on that machine, or the TUI's Schedules manager.

## Manage schedules

In the CLI, every command selects the schedule by `--name`:

| Task | Command |
|---|---|
| List schedules | `dot-agent-deck schedule list` |
| Change fields | `dot-agent-deck schedule update --name <name> [--cron …] [--working-dir …] [--command …] [--prompt …] [--new-tab-per-fire true\|false] [--enabled true\|false] [--shape …]` |
| Pause | `dot-agent-deck schedule disable --name <name>` |
| Resume | `dot-agent-deck schedule enable --name <name>` |
| Run now | `dot-agent-deck schedule run-now --name <name>` |
| Delete | `dot-agent-deck schedule remove --name <name>` |
| Re-read a hand-edited file | `dot-agent-deck schedule reload` |

Things to know when changing schedules:

- **A schedule cannot be renamed.** `update` has no rename flag. Remove it and add it again under the new name.
- **Pause rather than delete** when you only want it to stop for a while: `disable` keeps every field.
- **Run now needs an enabled schedule.** The daemon only holds enabled schedules, so `run-now` on a disabled one fails with `run-now failed: no schedule named "<name>"`.
- **Deleting does not close tabs.** A tab a schedule already opened stays open.
- **Prompt, cron and `new_tab_per_fire` changes apply from the next run.** A change of `working_dir` or `command` also applies to the next run that opens a new tab. While the schedule reuses a tab whose agent is still running (the default), the next prompt goes into that existing tab, in its old directory with its old command. Close that tab to make the change take effect.
- **`update` cannot change issue-dispatch settings** (`repo`, `max_per_run`, `label`, `query`). Remove the schedule and add it again, or edit the file and run `schedule reload`.

### The TUI's Schedules manager

![The TUI's Schedules manager with one schedule: its row shows the name, the status disabled and a next fire of —, above the Add, Edit, Delete, Run now and Toggle buttons](/img/schedules-tui.png)

Each row shows the schedule's name, a status and its next run time. The statuses are taken when the dialog opens and refreshed after a run-now:

| Status | Meaning |
|---|---|
| `live` | Enabled, and a tab or agent this schedule started is still running. |
| `idle` | Enabled, with nothing it started running now. |
| `disabled` | Paused (`enabled = false`). Its next-fire cell shows `—`. |

| Key / button | Action |
|---|---|
| `a` / **[Add a]** | Add a schedule through the authoring agent (see above). |
| `Enter` / `e` / **[Edit e]** | Edit the selected schedule: the directory picker opens at its working directory, and the authoring agent starts with its current values and saves with `schedule update`. |
| `d`, then `y` / **[Delete d]** | Delete the selected schedule after confirmation (`n` or `Esc` cancels). |
| `r` / **[Run now r]** | Run the selected schedule now. The status line says `Ran schedule '<name>'`, `'<name>' already running — skipped`, or `Run-now failed: …`. |
| `t` / **[Toggle t]** | Pause or resume the selected schedule, without confirmation. |
| `j` / `k` (or `↓` / `↑`) | Move the selection. Rows can also be clicked. |
| `Esc` / `q` / `s` / `S` | Close the manager. |

The manager reads and writes the schedules file on the machine the TUI runs on.

## Choose what a run opens

Without a `shape`, a run looks at the `.dot-agent-deck.toml` in the schedule's `working_dir` itself (parent directories are not searched):

- If it defines an `[[orchestrations]]` block with at least one role, the run opens that directory's default orchestration (the one with `default = true`, otherwise the first one with roles; see [Which orchestration a schedule opens](orchestration.md#which-orchestration-a-schedule-opens)) and sends the prompt to its orchestrator. The schedule's `command` is not used.
- Otherwise the run opens one agent running `command` and sends the prompt to it.

Set `shape` to decide it yourself:

| `shape` | What a run opens |
|---|---|
| *(unset)* | Decided from the directory, as above. `schedule list` shows `shape=config-derived`. |
| `single` | One agent running `command`, even where the directory defines orchestrations. Use it when the job needs the repository (its skills, its git remote) but not the team. |
| `orchestration` | The directory's default orchestration. |
| `orchestration:<name>` | The orchestration with that name. |

```bash
dot-agent-deck schedule update --name morning-digest --shape single
dot-agent-deck schedule update --name morning-digest --shape ""   # back to config-derived
```

If a `shape` cannot be satisfied when the run comes due (the named orchestration no longer exists, more than one orchestration with roles uses that name, none has roles, or the directory's `.dot-agent-deck.toml` cannot be parsed), the run is skipped and nothing opens in its place. The reason, including the orchestrations that do exist, goes to the [daemon's output](#where-schedule-errors-are-reported).

## Reuse one tab or open a new one per run

- **`new_tab_per_fire = false` (default):** a run sends its prompt into the tab the previous run opened, if that tab's agent is still the one the schedule started. The agent receives the prompt in the same session, so it still has the previous run's conversation. If that tab was closed, its agent exited, or the daemon restarted since, the run opens a new tab.
- **`new_tab_per_fire = true`:** every run opens a new tab, so you keep one tab per run.

An orchestration a run starts is named after the orchestration and its working directory, for example `team · my-repo`. The TUI shows that name on the run's tab, and the desktop app as the title of the run's group on the Dashboard. If another run of the same orchestration is still running in that directory, started by this schedule or another one, the new run gets the next free number instead (`team · my-repo · 2`, then `· 3`), so you can tell the runs apart in both clients. The run is never skipped because of its name.

A run's prompt waits in the agent's inbox, including when the schedule reuses a tab. The TUI card and desktop tile show `✉N queued`, then `✉ delivered` after receipt, or `not delivered: …` with **Error** (desktop **error**) if delivery fails. Native delivery leaves your input box alone. When the doorbell is needed, it waits for an idle agent and a clear input box; it does not force submission after a timeout. See [Delivery in the TUI and desktop app](orchestration.md#delivery-in-the-tui-and-desktop-app).


## Dispatch agents onto open GitHub issues

An issue-dispatch schedule takes the open issues of one GitHub repository on each run and starts one agent per issue, each in its own git worktree on the branch `agent/issue-<n>`. For several repositories, create one schedule per repository.

### Create one

With the CLI (the `--repo` flag makes it an issue-dispatch schedule):

```bash
dot-agent-deck schedule add \
  --repo vfarcic/dot-ai \
  --name "Issues vfarcic/dot-ai" \
  --cron "0 9 * * MON-FRI" \
  --working-dir ~/dispatch \
  --max-per-run 3 \
  --label agent-eligible \
  --prompt "Work on issue {{issue_number}}"
```

- `--repo` must be `owner/name`; anything else is rejected before the file is written.
- `--max-per-run` defaults to `3`; `--label` and `--query` are optional.
- `--command` is optional. It is used only for issues whose clone has no orchestration (see below).
- `--shape` cannot be combined with `--repo`.
- `{{issue_number}}` in the prompt is replaced with each issue's number. The agent works inside that issue's worktree, so the number is usually enough context; `--prompt "/prd-full {{issue_number}}"` runs one of your own skills instead.

The same schedule as TOML is in [Worked examples](#an-issue-dispatch-schedule). A schedule with an `[scheduled_tasks.issue_dispatch]` table runs whether or not the `experimental` flag is on.

**With an authoring agent:** the **schedule: issues** mode in the TUI's New Agent form, or the **schedule: issues** chip in the desktop app's **New agent**, starts an agent that builds one with you. Both appear only when the `experimental` flag is on: for the TUI, set `experimental = true` under `[features]` in `.dot-agent-deck.toml` or launch it with `DOT_AGENT_DECK_EXPERIMENTAL=1` (the variable wins); for the desktop app, the flag of the daemon you create the agent on decides. See [Configuration](configuration.md).

Check it: `dot-agent-deck schedule list` shows the schedule, and `dot-agent-deck schedule run-now --name "Issues vfarcic/dot-ai"` runs it once. Once the run has cloned the repository, a tab opens per dispatched issue, and `git -C ~/dispatch/"Issues vfarcic-dot-ai" worktree list` lists one worktree per issue.

### What a run does

1. **Gets the repository.** The clone lives at `<working_dir>/<schedule name>`, with `/` and `\` in the name replaced by `-` (for the example above, `~/dispatch/Issues vfarcic-dot-ai`). The first run clones it with `gh repo clone`; later runs check that the clone's `origin` is that repository and then run `git fetch` and `git pull --ff-only`. A failed refresh is logged and the run continues with what is on disk; an `origin` that points at another repository stops the run.
2. **Lists issues.** It runs `gh issue list --repo <repo> --state open --limit <max_per_run>`, adding `--label <label>` and `--search <query>` when set, and takes at most `max_per_run` issues in the order `gh` returns them.
3. **Skips claimed issues.** An issue is skipped when its worktree `<clone>/.worktrees/issue-<n>` already exists, or when an open pull request has the head branch `agent/issue-<n>`. With `claim_gating = true`, it is also skipped while another schedule or machine holds a live claim on it (see [Claims on GitHub](#claims-on-github)). Each skip is logged with its reason. Skipped issues still count toward `max_per_run`: if every listed issue is claimed, the run starts nothing, even when later issues are free.
4. **Creates the worktree** at `<clone>/.worktrees/issue-<n>` on the branch `agent/issue-<n>`. If that branch already exists in the clone (from an earlier run whose tab you closed without opening a pull request), the worktree checks it out, so its commits carry over.
5. **Starts the agent** in the worktree and sends it the prompt. If the worktree's `.dot-agent-deck.toml` defines an orchestration with roles, the issue gets an orchestration tab, using the roles' own commands. Otherwise it gets one agent running the schedule's `command`, else your [`default_command`](configuration.md#set-the-command-new-agents-start-with); if neither is set, the agent pane starts the daemon's shell (`$SHELL`), which cannot act on the prompt, so set one of them.
6. **Claims the issue on GitHub** once the agent has started, as described in [Claims on GitHub](#claims-on-github). An issue whose agent did not start gets no claim.

A failure on one issue (a `gh` error, a worktree error) is reported and the run carries on with the other issues. A failure to get the repository or list issues stops that run.

### Claims on GitHub

The deck tries to mark each dispatched issue on GitHub, so anyone reading the repository's issue list can see an agent is working on it before a pull request exists:

- **The `agent-deck/claimed` label** is added to the issue. If the repository does not have that label yet, the run creates it first; if it already exists, its colour and description are reset to the deck's. When the account cannot create or edit labels, the run logs that in the [daemon log](#where-schedule-errors-are-reported) and still adds the label if the repository already has it.
- **One claim comment** on the issue, posted by the account `gh` is signed in with, names the schedule, a machine id for the machine that dispatched it, and when it last claimed the issue. When the same schedule on the same machine, signed in as the same account, dispatches that issue again later, it updates its comment, including that time, instead of posting another one. A copy of that comment posted by anyone else is left alone.

The claim comment is as visible as the issue: on a public repository, anyone can read it. It shows the schedule's `name`, so give an issue-dispatch schedule a name you are happy to publish. The machine id (`h1-` followed by 16 hex digits) is computed from the machine's host name, so the comment does not show the host name itself, but anyone who guesses the host name can compute the same id and confirm the guess. If the machine's host name changes, its next claim posts a new comment.

With write access to the repository, the account `gh` is signed in with can do all of this. An account that can apply labels and comment on issues but not create labels, such as one with GitHub's triage role, claims issues too once the `agent-deck/claimed` label exists in the repository: if it is missing, ask someone with write access to create it once, for example with `gh label create agent-deck/claimed --repo <owner/name>`. If the label or the comment cannot be written, the run reports it with one `claim failed for issue #<n>` line in the [daemon log](#where-schedule-errors-are-reported) saying what happened to the label and to the claim record (the comment), and why. The agent keeps working either way: a failed claim never stops a dispatch. The claims of one run share a two-minute time limit on GitHub calls, so claims can delay later dispatches by up to two minutes in total per run. Once that limit is used up, the run's remaining issues are dispatched without a claim attempt, and each gets a `claim failed` line saying its claim was skipped because the run's claim time budget was used up.

The deck never removes the label or the comment, not even when you close the issue's tab. Remove them yourself when you no longer want them, for example with `gh issue edit <n> --repo <owner/name> --remove-label agent-deck/claimed`.

#### Skip issues that others have claimed

By default the claim is for people reading the issue: another machine, or another schedule dispatching the same repository, still dispatches an issue that is already claimed. To make a schedule skip issues that others are working on, set `claim_gating = true` in its `[scheduled_tasks.issue_dispatch]` table:

```toml
[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"
claim_gating = true
claim_ttl_hours = 168   # the default: a week
```

With it on, a run skips an issue when the issue carries the `agent-deck/claimed` label and a claim comment from another schedule, another machine or another GitHub account was posted or updated within the last `claim_ttl_hours`. The log line names the schedule and machine id holding the claim, the GitHub account that wrote it, and how long ago it was last active, for example `skipping already-claimed issue #42 of vfarcic/dot-ai (agent/issue-42): claimed by schedule "Issues vfarcic/dot-ai" on machine h1-… (GitHub account teammate), last active 3h 2m ago`.

- **Only claims by people with standing on the repository block a dispatch.** A claim comment counts only when GitHub reports its author as the repository's owner, a member of the organization that owns it, or a collaborator. On a public repository anyone can comment, so a claim comment written by anyone else is ignored: it never stops a schedule from dispatching the issue. Sign every deck's `gh` in with an account that has that standing, or their claims will not block each other.

- **A schedule's own claim does not block it.** A claim comment this schedule wrote on this machine, signed in as the same account, is checked against the issue's worktree and pull request instead: if both are gone, for example after the machine's disk was wiped, the next run that considers the issue dispatches it again, even though its own claim comment is still there.
- **Runs refresh their own claims on issues still in progress.** When a run considers an issue whose worktree still exists, it updates that schedule's claim comment once at least half of `claim_ttl_hours` has passed since its last update, whether or not `claim_gating` is on. A claim can still expire while its agent works: an issue the run does not consider, because it is not among the first `max_per_run` issues listed, is not refreshed, and neither is one whose update fails, for example because the account's GitHub API rate limit is used up. A claim whose schedule stopped running (the machine is gone, or the schedule was removed) expires `claim_ttl_hours` after its last update, and then other schedules can dispatch the issue.
- **Remove the label to release a claim by hand.** Without the `agent-deck/claimed` label, no claim on the issue blocks anyone, whatever its comments say: `gh issue edit <n> --repo <owner/name> --remove-label agent-deck/claimed`. The next run that dispatches the issue adds the label again.
- **`claim_ttl_hours` must be at least twice the longest gap between the schedule's runs, plus one hour**, which leaves headroom between the owner's scheduled refreshes before a claim expires. The extra hour allows for the time between a run starting and its claim being written or updated. For `0 9 * * MON-FRI`, the longest gap is the 72 hours over the weekend, so the TTL must be at least 145; the default of 168 is enough. When it is shorter, the schedule still loads, runs and writes claims, but does not skip anything for them: the daemon log has a `[scheduler] warning: schedule "<name>": claim_gating is OFF …` line naming the schedule and the TTL it needs, on every start and `schedule reload`.
- **Claim ages are judged by GitHub's clock when GitHub reports it**, so a machine whose clock is fast or slow judges claims the same way as the others. When GitHub does not report its time, the run uses the machine's own clock instead and the daemon log has a warning saying so; a machine whose clock is wrong can then treat a live claim as expired, or the other way round. A claim comment dated more than a few minutes in the future does not block a dispatch.
- **When an issue's claims cannot be read, the issue is dispatched.** If the run cannot read an issue's claims from GitHub, the `dispatched issue #<n>` line says it dispatched without checking its claims, and why. Reading claims shares the run's two-minute GitHub time limit with writing them.

Claims cost GitHub API requests, whether or not `claim_gating` is on. A run that reads claims looks up the signed-in account and reuses a successful lookup for the rest of the run; after a failed lookup it may try again within the run's two-minute GitHub time limit. For each issue it considers whose worktree exists, it reads the issue's comments, plus the issue's events when its claim is due for an update. With `claim_gating = true`, each considered issue that has no worktree and no open pull request also costs a read of its events, and of its comments when it carries the label. Each read takes one request per 100 comments or events. These requests count against the GitHub API rate limit of the account `gh` is signed in with (GitHub's usual limit for a signed-in user is 5,000 requests an hour), shared with everything else that uses that account: a schedule that runs every minute with `max_per_run = 50` can spend over 3,000 requests an hour on claims alone. When the limit is used up, claims cannot be read, so issues are dispatched without checking them. Run issue-dispatch schedules every 15 minutes or less often, and lower `max_per_run` if a frequent schedule has many issues in flight.

Machines running an earlier version of dot-agent-deck write claims but do not skip issues for them, and do not update their claims while their agents work. A schedule with `claim_gating = true` honours their claims too, until `claim_ttl_hours` after their last update; after that it can dispatch an issue an older machine is still working on.

### Clean up

Dispatched tabs stay open until you close them. **Closing an issue's tab removes its worktree with `git worktree remove --force`, which discards uncommitted changes in it.** Commit or push anything you want to keep first. The branch and the clone stay, and the issue becomes eligible again on the next run unless it has an open pull request.

After the daemon restarts, closing such a tab no longer removes the worktree. Remove it yourself with `git -C <clone> worktree remove .worktrees/issue-<n>`, or that issue stays skipped.

An issue that is still open after its pull request merged or closed has no open pull request any more, so once its worktree is gone a later run dispatches it again on the existing branch. Close the issue (or remove the label your schedule's `label` filter selects) to stop that.

## Keep schedules running

- **Closing the TUI or the desktop app does not stop schedules**, and the daemon keeps running while at least one enabled schedule exists.
- **Nothing runs while the daemon is stopped.** A run whose time passed while the daemon was stopped, or while the machine was asleep for more than a minute, is not made up later.
- **A reboot or a crash stops schedules until something starts the daemon again.** For schedules that must run without you opening the deck, run the daemon as a service with `dot-agent-deck daemon install`; it then starts at boot or login and after a crash. See [Run the Daemon as a Service](daemon-service.md).
- **Stopping the daemon stops the agents it runs**, including those schedules started. `dot-agent-deck daemon stop` may refuse while agents are running unless you pass `--force`. After a restart, the next run of each schedule opens a new tab.
- **A run that comes due while the daemon is restarting** (for example during an upgrade) is not reported as failed. If the restart is called off, the run goes ahead; if it happens, the run is skipped and the schedule's next run happens on the new daemon. An issue-dispatch run leaves the issues it had not reached for that next run.
- **Your schedules are kept.** A daemon loads the schedules file when it starts. The `schedule` subcommands do not start a daemon; start the TUI (`dot-agent-deck`), or run `dot-agent-deck daemon serve` to run one in the foreground.
- **Time zone.** Cron times use the daemon's local time zone; there is no per-schedule time zone. On a daylight-saving change, a run inside the skipped hour does not happen and a run inside the repeated hour happens twice.
- **One run at a time per schedule.** If a schedule comes due while its previous run is still starting its agent or waiting to deliver the prompt, the new run is skipped, and `run-now` prints `skipped <name>: previous run still active`.

## When a schedule does not run

### Where schedule errors are reported

Schedule problems are not shown in the TUI or the desktop app. The daemon writes one line per problem, each starting with `[scheduler]`, to its output. For a daemon the deck started in the background, that output is appended to `daemon.log` in the state directory: `$XDG_STATE_HOME/dot-agent-deck/daemon.log`, or `~/.local/state/dot-agent-deck/daemon.log` when `XDG_STATE_HOME` is unset (on Windows, `%LOCALAPPDATA%\dot-agent-deck\daemon.log`). `DOT_AGENT_DECK_STATE_DIR` moves it. For a daemon started with `dot-agent-deck daemon serve`, the lines go to that terminal.

```bash
grep '\[scheduler\]' ~/.local/state/dot-agent-deck/daemon.log | tail -20
```

For more detail, see [Troubleshooting](troubleshooting.md#enabling-debug-logs).

### Symptoms

| Symptom | Cause | What to do |
|---|---|---|
| `schedule add` fails with `invalid cron expression: …` | The cron expression does not parse. | Fix it using [Cron syntax](#cron-syntax). |
| `schedule add` fails with `--command is required: …` | A plain schedule has no `--command`. | Pass `--command`. |
| `schedule add` fails with `a schedule named "<name>" already exists; …` | Names are unique. | Use `schedule update`, or another name. |
| `schedule reload` does not list the schedule | It is disabled, or the daemon rejected the entry (missing `command`, bad `shape`, bad `repo`, `shape` together with `issue_dispatch`, invalid cron). | Run `schedule enable`, or read the `[scheduler] config error …` line in the [daemon log](#where-schedule-errors-are-reported) and fix the entry. |
| `schedule` commands print `warning: skipped malformed entry…` | An entry in the file does not parse or is invalid. | Fix it by hand. **A command that changes the file (`add`, `update`, `remove`, `enable`, `disable`) rewrites it without the malformed entry**, so fix the entry first. |
| `schedule run-now` or `reload` fails with a connection error | No daemon is running (or the CLI and the daemon use different sockets). | Start the TUI or `dot-agent-deck daemon serve`; check with `dot-agent-deck daemon status`. |
| A schedule runs on the wrong days | Numeric days of the week count from `1` = Sunday, not `0` = Sunday. | Use day names (`MON-FRI`); see [Cron syntax](#cron-syntax). |
| A run opened the whole team instead of one agent | The working directory defines `[[orchestrations]]` and the schedule has no `shape`. | `schedule update --name <name> --shape single`. |
| A run opened one agent instead of the team | The directory's `.dot-agent-deck.toml` is missing, has no role-bearing orchestration, or does not parse (without a `shape`, a file that does not parse counts as no file). | Run `dot-agent-deck validate` in that directory, or set `--shape orchestration` to get an error instead of a single agent. |
| Nothing opened, and the log has `[scheduler] task "<name>": spawn failed: shape …` | The `shape` could not be resolved in that directory. | The line names the orchestrations that exist; fix the `shape` or the directory's config. |
| The log has `could not create working_dir` | The working directory cannot be created. | Fix the path or its permissions. |
| A changed `working_dir` or `command` is ignored | The run reused the tab the previous run opened. | Close that tab; see [Manage schedules](#manage-schedules). |
| The log has `claim failed for issue #<n>` | One step of the claim did not work. The agent was still dispatched. Possible causes: the account `gh` is signed in with cannot write to the repository's issues or labels, a GitHub call timed out (each claim call is given 30 seconds, and all the claims of one run two minutes together), or a network or GitHub server error. | Read what the line says after `label agent-deck/claimed:` and after `claim record:` before changing anything: an `HTTP 403` or `not accessible` points at permissions. The account (`gh auth status` shows which) needs to be able to add labels and comments to the repository's issues; if the line also says creating the label failed, have someone with write access create `agent-deck/claimed` once, or sign `gh` in with an account that has write access. `timed out`, `claim time budget` or a server error is usually temporary, but the deck does not come back to claim the issue later, so add the label yourself if you want it. Or leave it: the dispatch works without the claim. See [Claims on GitHub](#claims-on-github). |
| An issue-dispatch run started nothing | The listed issues were all claimed, `gh` failed, or the clone's `origin` does not match `repo`. | Read the `[scheduler] task "<name>": …` lines: `skipping already-claimed issue` (the end of the line says why: its worktree exists, an open pull request targets its branch, or another schedule's claim), `repo … dispatch error`, `issue #<n> … failed`. |
| An issue is skipped with `claimed by schedule "<other>" on machine h1-… (GitHub account <login>)` | `claim_gating` is on and another schedule, or the same schedule on another machine, claimed the issue within `claim_ttl_hours`, signed in as an account that is the repository's owner, a member or a collaborator. | Leave it to that schedule, or, if nobody is working on it, remove the `agent-deck/claimed` label from the issue; the next run that considers it dispatches it. See [Skip issues that others have claimed](#skip-issues-that-others-have-claimed). |
| The log has `warning: schedule "<name>": claim_gating is OFF` | `claim_ttl_hours` is shorter than twice the longest gap between the schedule's runs plus one hour. The schedule loads, runs and writes claims, but skips nothing for other schedules' claims. | Raise `claim_ttl_hours` to the value the line names, or run the schedule more often, then `dot-agent-deck schedule reload`. |
| The log has `dispatched issue #<n> … without checking its claims` | `claim_gating` is on, but the issue's claims could not be read from GitHub (for example a timeout, the run's two-minute GitHub time limit being used up, or the account's GitHub API rate limit being used up). The issue was dispatched, possibly while someone else holds it. | Read the reason at the end of the line. If another schedule had claimed the issue, close one of the two tabs. |

## Worked examples

### A daily single-agent digest

```toml
[[scheduled_tasks]]
name = "morning-digest"
cron = "0 9 * * MON-FRI"          # 09:00 Monday to Friday, daemon local time
working_dir = "~/scheduled/morning-digest"
command = "claude"
prompt = """
Summarize the GitHub issues opened in the last 24 hours
in vfarcic/dot-ai and vfarcic/dot-agent-deck.
"""
new_tab_per_fire = false
enabled = true
```

`~/scheduled/morning-digest` has no `.dot-agent-deck.toml`, so each run uses one `claude` agent, reusing its tab.

### A schedule that opens an orchestration

`~/work/release-audit/.dot-agent-deck.toml`:

```toml
[[orchestrations]]
name = "release-audit"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
```

The schedule:

```toml
[[scheduled_tasks]]
name = "weekly-release-audit"
cron = "0 8 * * MON"               # 08:00 every Monday
working_dir = "~/work/release-audit"
command = "claude"                 # required, but not used: the roles' commands are
prompt = "Audit everything merged into main since last Monday and delegate the per-area review."
shape = "orchestration:release-audit"
```

The `shape` is optional here; setting it makes a missing orchestration an error instead of a single agent.

### An issue-dispatch schedule

```toml
[[scheduled_tasks]]
name = "Issues vfarcic/dot-ai"
cron = "0 9 * * MON-FRI"
working_dir = "~/dispatch"
prompt = "Work on issue {{issue_number}}"
enabled = true

[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"
max_per_run = 3
label = "agent-eligible"
```

## Reference

### The schedules file

One file per user holds every schedule on a machine:

| Platform | Path |
|---|---|
| Linux, macOS | `$XDG_CONFIG_HOME/dot-agent-deck/schedules.toml`, or `~/.config/dot-agent-deck/schedules.toml` when `XDG_CONFIG_HOME` is unset or empty |
| Windows | `%APPDATA%\dot-agent-deck\schedules.toml` |

`DOT_AGENT_DECK_SCHEDULES` replaces the whole path. The CLI and the daemon each read it from their own environment, so set it for both or for neither. A missing file means no schedules. The deck writes the file with owner-only permissions (`0600` on Unix), because prompts may contain secrets.

You can edit the file by hand. A running daemon does not notice the edit until you run `dot-agent-deck schedule reload` or it restarts. When a `schedule` command or the TUI's Schedules manager later changes the file, it rewrites the whole file: comments and formatting are not kept, and every `working_dir` is stored with `~` and `$VAR` already expanded.

The file contains an array of `[[scheduled_tasks]]` tables. An entry that does not parse or is invalid is skipped and reported; the other entries still load. If two entries share a name, only one of them runs.

### `[[scheduled_tasks]]` keys

| Key | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `name` | string | yes | — | Unique name. `schedule` commands select by it, and it identifies the tab a run reuses. Cannot be renamed. |
| `cron` | string | yes | — | When to run; see [Cron syntax](#cron-syntax). |
| `working_dir` | string | yes | — | Where the run opens. `~`, `$VAR` and `${VAR}` are expanded (an undefined variable becomes empty), and a relative path is taken relative to your home directory. Created when a run needs it. For issue dispatch, the directory the repository is cloned into. |
| `command` | string | yes, except for issue dispatch | — | The command a single-agent run starts, for example `claude --model opus`. Must be non-blank. Not used when the run opens an orchestration. For issue dispatch, optional: used for an issue whose clone has no orchestration, instead of `default_command`. |
| `prompt` | string | yes | — | Sent to the agent, or to the orchestrator. For issue dispatch, `{{issue_number}}` is replaced with each issue's number. |
| `shape` | string | no | unset | `single`, `orchestration` or `orchestration:<name>`; see [Choose what a run opens](#choose-what-a-run-opens). Any other value, or `orchestration:` with an empty name, makes the entry invalid. Not allowed together with `issue_dispatch`. |
| `new_tab_per_fire` | boolean | no | `false` | `false` reuses the previous run's tab; `true` opens a new one each run. |
| `enabled` | boolean | no | `true` | `false` keeps the entry but does not run it. |

### `[scheduled_tasks.issue_dispatch]` keys

The table's presence makes the entry an issue-dispatch schedule. It must follow the entry's other keys.

| Key | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `repo` | string | yes | — | `owner/name`. Each part is non-empty and contains only letters, digits, `.`, `_` and `-`; it must not start with `-`. |
| `max_per_run` | integer | no | `3` | How many open issues a run lists and considers. At least `1`. |
| `label` | string | no | unset | Only issues with this label (`gh issue list --label`). Must not be empty or start with `-`. |
| `query` | string | no | unset | A GitHub search query passed to `gh issue list --search`, for example `no:assignee`. Must not be empty or start with `-`. |
| `claim_gating` | boolean | no | `false` | `true` skips issues that another schedule or machine has a live claim on; see [Skip issues that others have claimed](#skip-issues-that-others-have-claimed). Claims are written either way. Set in the file only: `schedule add` has no flag for it. |
| `claim_ttl_hours` | integer | no | `168` | How long after its last update a claim stays live. At least `1`, and at least twice the longest gap between the schedule's runs plus one hour for `claim_gating` to take effect. Set in the file only. |

### Cron syntax

A `cron` value has 5, 6 or 7 space-separated fields. The 5-field form is the usual one; the deck adds a seconds field of `0` in front of it.

| Form | Fields |
|---|---|
| 5 fields | `minute hour day-of-month month day-of-week` |
| 6 fields | `second minute hour day-of-month month day-of-week` |
| 7 fields | `second minute hour day-of-month month day-of-week year` |

| Field | Values |
|---|---|
| second, minute | `0`–`59` |
| hour | `0`–`23` |
| day-of-month | `1`–`31` |
| month | `1`–`12` or `JAN`–`DEC` |
| day-of-week | `1`–`7` where **`1` is Sunday** and `7` is Saturday, or `SUN`–`SAT` |
| year | a year, for example `2027` |

Each field accepts `*` (any), a value, a range `a-b`, a list `a,b,c`, and a step `*/n` or `a-b/n` or `a/n`. Day-of-month and day-of-week also accept `?`, meaning any. Names are case-insensitive, and ranges of names work (`MON-FRI`, `JAN-MAR`).

Differences from classic Unix cron that matter when you write an expression:

- **Day-of-week numbers start at 1 for Sunday.** `0` is rejected, and `1-5` means Sunday to Thursday. Prefer names: `MON-FRI`.
- **Day-of-month and day-of-week must both match** when both are restricted. `0 9 1 * MON` runs at 09:00 on the 1st of the month only when that day is a Monday, not on every Monday and every 1st.
- `L`, `W` and `#` are not supported.

The shorthands `@yearly`, `@monthly`, `@weekly`, `@daily` and `@hourly` are accepted. `@weekly` runs at 00:00 on Sunday.

| Expression | Runs |
|---|---|
| `0 9 * * MON-FRI` | 09:00, Monday to Friday |
| `30 7 * * *` | 07:30 every day |
| `0 */2 * * *` | every two hours, on the hour |
| `0 8 * * MON` | 08:00 every Monday |
| `0 0 1 * *` | midnight on the 1st of each month |
| `0 18 * * SAT,SUN` | 18:00 on Saturday and Sunday |

`dot-agent-deck schedule list` prints each schedule's next run time, which is the quickest way to check an expression after adding it.

### `dot-agent-deck schedule` subcommands

Every subcommand that changes the file (`add`, `update`, `remove`, `enable`, `disable`) validates its input, writes the file atomically, and then asks the running daemon to reload. Errors are printed to stderr with a non-zero exit status.

| Subcommand | Flags | Notes |
|---|---|---|
| `add` | `--name <NAME>` `--cron <CRON>` `--working-dir <DIR>` `--prompt <TEXT>` (all required); `--command <CMD>` (required unless `--repo`); `--new-tab-per-fire <true\|false>` (default `false`); `--enabled <true\|false>` (default `true`); `--shape <SHAPE>`; `--repo <OWNER/NAME>`; `--max-per-run <N>` (default `3`, with `--repo`); `--label <LABEL>`; `--query <QUERY>` | `--repo` makes an issue-dispatch schedule. `--max-per-run`, `--label` and `--query` only take effect with `--repo`. `--shape` and `--repo` cannot be combined. |
| `update` | `--name <NAME>` (required); `--cron`, `--working-dir`, `--command`, `--prompt`, `--new-tab-per-fire <true\|false>`, `--enabled <true\|false>`, `--shape <SHAPE>` | Omitted flags leave the value unchanged. `--shape ""` clears the shape. No flag renames a schedule or changes issue-dispatch settings. Fails with `no schedule named "<name>"` for an unknown name. |
| `remove` | `--name <NAME>` | Does not close tabs the schedule opened. |
| `enable` | `--name <NAME>` | |
| `disable` | `--name <NAME>` | |
| `list` | — | Reads the file only; prints `No schedules.` when empty. |
| `run-now` | `--name <NAME>` | Needs a running daemon and an enabled schedule. Prints `ran <name>`, or `skipped <name>: previous run still active` (exit 0 for both). |
| `reload` | — | Needs a running daemon. Prints `reloaded; registered: <names>`. |

`--new-tab-per-fire` and `--enabled` take a value (`--enabled false`), not a bare flag.

### Environment variables

| Variable | Read by | Effect |
|---|---|---|
| `DOT_AGENT_DECK_SCHEDULES` | CLI, TUI, daemon | Path of the schedules file. |
| `XDG_CONFIG_HOME` | CLI, TUI, daemon (Linux, macOS) | Base directory of the default schedules file path. |
| `DOT_AGENT_DECK_STATE_DIR` | daemon | Where `daemon.log` goes. |
