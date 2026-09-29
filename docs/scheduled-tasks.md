---
sidebar_position: 5.7
title: Schedules
---

# Schedules

Schedules let you say *"every weekday at 09:00, run this prompt in this directory"* and have the result land in the deck where you can read it after a notification — no opening a terminal at the right time, `cd`-ing to the right place, and pasting the prompt by hand.

Each schedule pairs **when it runs** (a cron expression) with **what runs** (a working directory and a prompt). When it comes due, the deck opens a tab in that directory and hands the prompt to a fresh agent — or to an orchestration, if that directory defines one — exactly as if you had started it yourself.

> **Schedules keep running after you close the deck**
>
> They run in the deck's background **daemon**, so closing the deck window does not stop them. While the daemon is stopped nothing runs, and a run that came due meanwhile is **not** made up later — but your schedules come back the next time the daemon starts. See [Daemon must be running](#daemon-must-be-running).

## Creating and managing schedules

**Do this in the deck.** Open the **Schedules** dialog and an agent writes the schedule for you — you describe the job in plain English instead of getting cron syntax and TOML right by hand.

The [reference section](#reference) below describes the file the agent writes, so you can read it back and know the field names (`cron`, `working_dir`, `shape`, …) to ask for what you want.

### The Schedules dialog

*The Schedules dialog is in the TUI only. The desktop app has no schedule manager: it can start the authoring agent from **New agent** (below), and schedules it creates are listed and changed from the TUI or with `dot-agent-deck schedule`.*

Press **`s`** on the dashboard (lowercase; the legacy uppercase **`S`** also works) to open the **Schedules** manager — your one place to see and manage every schedule. Its **`[Schedules s]`** button is **always present on the dashboard**: it doesn't wait for a schedule to exist, because the manager's **`[Add]`** action is itself how you create the first one. You never type field values into the dialog itself — **`[Add]`** and **`[Edit]`** hand you to the authoring agent described below, which does the writing for you.

![The TUI's Schedules manager with one schedule: its row shows the name, the status disabled and a next fire of —, above the Add, Edit, Delete, Run now and Toggle buttons](/img/schedules-tui.png)

Rows are **click-selectable**. Each row shows the task **name**, a **status** indicator, and its **next-fire** time:

| Status | Meaning |
|---|---|
| `live` | The schedule's tab is open with its agent running. |
| `idle` | Enabled, but no tab open right now. |
| `disabled` | Paused (`enabled = false`). Its next-run cell shows `—`. |

Actions — the footer buttons mirror the keys, shown as `[Add a]` `[Edit e]` `[Delete d]` `[Run now r]`:

| Key / Button | Action |
|---|---|
| `a` / `[Add a]` | **Add** — pick a directory, confirm the **New Schedule** form (Dir and Command), and the authoring agent starts in that directory. |
| `Enter` / `e` / `[Edit e]` | **Edit** the selected schedule — the same steps, starting at its directory, and the authoring agent starts with its current values. |
| `d` then `y` / `[Delete d]` | **Delete** the selected schedule, after a confirmation. A tab it already opened stays open. |
| `r` / `[Run now r]` | **Run now** — run the selected schedule immediately. |
| `t` / `[Toggle t]` | **Pause / resume** the selected schedule. No confirmation: press `t` again to undo it. |
| `j` / `k` | Move the selection. |
| `Esc` / `q` / `s` | Close the dialog. |

**Edits apply to the next run.** Change the prompt, cron, working directory, command or `new_tab_per_fire`, and the next run uses the new values.

**A schedule cannot be renamed.** To change its name, delete it and add a new one.

**Pause rather than delete** when you only want a schedule to stop for a while — while you are away, or while you debug what it drives. `[Delete d]` throws the schedule away; **`[Toggle t]`** keeps everything and just stops it running until you press `t` again.

### What the authoring agent does

Both doors below open the same guided authoring agent (the desktop app has only the second):

- **From the Schedules dialog** — press **`s`** on the dashboard, then **`a`** / **`[Add]`** to author a new one (or **`e`** / **`[Edit]`** to start from an existing row's values). First a **directory picker** (the dir you choose becomes the authoring agent's working directory, and is pre-seeded as the schedule's own working directory), then a small **New Schedule** / **Edit Schedule** form with a **Dir** and a free-text **Command** field (pre-filled from your `default_command`). Confirm to start the authoring agent in that directory running that command; **`Esc`** / **`[Cancel]`** returns you to the dialog.
- **From the New Agent form** — open it (`Ctrl+n`), confirm a directory, and cycle the **Mode** field to the end — past your project's workload modes — to the built-in **`schedule`** option (marked `authoring (one-off)`). In the desktop app, open **New agent**, choose a directory and pick the **schedule** chip under **Mode**.

Either way an agent opens — running the command you chose, which defaults to your [`default_command`](configuration.md#default-command), or `claude` if that is unset — and walks you through it. It:

- asks you for the fields (name, cron, working directory, command, prompt, …);
- asks for the **command that launches your agent** — one that starts `claude`, `opencode`, `pi`, `codex` or `devin`, directly (`claude --model opus`, `opencode --model gpt-4o`) or through a project wrapper (`devbox run agent-new`, `npm run agent`). Any other command runs, but the deck cannot track its status. The command is **required**;
- lets you **try the prompt with the same agent** before saving;
- **confirms the whole schedule** with you, then saves it.

When it is done it tells you the pane can be closed — it existed only to create the schedule. When the schedule runs, a single-agent run **appears live in its own pane** on the deck, while an orchestration run opens in its tab when you next open the deck. The desktop app shows both on its Dashboard like agents you started yourself: a single-agent run as a row, an orchestration run as an **ORCHESTRATION** group.

## What happens when a schedule runs

What a run opens — its **shape** — is set by the schedule's `shape` field if it has one. Otherwise it depends on the **`working_dir`'s** `.dot-agent-deck.toml`:

- If it defines **`[[orchestrations]]`** → an **orchestration tab** opens in that directory and the prompt goes to the orchestrator (the schedule's `command` is not used).
- Otherwise → a **single agent card** opens, running `command`, and the prompt goes to it.

**Use `shape = "single"` when you want the repo but not the team** — for example, so the repo's `.claude/skills/` load and `git` runs in the right place, with **one** agent doing the job. Without it, a repo that defines `[[orchestrations]]` opens the whole team and ignores the schedule's `command`.

If `shape` names an orchestration the directory does not define, **the run is skipped**: you get a notification listing the ones that exist, and nothing else is opened in its place.

`schedule list` shows each schedule's shape, as `shape=config-derived` when the field is unset.

**The prompt arrives once the agent is ready.** A newly started agent gets a moment to finish starting before the prompt is sent, so nothing is lost.

A malformed `[[scheduled_tasks]]` entry is reported and skipped; your other schedules still run. An entry without a `command` is skipped this way.

## Tab reuse

Most schedules should **reuse** one tab: you usually hear about a run through a notification and open the deck only when you want to look at the result.

- **Default (`new_tab_per_fire = false`)** — each run reuses the same tab. Yesterday's weather report is replaced by today's: one weather tab, ever.
- **Opt-in (`new_tab_per_fire = true`)** — each run opens a new tab, for when you want a history of runs.

After the daemon restarts, the next run opens a new tab even when reuse is on.

### If a run lands while you are typing

If a run reuses a tab you are typing in, its prompt **waits** until you stop typing for about 5 seconds; otherwise it arrives immediately. Set `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` (milliseconds) to change the 5 seconds.

If you have left unsent text in the run's pane — in either client — the prompt also waits until you press Enter or clear it with Ctrl+U or Ctrl+C, so it is not sent together with your text. Either way it waits at most 60 seconds from the start of the run, then arrives anyway. See [A deck prompt waits while you have an unsent draft](orchestration.md#a-deck-prompt-waits-while-you-have-an-unsent-draft).

## Daemon must be running

Schedules only run while the deck's daemon is running. When it stops, restarts, is upgraded, or the machine reboots:

- Stopping the daemon (`daemon stop`, `daemon restart`, an upgrade, or a crash) **stops every running agent**, and the next run of each schedule opens a new tab.
- **Missed runs are not made up.** An "every 09:00" schedule whose daemon was down at 09:00 simply misses that day.
- **Your schedules are kept**: the daemon loads them from `schedules.toml` the next time it starts.
- The next `dot-agent-deck` command starts the daemon again; nothing restarts it automatically.

The daemon normally exits on its own when nothing is using it, but **an enabled schedule keeps it running** between runs, as long as you do not stop it.

## Dispatching agents onto open GitHub issues (`issue_dispatch`)

> **Issue-dispatch schedules always run; only the guided way to create one is experimental.**
>
> A schedule with an `[scheduled_tasks.issue_dispatch]` table runs like any other, with no flag. What is behind the `experimental` flag is the **`schedule: issues`** option in the New Agent form, which lets an agent build one with you. To turn that option on, set `experimental = true` under a `[features]` table in your `.dot-agent-deck.toml`, or launch with `DOT_AGENT_DECK_EXPERIMENTAL=1` (the environment variable wins over the file).

The schedules so far run **one** prompt in **one** directory. An **`issue_dispatch`** schedule instead looks at the **open GitHub issues of one repo** on each run and starts an agent **per issue** — so *"every weekday at 09:00, take up to five open issues from `vfarcic/dot-ai` and start an agent on each"* is one schedule instead of a morning of cloning, making worktrees and pasting prompts.

Add a `[scheduled_tasks.issue_dispatch]` table to an ordinary schedule. The usual fields (`name`, `cron`, `working_dir`, `prompt`, `enabled`) mean the same; the table adds the GitHub-specific ones:

```toml
[[scheduled_tasks]]
name = "Issues vfarcic/dot-ai"        # default-seeded to "Issues <repo>"
cron = "0 9 * * MON-FRI"              # 09:00 on weekdays, local time
working_dir = "~/dispatch"            # the workspace root — see "Where things land" below
prompt = "Work on issue {{issue_number}}"   # per-issue template; {{issue_number}} is substituted per issue
enabled = true

[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"               # ONE repo, "owner/name"
max_per_run = 5                       # hard cap on how many issues a single fire dispatches
# label = "agent-eligible"            # optional: only issues carrying this label
# query = "is:open no:assignee"       # optional: advanced gh search override
```

> **`command` is not used here**
>
> Unlike a plain schedule, an `issue_dispatch` schedule does **not** need a `command`. If the cloned repo defines `[[orchestrations]]`, each issue gets an **orchestration tab** (with the roles' own commands); otherwise it gets a **single-agent card** running your [`default_command`](configuration.md#default-command) (or `claude` if that is unset).

**With the `experimental` flag off, you create one yourself** — either by writing the table above into the file, or with the CLI. It takes `--repo` plus the optional `--max-per-run`, `--label` and `--query`, and needs no `--command`:

```bash
dot-agent-deck schedule add \
  --repo vfarcic/dot-ai \
  --max-per-run 5 \
  --name "Issues vfarcic/dot-ai" \
  --cron "0 9 * * MON-FRI" \
  --working-dir ~/dispatch \
  --prompt "Work on issue {{issue_number}}" \
  --label agent-eligible      # optional
```

A `--repo` that is not `owner/name` is rejected before anything is saved. The CLI checks the schedule, saves it, and the running daemon picks it up straight away.

### What a run does, issue by issue

On each run it:

1. **Gets the repo** under the workspace root — cloning it the first time, pulling the latest changes after that.
2. **Lists open issues** with `gh` (using `label` and `query` if you set them) and takes the first `max_per_run`, in the order GitHub returns them.
3. For each of those issues, **creates a worktree** on a branch named `agent/issue-<n>`.
4. **Starts an agent** in that worktree and sends it your `prompt`, with `{{issue_number}}` replaced. The agent is working inside the issue's worktree, so the issue number is enough context. Use `prompt = "/prd-full {{issue_number}}"` to run your own skill instead.

If one issue fails (a `gh` rate limit, a clone error), you get a deck notification and the run **carries on** with the other issues.

### Where things land

Everything lives under the schedule's `working_dir` (the **workspace root**):

| Path | What |
|---|---|
| `<working_dir>/<name>` | The **clone** of the repo (created once, reused and pulled thereafter). |
| `<working_dir>/<name>/.worktrees/issue-<n>` | The **per-issue worktree** for issue `<n>`. |
| `agent/issue-<n>` | The **branch** each worktree checks out. |

### Re-runs skip issues already in progress

A later run — on schedule, or when you press **Run now** — does not start the same issue twice. An issue is **skipped** when either:

- its `.worktrees/issue-<n>` worktree **already exists**, or
- an **open PR** already has head branch `agent/issue-<n>`.

A skipped issue is reported and left alone, so each run only fills the slots freed since the last one, up to `max_per_run`.

### Cleanup: closing a tab removes its worktree

Dispatched tabs stay open until **you** close them, so you decide when to review, keep iterating, or discard the work. **Closing one removes its worktree** (`git worktree remove`), freeing the slot for a future run; the **clone stays**. Until you close it, later runs skip that issue.

> **Requirements & caveats**
>
> - The **GitHub CLI (`gh`) must be installed and signed in** — listing issues, checking for open PRs and cloning all go through it.
> - **GitHub only, for now.** Other forges (GitLab, Gitea, Bitbucket, …) are not supported yet. If you would like another one, please [open an issue](https://github.com/vfarcic/dot-agent-deck/issues) — it helps us gauge demand.
> - Like every schedule, runs that come due while the daemon is down are **not** made up (see [Daemon must be running](#daemon-must-be-running)).
> - **Closing the deck vs. stopping the daemon.** Closing the deck window leaves the dispatched agents and their tabs **running** — open the deck again and they are still there. **Stopping the daemon** (`daemon stop`, a restart, an upgrade, or a crash) ends them. The worktrees **stay on disk** afterwards, so later runs still skip those issues, but the tabs do **not** come back. Run `git worktree remove` to free a slot yourself.

## Worked examples

### A daily single-agent digest

```toml
# ~/.config/dot-agent-deck/schedules.toml

[[scheduled_tasks]]
name = "morning-digest"
cron = "0 9 * * MON-FRI"          # 09:00 on weekdays, local time
working_dir = "~/scheduled/morning-digest"
command = "claude"                 # required — the single-agent card's command (claude, opencode, pi, codex, or devin)
prompt = """
Generate a brief: Barcelona weather forecast for today, plus GitHub issues
opened in the last 24h across vfarcic/dot-ai and vfarcic/dot-agent-deck.
Notify when done.
"""
new_tab_per_fire = false           # reuse one tab (default)
enabled = true
```

`~/scheduled/morning-digest` has no `.dot-agent-deck.toml`, so the run opens a single `claude` card there and sends it the prompt.

### A schedule that targets an orchestration

If the target directory defines an orchestration, the run opens an orchestration tab and sends the prompt to the orchestrator. The schedule's `command` is **still required** (every schedule needs one) but is **not used** — the roles' own commands are.

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

`~/.config/dot-agent-deck/schedules.toml`:

```toml
[[scheduled_tasks]]
name = "weekly-release-audit"
cron = "0 8 * * MON"               # 08:00 every Monday
working_dir = "~/work/release-audit"
command = "claude"                 # required to load; ignored at fire (the orchestration's role commands win)
prompt = """
Audit everything merged into main since last Monday: changelog accuracy,
breaking changes, and follow-up issues to open. Delegate the per-area review.
"""
enabled = true
```

## Reference

You do not need this section to create or manage a schedule — the authoring agent writes this file for you. It is here so you can **read back** what it wrote, **know the field names** to ask for what you want, and **edit the file by hand** if you prefer.

### The global config file

All your schedules live in one file for your user:

```
~/.config/dot-agent-deck/schedules.toml
```

(under `$XDG_CONFIG_HOME` when that is set; set the `DOT_AGENT_DECK_SCHEDULES` environment variable to use another path). It is **one file for all your projects**, not the per-project `.dot-agent-deck.toml`.

Each schedule is a `[[scheduled_tasks]]` block:

```toml
[[scheduled_tasks]]
name = "morning-digest"
cron = "0 9 * * MON-FRI"
working_dir = "~/scheduled/morning-digest"
command = "claude"
prompt = """
Generate a brief: Barcelona weather forecast for today, plus the list of
GitHub issues opened in the last 24h across vfarcic/dot-ai and
vfarcic/dot-agent-deck. Notify when done.
"""
# shape = "single"        # optional: force ONE agent even where this dir
                          # defines [[orchestrations]]. Omit to derive the
                          # shape from that dir's config (the default).
new_tab_per_fire = false
enabled = true
```

### Field reference

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Unique name, also used to find the schedule's reused tab — see [Tab reuse](#tab-reuse). Cannot be changed; to rename, delete the schedule and add it again. |
| `cron` | string | yes | A **5-field** cron expression (`min hour day-of-month month day-of-week`), e.g. `0 9 * * MON-FRI`, in **local time**. 6- and 7-field forms (with seconds) also work. |
| `working_dir` | string | yes | Directory the run opens in. `~` and `$VAR` / `${VAR}` are expanded; a relative path is relative to your home directory. Created if missing. |
| `command` | string | **yes** | The agent command for a **single-agent** run (e.g. `claude`, `opencode`, `pi`, `codex`, or `devin`), like the command field in the New Agent form. **Required on every schedule** — `schedule add` refuses to save without it, and an entry without one is skipped. **Not used** when the run opens an orchestration; set `shape = "single"` to use it in a directory that defines `[[orchestrations]]`. |
| `prompt` | string | yes | The prompt sent to the agent (or to the orchestrator). |
| `shape` | string | no (default: from the directory) | What the run opens, instead of deciding from `working_dir`'s config. `"single"` opens **one** agent card running `command`, *even where that directory defines `[[orchestrations]]`*; `"orchestration"` opens that directory's default orchestration; `"orchestration:<name>"` opens the one with that name. **Leave it out** to decide from the directory. Any other value is an error naming the schedule. Cannot be combined with `issue_dispatch` (an error). |
| `new_tab_per_fire` | bool | no (default `false`) | `false` reuses one tab; `true` opens a new tab every run. See [Tab reuse](#tab-reuse). |
| `enabled` | bool | no (default `true`) | `false` keeps the schedule but stops it running. |

> **Local time & daylight saving**
>
> Cron uses the machine's **local time** — there is no timezone field. When clocks change for daylight saving, a run in the changed hour may be **skipped** (the hour never happens) or **run twice** (the hour repeats). If that matters, do not schedule inside that hour.

### Hand-editing the file

Edit `~/.config/dot-agent-deck/schedules.toml` directly (see [the global config file](#the-global-config-file) above for the format). A running daemon does not notice on its own — **the deck has no "re-read the file" action** — so your edit takes effect the next time the daemon starts, or straight away if you run:

```bash
dot-agent-deck schedule reload
```

Schedules saved through the deck need no reload.
