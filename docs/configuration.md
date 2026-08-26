---
sidebar_position: 7
title: Configuration
---

# Configuration

## Default Command

```bash
# Set the default command pre-filled in the new-pane form
dot-agent-deck config set default_command "claude"

# Read the current value
dot-agent-deck config get default_command
```

`default_command` is the agent command pre-filled in the **new-pane form**'s Command field and the value that seeds the **schedule-authoring** agent. Both the new-pane form and the Scheduled Tasks **Add/Edit** flow use the same form — you type the command directly into the **Command** field (it accepts `claude`, `opencode`, `pi`, `codex`, `devin`, a path, or any command), pre-filled from `default_command`. If `default_command` is unset, the schedule-authoring agent falls back to `claude`.

When `default_command` is **unset or empty**, the new-pane form's Command field is instead pre-filled with your **last command** — the most recent command you launched from the new-agent form, in any mode (schedule / issue-dispatch authoring included). This value is global, persists across deck restarts, and is only ever pre-filled into the editable field (never auto-run), so you can edit or clear it before you submit. On a fresh install — where you have never launched a command from the form — the field starts blank. An explicit `default_command` always takes precedence over this last-command fallback.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `DOT_AGENT_DECK_SOCKET` | `$XDG_RUNTIME_DIR/dot-agent-deck.sock` or `/tmp/dot-agent-deck-{uid}.sock` | Unix socket path for daemon IPC. `{uid}` in the `/tmp` fallback is the user's POSIX uid, included so two users on the same host get disjoint sockets (the XDG path is already per-user since `XDG_RUNTIME_DIR` typically resolves to `/run/user/{uid}`). |
| `DOT_AGENT_DECK_CONFIG` | `~/.config/dot-agent-deck/config.toml` | Config file path |
| `DOT_AGENT_DECK_SESSION` | `~/.config/dot-agent-deck/session.toml` | Session file path |
| `DOT_AGENT_DECK_LOG` | *(unset)* | When set, enables file-based tracing logs. Empty value or `1` writes to `/tmp/dot-agent-deck.log`; any other value is treated as the target log file path. |
| `RUST_LOG` | `error,dot_agent_deck=info` | Verbosity of the `DOT_AGENT_DECK_LOG` file, in [`tracing` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html). Layered on top of the default, so `RUST_LOG=dot_agent_deck=debug` raises the deck to debug while an unrelated directive leaves it at `info`. No effect unless `DOT_AGENT_DECK_LOG` is also set. |

## Project Configuration

Per-project workspace modes are defined in `.dot-agent-deck.toml` at the project root. This file is loaded automatically when you select a directory in the new-pane flow.

### Quick Example

```toml
[[modes]]
name = "dev"

[[modes.panes]]
command = "git log --oneline -20"
name = "Recent Commits"

[[modes.rules]]
pattern = "cargo\\s+(build|test|check)"
watch = false
```

### Schema Overview

| Block | Key Fields |
|---|---|
| `[[modes]]` | `name` (required), `init_command` (optional), `panes`, `rules`, `reactive_panes` (default: 2) |
| `[[modes.panes]]` | `command` (required), `name` (optional label), `watch` (default: true) |
| `[[modes.rules]]` | `pattern` (regex, required), `watch` (bool), `interval` (seconds) |
| `[[orchestrations]]` | `name` (optional), `default` (bool, default: false), `extends` (optional), `roles` |

For the full reference and more examples, see [Workspace Modes](workspace-modes.md) and [Orchestration](orchestration.md). `default` and `extends` only matter to a project that defines **several** orchestrations — see [Sharing a Workflow Between Orchestrations](#sharing-a-workflow-between-orchestrations) and [Which Orchestration a Scheduled Task Opens](#which-orchestration-a-scheduled-task-opens) at the end of this page.

### Top-Level Keys

These belong to no block, which makes their placement load-bearing: TOML assigns every key after a table header to that table, so a top-level key **must appear above the first `[[modes]]` or `[[orchestrations]]` header in the file**. Appended at the end it silently becomes a key of whichever table came last, where nothing reads it — the file still parses, `dot-agent-deck validate` still reports `Config is valid.`, and the default stays in effect. A misplaced key gives you no signal at all.

| Key | Default | Description |
|-----|---------|-------------|
| `worker_response_timeout_minutes` | `120` | How long a delegated worker may go without signalling `work-done` before the daemon reports it to the orchestrator. Accepted range `1`–`10080` (one minute to seven days); an out-of-range value falls back to the default rather than being clamped. **`0` disables the detector entirely** — it does not mean "report immediately". Applies to orchestrations only; see [Idle Workers & Notifications](idle-workers-and-notifications.md). |

### Scaffolding

Run `dot-agent-deck init` inside a project directory to generate a starter `.dot-agent-deck.toml`.

### Sharing a Workflow Between Orchestrations

*Both of the remaining sections apply only to a project that defines **several** `[[orchestrations]]` — most often one per kind of work, so a feature that needs a test-plan gate and a release step is not run by the same team as a one-line bug fix. With a single orchestration neither key is needed.*

Two orchestrations that share a workflow — the same roles, the same prompts, the same order — should not be two copies of it. `extends` lets one inherit another's roles, so the second is only what actually differs. The clearest case is a set of provider variants, where that is just each role's `command`:

```toml
[[orchestrations]]
name = "mixed"
default = true

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent-orchestrator"
start = true
prompt_template = """
You coordinate the team. …
"""

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder"
description = "Implements features, fixes bugs"

[[orchestrations]]
name = "GPT"
extends = "mixed"

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent-orchestrator-oc"

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder-oc"
```

`GPT` gets both roles with `mixed`'s `start`, `description` and `prompt_template` intact; only the two commands differ. Editing the orchestrator's `prompt_template` in `mixed` changes it for every variant — which is the point, and the reason to prefer this over copying the block.

The rules:

- **`extends` names the parent's literal `name`.** The parent may appear anywhere in the file, above or below. A block with no `name` cannot be a parent.
- **Roles are matched by name and the parent's ORDER is kept.** A role's position within the orchestration is what the tab layout and delegation key panes on, so a variant always opens with the same columns as its parent, whatever order you write the overrides in.
- **An omitted field keeps the parent's value.** Restate only what differs. To turn off an inherited `clear = true`, write `clear = false` explicitly — an omitted boolean means "inherit", not "false".
- **A role name the parent does not have is added** as a new role, and must carry its own `command` since there is nothing to inherit one from.
- **Chains work** (`a` extends `b` extends `c`); a cycle is rejected when the file is read.
- **`default` and `name` are never inherited** — they identify the block, not its workflow.

An `extends` naming an orchestration that does not exist, or forming a cycle, fails the whole config to load with a message naming both sides. That is deliberate: the alternative leaves the variant with only the roles it restated, and the symptom is then "orchestration must have at least 2 roles" about a file that plainly has six.

### Which Orchestration a Scheduled Task Opens

**Most of the time nothing needs a default.** Both ways of starting an orchestration by hand ask you which one: the new-pane form (`Ctrl+n`) lists every orchestration as a Mode chip to cycle through, and a [dispatcher pane](dispatcher-mode.md) lists them and asks before it starts anything.

`default = true` is for the case where **there is nobody to ask** — a [scheduled task](scheduled-tasks.md) whose working directory defines orchestrations. It fires on a cron tick, and something has to decide which team it opens:

```toml
[[orchestrations]]
name = "prd"
default = true
# roles …

[[orchestrations]]
name = "issue"
# roles …
```

`default` sits on the block, so it moves with the block. Exactly one orchestration may declare it, and that orchestration must define roles — `dot-agent-deck validate` rejects both mistakes. **With a single orchestration the key does nothing; omit it.**

**If nothing declares it, the first orchestration with roles wins.** That is the historical rule and it still applies, so a config written before this key keeps behaving identically. With several orchestrations it is worth declaring anyway, because reordering the file then changes which team every scheduled run opens, and nothing in that diff says so.

When the choice is left implicit, the deck says so rather than quietly picking. `dot-agent-deck validate` is where **you** see it:

```
$ dot-agent-deck validate
[warning] 'prd': 2 orchestrations are defined and none declares `default = true`, so a dispatch or scheduled task that names none opens this one purely because it comes first in the file — reordering the file would silently change that. Add `default = true` to the one you want.
```

A **dispatcher agent** is told the same thing in its own words, and its listing marks the default so it can act on *"just use the usual one"* rather than asking twice:

```
Available dispatch targets:
  single            one agent (--single)
  orchestration     'prd' — 6 roles (--orchestration 'prd')  [default]
  orchestration     'issue' — 4 roles (--orchestration 'issue')

Ask the user which they want before dispatching, then pass the matching flag.
```

A **scheduled task** has nobody to tell, so its copy goes only to the daemon log. That is the whole reason to declare the default: it is the one path where the deck cannot ask you and cannot show you that it did not.
