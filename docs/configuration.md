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

For the full reference and more examples, see [Workspace Modes](workspace-modes.md) and [Orchestration](orchestration.md).

### Choosing the Default Orchestration

A project may define several `[[orchestrations]]` — commonly the same team of roles wired to different providers. Most of the time you name the one you want:

```bash
dot-agent-deck dispatch fix-auth --orchestration 'anthropic' --task '…'
```

But two things start an orchestration **without** naming one: `dispatch --orchestration=` with an empty value, and a [scheduled task](scheduled-tasks.md) whose working directory defines orchestrations. Add `default = true` to the block those should open:

```toml
[[orchestrations]]
name = "mixed"
default = true
# roles …

[[orchestrations]]
name = "anthropic"
# roles …
```

`default` sits on the block, so it moves with the block. Exactly one orchestration may declare it, and that orchestration must define roles — `dot-agent-deck validate` rejects both mistakes.

**If nothing declares it, the first orchestration with roles wins.** That is the historical rule and it still applies, so a config written before this key keeps behaving identically. It is worth declaring anyway: with several orchestrations defined, reordering the file changes which one every unnamed run opens, and nothing in that diff says so. When the choice is left implicit, the deck says which one it took and what else was available — in the dispatch's reply, in `dispatch --list-targets`, in `dot-agent-deck validate`, and in the daemon log for a scheduled run that has nobody watching.

`dispatch --list-targets` marks the answer:

```
Available dispatch targets:
  single            one agent (--single)
  orchestration     'mixed' — 6 roles (--orchestration 'mixed')  [default]
  orchestration     'anthropic' — 6 roles (--orchestration 'anthropic')
```

### Sharing One Orchestration Across Providers

`extends` lets one orchestration inherit another's roles, so a set of variants that differ only in which agent each role launches is written once:

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

### Top-Level Keys

These belong to no block, which makes their placement load-bearing: TOML assigns every key after a table header to that table, so a top-level key **must appear above the first `[[modes]]` or `[[orchestrations]]` header in the file**. Appended at the end it silently becomes a key of whichever table came last, where nothing reads it — the file still parses, `dot-agent-deck validate` still reports `Config is valid.`, and the default stays in effect. A misplaced key gives you no signal at all.

| Key | Default | Description |
|-----|---------|-------------|
| `worker_response_timeout_minutes` | `120` | How long a delegated worker may go without signalling `work-done` before the daemon reports it to the orchestrator. Accepted range `1`–`10080` (one minute to seven days); an out-of-range value falls back to the default rather than being clamped. **`0` disables the detector entirely** — it does not mean "report immediately". Applies to orchestrations only; see [Idle Workers & Notifications](idle-workers-and-notifications.md). |

### Scaffolding

Run `dot-agent-deck init` inside a project directory to generate a starter `.dot-agent-deck.toml`.

