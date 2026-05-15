---
sidebar_position: 7.4
title: Remote Environments
---

# Remote Environments

A **remote environment** is a per-project Linux host that runs the deck — both the daemon and the TUI live on the remote. Your laptop is just a terminal: `dot-agent-deck connect` is an `ssh -t` wrapper that runs the TUI on the remote, and the local terminal forwards keystrokes in and renders the bytes that come back. When the ssh session ends, the daemon and agents on the remote keep running.

This page covers how that works in practice: the lifecycle model, the difference between "stop" and "detach", the failure modes you'll see when a connect goes wrong, and how hooks behave on the remote.

For host prerequisites see [Remote Environment Requirements](remote-requirements.md). For provisioning recipes see [Remote Recipes](remote-recipes.md).

## Quick start

```bash
# 1. Register a remote (one-time per host).
dot-agent-deck remote add my-vm user@host

# 2. Connect. With no name, an interactive picker opens.
dot-agent-deck connect my-vm
```

`remote add` ssh's into the host, installs the `dot-agent-deck` binary into `~/.local/bin/dot-agent-deck`, runs `hooks install`, and writes a registry entry to `~/.config/dot-agent-deck/remotes.toml`. `connect` runs `ssh -t` to the remote and execs `dot-agent-deck` there with `DOT_AGENT_DECK_VIA_DAEMON=1`, so the TUI runs on the remote and attaches to a local-on-remote daemon over a Unix socket. The laptop process blocks until ssh exits and propagates the remote exit code.

Other registry commands:

```bash
dot-agent-deck remote list                 # show configured remotes
dot-agent-deck remote upgrade my-vm        # reinstall the binary at the local client's version
dot-agent-deck remote remove my-vm         # forget the registry entry (host untouched)
```

### `remote add` flags

| Flag | Default | Notes |
|---|---|---|
| `--type` | `ssh` | Only `ssh` is implemented today; `kubernetes` is planned in [PRD #81](https://github.com/vfarcic/dot-agent-deck/issues/81). |
| `--port` | `22` | ssh port. |
| `--key` | _none_ | Path to an ssh identity file. Forwarded to ssh as `-i`. Omit to use ssh's default key search. |
| `--version` | client version | Daemon binary version to install on the remote. Usually leave unset. |
| `--no-install` | `false` | Skip the binary push; pre-flight requires `~/.local/bin/dot-agent-deck` on the remote with a matching version. |

Example with a non-default identity file and port:

```bash
dot-agent-deck remote add my-vm deck@198.51.100.10 \
  --key ~/.ssh/dot-agent-deck \
  --port 2222
```

## Lifecycle model

Everything except the terminal lives on the remote: the daemon owns the agent PTYs, the TUI attaches to the daemon over a Unix socket, and `ssh -t` carries terminal bytes between the laptop and the remote shell.

```
laptop                       remote host
+-----------+   ssh -t      +-----------------------------------+
| terminal  | <-----------> | dot-agent-deck TUI                |
| (xterm,   |  stdin/stdout |    ^                              |
|  iTerm…)  |               |    | unix socket (attach proto)   |
+-----------+               |    v                              |
                            | dot-agent-deck daemon             |
                            |    |                              |
                            |    +-- PTY --> agent #1           |
                            |    +-- PTY --> agent #2           |
                            +-----------------------------------+
```

Three properties follow from this shape:

1. **Agents survive your laptop.** Close the lid, lose Wi-Fi, kill the ssh session — the remote TUI process dies with its terminal, but the daemon and agents are separate processes and keep running. Reconnect later from the same laptop or a different one.
2. **Hooks never cross the network.** Agents run on the remote and so does the daemon; hook events travel over a Unix socket on the remote. Network drops do not lose hook events.
3. **One environment per project.** A remote is registered to a single project's working tree on the host. Running multiple projects on one host is supported (one directory per project under `~/projects/`), but each project should still be one registered remote.

## Stop vs detach

Two distinct user actions; very different consequences.

| Action | What happens | When to use |
|---|---|---|
| **Stop** (`Ctrl+W` on a remote pane) | Sends `StopAgent` to the daemon over the local-on-remote socket; the daemon kills the PTY and removes the agent from the registry. | You're done with the agent; want it gone. |
| **Detach** (Ctrl+C in dashboard, then "Detach" in the dialog) | The TUI sends an explicit `KIND_DETACH` frame to the daemon on its Unix socket, then exits. The daemon records a clean detach and keeps the agents running. The ssh session ends when the TUI exits. | You want to step away and come back later, and want the daemon's logs to show a voluntary detach. |
| **Quit** (Ctrl+C in dashboard, then "Quit") | The TUI exits without sending a detach frame. The daemon observes EOF on its socket and treats it the same as detach — agents stay alive. The ssh session ends when the TUI exits. | You're done for the day; don't need the explicit signal. |
| **Sleep / network drop** (no action) | The ssh session dies abruptly; the remote TUI process is killed by its lost controlling terminal, but the daemon is a separate process and keeps running. Agents stay alive. | Implicit; happens automatically when the laptop disconnects. |

The TUI reflects this split in its quit dialog. Pressing `Ctrl+C` in the dashboard opens a three-option prompt:

```
> Quit    — exit without signaling detach
  Detach  — leave remote agents running
  Cancel  — return to dashboard
```

Quit and Detach both leave agents running. The only difference is that Detach sends an explicit signal so the daemon can distinguish a voluntary detach from a network drop in its logs and metrics. For most users either choice does what they want; pick Detach when you want the daemon's record to reflect that you meant to step away.

## Reattaching

Run `connect` again. That re-runs `ssh -t` to the remote, which launches a fresh TUI; the TUI calls `list_agents` on the still-running daemon and hydrates one pane per agent (each `AgentRecord` carries the agent's `display_name` and `cwd`, so the dashboard looks the way you left it). Each pane then attaches to the daemon's per-agent stream, and the daemon replays its scrollback snapshot as the first bytes of the new attach so you can see what was happening while you were gone.

Per-agent scrollback is a 1 MiB ring buffer (`SCROLLBACK_CAP_BYTES` in `src/agent_pty.rs`): if an agent produced more than 1 MiB since you last attached, the oldest bytes are evicted. This is not a feature ceiling — long-running agent transcripts are best read from the agent's own log file, not the deck's scrollback buffer.

## Failure modes

Before exec'ing `ssh -t`, `connect` runs a short version probe (`<install_path> --version` over ssh) so it can classify three failure classes up front and give you an actionable message instead of dropping a half-broken TUI on you. A version mismatch between the remote binary and the laptop client is reported as a warning and the session still proceeds.

### Host unreachable

Symptom: ssh itself failed — refused the connection, timed out, mismatched a host key, or rejected your key. Auth failures fold into this class because the recovery hint is the same.

```
Could not reach remote 'my-vm': <ssh stderr verbatim>
Check your ssh config (`~/.ssh/config`), the host is up, and the network path is open.
```

What to check:

- Is the host on, and is its sshd listening on the configured port?
- Does `ssh user@host` work outside the deck? If not, this isn't a deck problem — it's an ssh/network problem and you'll see the same error.
- Did the host key change? `ssh-keygen -R host` to remove the stale entry, then re-`ssh` to accept the new one.

### Remote binary missing

Symptom: ssh worked, but `dot-agent-deck` wasn't found at `~/.local/bin/dot-agent-deck` on the remote (or what's at that path isn't a real `dot-agent-deck` build).

```
Remote 'my-vm' is reachable but `dot-agent-deck` was not found at ~/.local/bin/dot-agent-deck. Run `dot-agent-deck remote upgrade my-vm` to (re)install.
```

What to do:

- Run `dot-agent-deck remote upgrade my-vm` to reinstall the binary at your local client's version. This re-runs the install flow that `remote add` did originally.
- If the upgrade fails, the install path itself is broken — check that the remote user has write access to `~/.local/bin/`.

### Version mismatch (warning, not an error)

If the remote binary reports a different version than the laptop, `connect` prints a warning and proceeds. The remote TUI runs against its own daemon over a Unix socket, so a version skew doesn't break the session — it just means future protocol changes might land on one side before the other.

```
warning: remote 'my-vm' runs dot-agent-deck 0.30.0; laptop runs 0.24.5. Run `dot-agent-deck remote upgrade my-vm` to align.
```

Run `remote upgrade` at your convenience to bring the remote binary in line with the laptop client.

### Empty dashboard on first connect

Not a failure, but worth calling out: a freshly-added remote has no agents yet, so the first `connect` drops you into an empty dashboard. Press `Ctrl+N` inside the TUI to start your first agent, and it will be there the next time you reconnect.

## Hooks on the remote

Agents emit hook events (delegate, work-done, etc.) by piping JSON to `dot-agent-deck hook`. On the remote, this resolves to the local socket the daemon serves — there is no network round-trip for hooks, and laptop disconnections do not lose events.

`dot-agent-deck hooks install` is run automatically by `remote add` and writes the agent-side hook configuration. If you provision agents on the remote out-of-band (manually editing `~/.claude/settings.json`, for example), run `hooks install` over ssh after the agent is installed so its hook payloads reach the daemon.

## Limitations in v1

- **One transport.** v1 ships ssh only. The daemon protocol is transport-agnostic and a Kubernetes transport is being designed in [PRD #81](https://github.com/vfarcic/dot-agent-deck/issues/81).
- **No multi-user host isolation.** A remote is assumed to be a single user's host. Sharing one host between multiple unrelated users (each with their own credentials) is not supported in v1.
- **No bidirectional file sync.** Project files live on the remote; sync via git. The deck does not bundle mutagen/syncthing/sshfs.

## See also

- [Remote Environment Requirements](remote-requirements.md) — what a host must provide before you can register it.
- [Remote Recipes](remote-recipes.md) — provisioning snippets for common cloud and local-VM hosts.
