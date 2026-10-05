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
- **For issue dispatch only:** the GitHub CLI `gh`, installed and signed in (`gh auth status` succeeds), and `git`, both on the daemon's `PATH`.

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

When a run reuses a tab you are typing in, its prompt waits until you have not typed for 5 seconds. If you left unsent text in that pane (in either client), it also waits until you press Enter or clear the text with `Ctrl+U` or `Ctrl+C`, so it is not submitted together with your text; see [A deck prompt waits while you have an unsent draft](orchestration.md#a-deck-prompt-waits-while-you-have-an-unsent-draft). Either way the prompt is sent at the latest 60 seconds after the run started. To change the 5 seconds, set `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` (milliseconds) in the environment the daemon starts with.

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
3. **Skips claimed issues.** An issue is skipped when its worktree `<clone>/.worktrees/issue-<n>` already exists, or when an open pull request has the head branch `agent/issue-<n>`. Skipped issues still count toward `max_per_run`: if every listed issue is claimed, the run starts nothing, even when later issues are free.
4. **Creates the worktree** at `<clone>/.worktrees/issue-<n>` on the branch `agent/issue-<n>`. If that branch already exists in the clone (from an earlier run whose tab you closed without opening a pull request), the worktree checks it out, so its commits carry over.
5. **Starts the agent** in the worktree and sends it the prompt. If the worktree's `.dot-agent-deck.toml` defines an orchestration with roles, the issue gets an orchestration tab, using the roles' own commands. Otherwise it gets one agent running the schedule's `command`, else your [`default_command`](configuration.md#set-the-command-new-agents-start-with); if neither is set, the agent pane starts the daemon's shell (`$SHELL`), which cannot act on the prompt, so set one of them.

A failure on one issue (a `gh` error, a worktree error) is reported and the run carries on with the other issues. A failure to get the repository or list issues stops that run.

### Clean up

Dispatched tabs stay open until you close them. **Closing an issue's tab removes its worktree with `git worktree remove --force`, which discards uncommitted changes in it.** Commit or push anything you want to keep first. The branch and the clone stay, and the issue becomes eligible again on the next run unless it has an open pull request.

After the daemon restarts, closing such a tab no longer removes the worktree. Remove it yourself with `git -C <clone> worktree remove .worktrees/issue-<n>`, or that issue stays skipped.

An issue that is still open after its pull request merged or closed has no open pull request any more, so once its worktree is gone a later run dispatches it again on the existing branch. Close the issue (or remove the label) to stop that.

## Keep schedules running

- **Closing the TUI or the desktop app does not stop schedules**, and the daemon keeps running while at least one enabled schedule exists.
- **Nothing runs while the daemon is stopped.** A run whose time passed while the daemon was stopped, or while the machine was asleep for more than a minute, is not made up later.
- **Stopping the daemon stops the agents it runs**, including those schedules started. `dot-agent-deck daemon stop` may refuse while agents are running unless you pass `--force`. After a restart, the next run of each schedule opens a new tab.
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
| An issue-dispatch run started nothing | The listed issues were all claimed, `gh` failed, or the clone's `origin` does not match `repo`. | Read the `[scheduler] task "<name>": …` lines: `skipping already-claimed issue`, `repo … dispatch error`, `issue #<n> … failed`. |

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
| `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` | daemon | How long a reused tab must be free of typing before a run's prompt is sent. Default `5000`. |
| `DOT_AGENT_DECK_STATE_DIR` | daemon | Where `daemon.log` goes. |
