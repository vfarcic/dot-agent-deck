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
| `[[modes]]` | `name` (required), `agent` (optional), `init_command` (optional), `panes`, `rules`, `reactive_panes` (default: 2) |
| `[[modes.panes]]` | `command` (required), `name` (optional label), `watch` (default: true) |
| `[[modes.rules]]` | `pattern` (regex, required), `watch` (bool), `interval` (seconds) |
| `[[orchestrations]]` | `name` (optional), `default` (bool, default: false), `extends` (optional), `roles` |

For the full reference and more examples, see [Workspace Modes](workspace-modes.md). Orchestrations live in the same file under `[[orchestrations]]`; see [Orchestration](orchestration.md#configuration-reference). `default` and `extends` only matter to a project that defines **several** orchestrations, and are explained under [More than one orchestration](orchestration.md#more-than-one-orchestration).

### Naming the agent a command launches

The deck identifies which agent a pane runs by reading the first word of its command, so `claude`, `codex`, `opencode --model gpt-4o` and `/usr/local/bin/pi` all resolve by themselves. A command that starts the agent through something else does not — `devbox run -- codex`, `mise exec -- codex`, `nix develop -c codex`, `make codex`, `./run-codex.sh` — because nothing about a launcher reveals what it will end up starting. Such a pane shows **No agent** and gets no status tracking, and for Codex it stays that way until you give it its first task.

The optional `agent` key says what the command cannot. It takes one of `claude`, `opencode`, `pi`, `codex`, `devin`, and it goes on the block that owns the command:

```toml
[[modes]]
name = "review"
agent = "codex"          # this mode's agent pane runs Codex

[[orchestrations.roles]]
name = "reviewer"
command = "devbox run -- codex --sandbox workspace-write"
agent = "codex"          # …and so does this role
```

An unrecognised name resolves to no agent rather than to a guess, and `dot-agent-deck validate` warns about it by name. Omitting the key leaves behaviour exactly as before, so no existing config needs to change. Full details in [Orchestration](orchestration.md#declaring-the-agent-behind-a-launcher-command) and [Workspace Modes](workspace-modes.md#declaring-the-agent-behind-a-launcher-command).

### Top-Level Keys

These belong to no block, which makes their placement load-bearing: TOML assigns every key after a table header to that table, so a top-level key **must appear above the first `[[modes]]` or `[[orchestrations]]` header in the file**. Appended at the end it silently becomes a key of whichever table came last, where nothing reads it — the file still parses, `dot-agent-deck validate` still reports `Config is valid.`, and the default stays in effect. A misplaced key gives you no signal at all.

| Key | Default | Description |
|-----|---------|-------------|
| `worker_response_timeout_minutes` | `120` | How long a delegated worker may go without signalling `work-done` before the daemon reports it to the orchestrator. Accepted range `1`–`10080` (one minute to seven days); an out-of-range value falls back to the default rather than being clamped. **`0` disables the detector entirely** — it does not mean "report immediately". Applies to orchestrations only; see [Idle Workers & Notifications](idle-workers-and-notifications.md). |

### Scaffolding

Run `dot-agent-deck init` inside a project directory to generate a starter `.dot-agent-deck.toml`.
