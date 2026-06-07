---
sidebar_position: 5.7
title: Scheduled Tasks
---

# Scheduled Tasks

Scheduled tasks let you say *"every weekday at 09:00, run this prompt in this
directory"* and have the result land in the deck where you can read it after a
notification — no opening a terminal at the right time, `cd`-ing to the right
place, and pasting the prompt by hand.

A scheduled task is two small primitives wired together: **a cron fires**, and
**a tab opens from a working directory plus a prompt**. The prompt is delivered
into a live agent (or an orchestration), exactly as if you had spawned it from
the new-deck dialog.

:::info The scheduler lives in the daemon
Scheduling runs inside the long-lived **daemon**, not the TUI, so fires keep
happening after you close the deck window. It does **not** survive the daemon
itself stopping — see [Daemon must be running](#daemon-must-be-running).
:::

## The global config file

Schedule **definitions** live in a single global, per-user file:

```
~/.config/dot-agent-deck/schedules.toml
```

(honoring `$XDG_CONFIG_HOME` when set; override the path with the
`DOT_AGENT_DECK_SCHEDULES` environment variable). It is **global** — not the
per-project `.dot-agent-deck.toml` — because the daemon is global; which
schedules are active must not depend on which directory you last launched the
deck from.

Each task is a `[[scheduled_tasks]]` block:

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
new_tab_per_fire = false
enabled = true
```

### Field reference

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Unique id. Also the **reuse-tab registry key** — see [Tab reuse](#tab-reuse). Renaming is forbidden (it would orphan an open reused tab); treat a rename as remove + add. |
| `cron` | string | yes | A **5-field POSIX** cron expression (`min hour day-of-month month day-of-week`), e.g. `0 9 * * MON-FRI`. Evaluated in **local time**. 6/7-field forms (with a seconds field) are also accepted. |
| `working_dir` | string | yes | Directory the fire spawns into. `~` and `$VAR` / `${VAR}` are expanded at load time; a relative path resolves against `$HOME` (never the authoring agent's cwd). Created with `mkdir -p` if missing. |
| `command` | string | no | The agent command for the **single-agent** card (mirrors the new-deck dialog's command field). Falls back to `$SHELL` when omitted. **Ignored** when the target dir defines an `[[orchestrations]]` block — the orchestration's role commands win. |
| `prompt` | string | yes | The prompt delivered into the spawned agent (or the orchestrator role). |
| `new_tab_per_fire` | bool | no (default `false`) | `false` reuses one tab per task; `true` opens a fresh tab every fire. See [Tab reuse](#tab-reuse). |
| `enabled` | bool | no (default `true`) | `false` keeps the definition but stops it firing. |

:::note Local time & daylight saving
Cron is evaluated in the host's **local time** — there is no timezone field. At a
daylight-saving transition this means a fire may be **skipped** (the
spring-forward hour never occurs) or **run twice** (the fall-back hour repeats).
This is an accepted tradeoff of local-time scheduling; if you need exactness
across a DST boundary, avoid scheduling inside the transition hour.
:::

### What a fire spawns into

When a task fires, the scheduler reads the **target `working_dir`'s**
`.dot-agent-deck.toml`:

- If it defines an **`[[orchestrations]]`** block → an **orchestration tab** is
  opened rooted at that directory and the prompt is delivered to the
  `orchestrator` role.
- Otherwise → a **single agent card** is opened, running `command` (or `$SHELL`
  if omitted), and the prompt is delivered to it.

A single malformed `[[scheduled_tasks]]` entry never crashes the daemon or
blocks the other (valid) entries — the bad entry is reported and skipped.

## Three ways to edit schedules

All edits funnel through one validated writer (cron validation, `~`/`$VAR`
expansion, atomic write to the fixed global path regardless of your current
directory, daemon reload). Pick the ergonomics you want.

### 1. Agent-driven authoring (primary)

The easiest door: converse with an agent that builds the entry and calls the
CLI for you. Open the new-deck dialog (`Ctrl+n`), confirm a directory, and cycle
the **Mode** field to the end — past your project's workload modes — to the
built-in **`schedule`** option (marked as an *authoring session*). Selecting it
spawns a throwaway `claude` session pre-seeded with instructions: it asks you
for the fields, lets you **test the prompt in the same session** ("run it now,
show me") before committing, confirms the full entry with you, and then calls
`schedule add`. The agent never hand-edits TOML, so it can't silently produce a
malformed cron or an unescaped multi-line prompt.

This is also where the [management dialog](#management-the-scheduled-tasks-dialog)
sends you for **add** and **edit**.

### 2. The `schedule` CLI

Scriptable, and the fast path for trivial edits:

```bash
# Add a task (the single validated writer):
dot-agent-deck schedule add \
  --name morning-digest \
  --cron "0 9 * * MON-FRI" \
  --working-dir ~/scheduled/morning-digest \
  --command claude \
  --prompt "Generate the morning brief. Notify when done." \
  --enabled true

# Update fields of an existing task (no --new-name; rename is forbidden):
dot-agent-deck schedule update --name morning-digest --cron "0 8 * * MON-FRI"

# Pause / resume without deleting:
dot-agent-deck schedule disable --name morning-digest
dot-agent-deck schedule enable  --name morning-digest

# Inspect:
dot-agent-deck schedule list

# Fire now, or ask a running daemon to re-read the file:
dot-agent-deck schedule run-now --name morning-digest
dot-agent-deck schedule reload

# Remove the definition (does NOT close an open tab for the task):
dot-agent-deck schedule remove --name morning-digest
```

| Subcommand | Purpose |
|---|---|
| `add` | Append a new task. |
| `update` | Change fields of an existing task by `name`. No rename. |
| `remove` | Delete a task **definition** (leaves any open tab alive). |
| `list` | Show each task with its enabled/disabled state and next-fire time. |
| `enable` / `disable` | Flip `enabled` without deleting the definition. |
| `run-now` | Fire the task immediately via the running daemon. |
| `reload` | Tell the running daemon to re-read `schedules.toml`. |

After any mutating command the CLI triggers a live daemon reload, so a running
daemon picks the change up immediately. If no daemon is running that's fine —
the change loads on the next `daemon serve`.

### 3. Hand-edit the file

The TOML is human-readable; edit `~/.config/dot-agent-deck/schedules.toml`
directly, then run `dot-agent-deck schedule reload` (or just let the next daemon
start pick it up).

## Management: the "Scheduled Tasks" dialog

Press **`S`** on the dashboard (command mode) to open the **Scheduled Tasks**
manager — the canonical home for the concept. It is *read-only-plus-actions*: it
lists schedules but does not edit fields in place (mutation goes through the
agent / CLI / file).

Each row shows the task **name**, a **status** indicator, and its **next-fire**
time:

| Status | Meaning |
|---|---|
| `live` | The task currently has a live reused tab/agent. |
| `idle` | Enabled, but no live tab right now. |
| `disabled` | `enabled = false`. Its next-fire cell shows `—`. |

Actions:

| Key | Action |
|---|---|
| `a` | **Add** — spawns the seeded authoring agent (blank). |
| `Enter` / `e` | **Edit** the selected row — spawns the seeded authoring agent **pre-filled** with the row's current values (it calls `schedule update`). |
| `d` then `y` | **Delete** the selected row's **definition only** (a confirmation appears first). It does **not** close an open/running tab for that schedule — deleting a schedule must not nuke a conversation you're reading. |
| `r` | **Run now** — fire the selected task immediately. |
| `j` / `k` | Move the selection. |
| `Esc` / `q` | Close the dialog. |

There is deliberately **no inline enable/disable toggle** and **no in-place
field editing** — that keeps the terminal dialog simple. Pause a task via the
agent, `schedule disable <name>`, or a file edit. **Rename is forbidden** via the
edit path because `name` is the reuse-tab key; to rename, remove + add.

## Tab reuse

Most scheduled tasks should **reuse** one tab, because you primarily learn about
fires through notifications and open the deck to dig into a result only when you
choose to.

- **Default (`new_tab_per_fire = false`)** — a task reuses the same tab/card each
  fire. Yesterday's weather output is replaced by today's. One weather tab, ever.
- **Opt-in (`new_tab_per_fire = true`)** — each fire opens a fresh tab, for
  audit-style tasks where you want per-fire history.

The reuse registry is keyed by task **name** and lives **in memory in the
daemon**, so it is **wiped on daemon restart** — the first fire after a restart
creates a fresh tab even under reuse.

### Mid-interaction deliver-on-idle

If a reuse fire lands while you are actively typing in that tab, the new prompt
is **queued** and delivered once the pane goes idle (a short debounce, ~5s by
default). If you are not typing, it is delivered immediately. The debounce
window is tunable via the `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` environment variable
(milliseconds).

## Daemon must be running

Scheduling depends on the daemon being up. The behavior on daemon
stop / upgrade / restart / reboot is honest and documented:

- Stopping the daemon (`daemon stop`, `daemon restart`, an upgrade, or a crash)
  **terminates every running agent** and **wipes the in-memory reuse registry**.
- **There is no catch-up.** Fires that come due while the daemon is down are
  **not replayed** — an "every 09:00" task that was offline at 09:00 simply
  misses that day. There is no persistent queue and no last-fire timestamp.
- Schedule **definitions survive** because they are reloaded from the global
  `schedules.toml` the next time the daemon starts.
- The daemon is **lazy-spawned** by the next `dot-agent-deck` invocation and is
  **not** auto-respawned after it exits.

The daemon also auto-exits after a short idle window when there are no clients
and no live agents — but a **registered enabled schedule keeps it alive**, so a
daily task survives the gaps between fires (and the gap before its first fire)
as long as the daemon isn't explicitly stopped.

For genuinely unattended scheduling across reboots, run the daemon under a
supervisor — see the next section.

## Optional: run the daemon under a supervisor

For "fires at 09:00 even if I never open the deck", keep the daemon always-on
with your init system. Disable idle shutdown so it doesn't exit between fires by
setting `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0`. This is an optional recipe, not a
built-in service.

### Linux (systemd user unit)

`~/.config/systemd/user/dot-agent-deck.service`:

```ini
[Unit]
Description=dot-agent-deck daemon
After=default.target

[Service]
Type=simple
Environment=DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0
ExecStart=%h/.local/bin/dot-agent-deck daemon serve
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now dot-agent-deck.service
loginctl enable-linger "$USER"   # keep the user manager running across logouts
```

### macOS (launchd LaunchAgent)

`~/Library/LaunchAgents/com.dot-agent-deck.daemon.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>com.dot-agent-deck.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/dot-agent-deck</string>
    <string>daemon</string>
    <string>serve</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS</key> <string>0</string>
  </dict>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <true/>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/com.dot-agent-deck.daemon.plist
```

Make sure the daemon and your interactive deck resolve the **same** socket and
schedules paths (the defaults already do); a supervised daemon and a
lazy-spawned one read the same global `schedules.toml`, so there is no migration.

## Worked examples

### A daily single-agent digest

```toml
# ~/.config/dot-agent-deck/schedules.toml

[[scheduled_tasks]]
name = "morning-digest"
cron = "0 9 * * MON-FRI"          # 09:00 on weekdays, local time
working_dir = "~/scheduled/morning-digest"
command = "claude"                 # single-agent card; falls back to $SHELL if omitted
prompt = """
Generate a brief: Barcelona weather forecast for today, plus GitHub issues
opened in the last 24h across vfarcic/dot-ai and vfarcic/dot-agent-deck.
Notify when done.
"""
new_tab_per_fire = false           # reuse one tab (default)
enabled = true
```

`~/scheduled/morning-digest` has no `.dot-agent-deck.toml`, so the fire opens a
single `claude` card there and delivers the prompt.

### A scheduled task that targets an orchestration

If the target directory defines an orchestration, the fire opens an
orchestration tab and delivers the prompt to the `orchestrator` role — the
schedule's `command` is ignored.

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
# command is omitted on purpose — the orchestration's role commands win.
prompt = """
Audit everything merged into main since last Monday: changelog accuracy,
breaking changes, and follow-up issues to open. Delegate the per-area review.
"""
enabled = true
```
