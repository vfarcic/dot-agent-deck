---
sidebar_position: 7.4
title: Remote Environments
---

# Remote Environments

A **remote environment** is a per-project Linux host that runs the deck daemon and owns the project's agents. The TUI on your laptop is a viewer — it attaches to the daemon over ssh, streams the agent terminals, and detaches when you close the lid. The agents keep running.

This page covers how that works in practice: the lifecycle model, the difference between "stop" and "detach", the failure modes you'll see when a connect goes wrong, and how hooks behave on the remote.

For host prerequisites see [Remote Environment Requirements](remote-requirements.md). For provisioning recipes see [Remote Recipes](remote-recipes.md).

## Quick start

```bash
# 1. Register a remote (one-time per host).
dot-agent-deck remote add my-vm user@host

# 2. Connect. With no name, an interactive picker opens.
dot-agent-deck connect my-vm
```

`remote add` ssh's into the host, installs the daemon binary into `~/.local/bin/dot-agent-deck`, runs `hooks install`, and writes a registry entry to `~/.config/dot-agent-deck/remotes.toml`. `connect` opens an ssh bridge to the daemon and brings up the local TUI attached to it.

Other registry commands:

```bash
dot-agent-deck remote list                 # show configured remotes
dot-agent-deck remote upgrade my-vm        # reinstall the binary at the local client's version
dot-agent-deck remote remove my-vm         # forget the registry entry (host untouched)
```

### `remote add` flags

| Flag | Default | Notes |
|---|---|---|
| `--type` | `ssh` | Only `ssh` is implemented today; `kubernetes` is planned in [PRD #80](https://github.com/vfarcic/dot-agent-deck/issues/80). |
| `--port` | `22` | ssh port. |
| `--key` | _none_ | Path to an ssh identity file. Forwarded to ssh as `-i`. Omit to use ssh's default key search. |
| `--version` | client version | Daemon binary version to install on the remote. Usually leave unset. |
| `--no-install` | `false` | Skip the binary push; pre-flight requires the remote to already have a matching `dot-agent-deck` on `PATH`. |

Example with a non-default identity file and port:

```bash
dot-agent-deck remote add my-vm deck@198.51.100.10 \
  --key ~/.ssh/dot-agent-deck \
  --port 2222
```

## Lifecycle model

The daemon lives on the remote and owns every agent process. Your laptop runs a viewer that attaches to the daemon, streams output back, and forwards keystrokes forward.

```
laptop                            remote host
+-----------+        ssh         +-------------------+
| TUI       | <----------------> | dot-agent-deck    |
| (viewer)  |  attach protocol   | daemon            |
+-----------+                    |                   |
                                 |  +-------------+  |
                                 |  | agent #1    |  |
                                 |  | (PTY)       |  |
                                 |  +-------------+  |
                                 |  | agent #2    |  |
                                 |  +-------------+  |
                                 +-------------------+
```

Three properties follow from this shape:

1. **Agents survive your laptop.** Close the lid, lose Wi-Fi, hit Ctrl+C in the TUI — the daemon keeps running and the agents keep running with it. Reattach later from the same laptop or a different one.
2. **Hooks never cross the network.** Agents run on the remote and so does the daemon; hook events travel over a Unix socket on the remote. Network drops do not lose hook events.
3. **One environment per project.** A remote is registered to a single project's working tree on the host. Running multiple projects on one host is supported (one directory per project under `~/projects/`), but each project should still be one registered remote.

## Stop vs detach

Two distinct user actions; very different consequences.

| Action | What it does to the remote agent | When to use |
|---|---|---|
| **Stop** (`Ctrl+W` on a remote pane) | Sends `StopAgent` to the daemon; the daemon kills the PTY and removes it from the registry. | You're done with the agent; want it gone. |
| **Detach** (Ctrl+C in dashboard, then "Detach" in the dialog) | Emits an explicit detach frame, then drops the connection. The daemon keeps the agent running. | You want to step away and come back later. |
| **Quit** (Ctrl+C in dashboard, then "Quit") | Exits the TUI without sending a detach frame. The daemon observes EOF and treats it the same as detach — the agent stays alive. | You're done for the day; don't need the explicit signal. |
| **Sleep / network drop** (no action) | The daemon observes EOF on the ssh connection. Treated identically to Quit: agent stays alive. | Implicit; happens automatically when the laptop disconnects. |

The TUI reflects this split in its quit dialog. Pressing `Ctrl+C` in the dashboard opens a three-option prompt:

```
> Quit    — exit without signaling detach
  Detach  — leave remote agents running
  Cancel  — return to dashboard
```

Quit and Detach both leave agents running. The only difference is that Detach sends an explicit signal so the daemon can distinguish a voluntary detach from a network drop in its logs and metrics. For most users either choice does what they want; pick Detach when you want the daemon's record to reflect that you meant to step away.

## Reattaching

Run `connect` again — the daemon's scrollback for each agent is replayed as the first frames of the new attach. Up to 1 MiB of recent output is preserved per agent across detach/reattach cycles, so you can see what the agent was doing while you were gone.

If the agent has produced more than 1 MiB since you last attached, the oldest output is trimmed. This is not a feature ceiling — long-running agent transcripts are best read from the agent's own log file, not the deck's scrollback buffer.

## Failure modes

`dot-agent-deck connect` distinguishes three failure classes and points you at the relevant fix for each.

### Host unreachable

Symptom: ssh itself failed — refused connection, timed out, host key mismatch, or rejected your key.

```
error: connect to remote 'my-vm' (user@host) — host unreachable
       <ssh stderr verbatim>
       check network connectivity, ssh config, or known_hosts
```

What to check:

- Is the host on, and is its sshd listening on the configured port?
- Does `ssh user@host` work outside the deck? If not, this isn't a deck problem — it's an ssh/network problem and you'll see the same error.
- Did the host key change? `ssh-keygen -R host` to remove the stale entry, then re-`ssh` to accept the new one.

### Daemon unavailable

Symptom: ssh worked, but the daemon binary isn't on the host, isn't executable, or is too old to speak the current protocol.

```
error: connect to remote 'my-vm' — daemon unavailable
       <stderr verbatim>
       run: dot-agent-deck remote upgrade my-vm
```

What to do:

- Run `dot-agent-deck remote upgrade my-vm` to reinstall the binary at your local client's version. This re-runs the install flow that `remote add` did originally.
- If the upgrade fails, the install path itself is broken — check that `~/.local/bin` is on the remote user's `$PATH`, and that the user has write access to `~/.local/bin`.

### No agents running

Symptom: the connect succeeded, but the daemon's agent registry is empty. The TUI prints a one-line hint before launching:

```
No agents running on 'my-vm'. Press Ctrl+N inside the TUI to start one.
```

This isn't an error — it's the normal first-time-after-`remote add` state. Press Ctrl+N inside the TUI to start your first agent, and it will be there next time you reattach.

## Hooks on the remote

Agents emit hook events (delegate, work-done, etc.) by piping JSON to `dot-agent-deck hook`. On the remote, this resolves to the local socket the daemon serves — there is no network round-trip for hooks, and laptop disconnections do not lose events.

`dot-agent-deck hooks install` is run automatically by `remote add` and writes the agent-side hook configuration. If you provision agents on the remote out-of-band (manually editing `~/.claude/settings.json`, for example), run `hooks install` over ssh after the agent is installed so its hook payloads reach the daemon.

## Limitations in v1

- **One transport.** v1 ships ssh only. The daemon protocol is transport-agnostic and a Kubernetes transport is being designed in [PRD #80](https://github.com/vfarcic/dot-agent-deck/issues/80).
- **No multi-user host isolation.** A remote is assumed to be a single user's host. Sharing one host between multiple unrelated users (each with their own credentials) is not supported in v1.
- **No bidirectional file sync.** Project files live on the remote; sync via git. The deck does not bundle mutagen/syncthing/sshfs.

## See also

- [Remote Environment Requirements](remote-requirements.md) — what a host must provide before you can register it.
- [Remote Recipes](remote-recipes.md) — provisioning snippets for common cloud and local-VM hosts.
