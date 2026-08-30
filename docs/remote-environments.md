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
dot-agent-deck remote doctor my-vm         # diagnose ssh, the install, and any forwards (read-only)
dot-agent-deck remote upgrade my-vm        # reinstall the binary at the local client's version
dot-agent-deck remote remove my-vm         # forget the registry entry (host untouched)
```

`remote doctor` is the one to reach for when a connection or a tunnel is not behaving: it runs a fixed list of read-only checks and prints each as PASS / WARN / FAIL / UNKNOWN with the directive and file to change. It never writes anything, anywhere. It exits `0` when the diagnosis is clear, `1` when a check failed, and `2` when a check could not be determined. See [Reverse tunnels › Troubleshooting with `remote doctor`](remote-recipes.md#troubleshooting-with-remote-doctor).

### `remote add` flags

| Flag | Default | Notes |
|---|---|---|
| `--type` | `ssh` | Only `ssh` is implemented today; `kubernetes` is planned — see [issue #81](https://github.com/vfarcic/dot-agent-deck/issues/81). |
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

1. **Agents survive your laptop.** Close the lid, lose Wi-Fi, kill the ssh session — the remote TUI process dies with its terminal, but the daemon and agents are separate processes and keep running. Reconnect later from the same laptop or a different one. A sleep or network drop reconnects on its own without closing the tab (see [Surviving sleep/wake](#surviving-sleepwake)); an explicit `connect` is only needed after you deliberately quit or move to another machine. The one and only thing that stops your remote agents is **you choosing to upgrade-and-restart** the remote daemon — never a detach, sleep, network drop, or machine switch. Upgrading the remote binary swaps the file on disk but leaves the running daemon (and its agents) alone; the daemon is recycled only when you consent to it on the next attach (see [Version skew and the upgrade nudge](#version-skew-and-the-upgrade-nudge)).
2. **Hooks never cross the network.** Agents run on the remote and so does the daemon; hook events travel over a Unix socket on the remote. Network drops do not lose hook events.
3. **One environment per project.** A remote is registered to a single project's working tree on the host. Running multiple projects on one host is supported (one directory per project under `~/projects/`), but each project should still be one registered remote.

## Stop vs detach

Two distinct user actions; very different consequences.

| Action | What happens | When to use |
|---|---|---|
| **Stop** (`Ctrl+W` from command mode, then **Close**) | Stops the agent on the remote and removes its card. If the agent had already exited, the card is removed anyway rather than wedging on an error you can never clear. | You're done with the agent; want it gone. |
| **Detach** (Ctrl+C in dashboard, then "Detach" in the dialog) | The TUI tells the daemon you are leaving deliberately, then exits. The daemon records a clean detach and keeps the agents running. The ssh session ends when the TUI exits. | You want to step away and come back later, and want the daemon's logs to show a voluntary detach. |
| **Quit** (Ctrl+C in dashboard, then "Quit") | The TUI exits without sending a detach frame. The daemon observes EOF on its socket and treats it the same as detach — agents stay alive. The ssh session ends when the TUI exits. | You're done for the day; don't need the explicit signal. |
| **Sleep / network drop** (no action) | SSH keepalive detects the dead connection within ~45s and ssh exits; `connect` then re-probes and **reconnects automatically** to the still-running agents, so the session resumes in place. The daemon and agents never stopped. See [Surviving sleep/wake](#surviving-sleepwake). | Implicit; happens automatically when the laptop sleeps or the network drops. |

The TUI reflects this split in its quit dialog. Pressing `Ctrl+C` in the dashboard opens a three-option prompt:

```
> Quit    — exit without signaling detach
  Detach  — leave remote agents running
  Cancel  — return to dashboard
```

Quit and Detach both leave agents running. The only difference is that Detach sends an explicit signal so the daemon can distinguish a voluntary detach from a network drop in its logs and metrics. For most users either choice does what they want; pick Detach when you want the daemon's record to reflect that you meant to step away.

## Reattaching

Run `connect` again. That re-runs `ssh -t` to the remote and launches a fresh TUI, which asks the still-running daemon what is going on and rebuilds one pane per agent — with the names and working directories you left, so the dashboard looks the way you remember it. Each pane then replays what the agent printed while you were away, so you can see what happened in your absence.

Each agent's replay buffer holds 1 MiB: if an agent produced more than that since you last attached, the oldest output is dropped. This is not a feature ceiling — long-running agent transcripts are best read from the agent's own log file, not the deck's scrollback buffer.

## Surviving sleep/wake

A long-lived `connect` session survives the laptop sleeping or the network dropping out from under it — you don't have to close the tab and start over. Reopen the laptop and the session reconnects to the same running agents on its own. Two mechanisms cooperate:

1. **SSH keepalive detects the dead connection.** When the laptop sleeps, the TCP connection dies silently — the sleeping endpoint never sends a FIN/RST, so on wake ssh is parked on a dead socket it can't tell is dead, and the TUI freezes. To prevent that, the live session probes the remote over the encrypted channel every 15 seconds (`ServerAliveInterval=15`) and aborts after 3 consecutive unanswered probes (`ServerAliveCountMax=3`). So a connection killed by sleep is noticed and torn down within roughly **45 seconds** of wake instead of hanging forever. Probing over the encrypted channel works through NAT and firewalls, unlike TCP-level keepalive.
2. **`connect` reconnects automatically.** When ssh exits because the transport dropped (its exit code 255), `connect` prints `connection to <name> lost — reconnecting…` to stderr, re-runs its version/protocol probe to confirm the host is reachable again, and re-launches the TUI. Because the daemon and agents on the remote never stopped (see [Reattaching](#reattaching)), the fresh session re-attaches to the **same running agents** — you see your session resume, not a blank dashboard.

Reconnection is **bounded**, so a genuinely-gone remote surfaces an error instead of looping forever: `connect` retries up to four times after the initial drop, with a short backoff between attempts. If the host is still unreachable when the budget is exhausted, `connect` prints a clear "giving up" message, restores your local terminal to a sane state (a session interrupted mid-stream may otherwise leave the terminal in raw mode), and exits.

Only a **dropped transport** triggers a reconnect. A clean quit or detach (exit 0), a `Ctrl-C` (exit 130), or a remote-side crash all end the session immediately — `connect` never reconnects into an intentional exit or a crashing TUI, and `last_connected` is recorded only on a clean exit, not on intermediate reconnects.

The keepalive interval/count and retry budget are sensible fixed defaults today; exposing them as configuration is a future improvement.

## Version skew and the upgrade nudge

A difference between the remote's `dot-agent-deck` version and your laptop's never blocks a connect. Your laptop is only ssh plus a terminal — the remote runs both its own TUI and its own daemon and they share one binary on the host — so the laptop's version has no bearing on whether the remote session is correct. A remote you simply haven't upgraded connects exactly as before, with its matched TUI and daemon and all its agents intact.

The one case where `connect` offers to do something is when your **laptop is strictly newer** than the remote. Then, just before handing over to ssh, it shows a single optional prompt:

```
Remote 'my-vm' runs 0.31.0; you have 0.31.1 (2 running agents). Upgrade and connect? [y/N]
```

Its behavior:

- **Newer-only.** The offer appears only when the laptop is ahead of the remote; it never suggests a downgrade or a same-version no-op.
- **Default No.** Pressing **Enter**, `n`, or anything that isn't an explicit `y`/`yes` connects to the remote **as-is**, at its existing version.
- **`y` upgrades, then connects.** It runs `dot-agent-deck remote upgrade my-vm` (a binary swap on the host — see below), then connects.
- **Non-TTY skip.** When stdin is not a terminal (a script, a piped invocation), the prompt is skipped entirely and `connect` proceeds against the existing version, so automation never hangs on it.
- **Upgrade failure falls back.** If the `remote upgrade` step fails, `connect` prints a clear message and connects to the **existing** remote version anyway — you are never left unable to reach your session.

The `(N running agents)` note appears when the count is known, so you can see the restart cost before you say yes.

### What `y` actually does to the remote daemon

`remote upgrade` swaps the binary on disk **only** — it does not touch the running daemon or its agents. The daemon is recycled, if at all, by the same TUI↔daemon handshake that runs locally, on the remote's own machine, when the freshly-installed TUI attaches:

- **No agents running on the remote** — the daemon restarts **silently** onto the new version. You land in the dashboard with nothing lost.
- **Agents running on the remote** — you get the restart prompt (rendered over your ssh session). It **names the live remote agents** and warns that restarting stops them. Press **S** to restart onto the new version (those agents stop), or any other key to **keep the current daemon** and stay attached with your agents intact.

This is the same shared handshake described under [Installation › Upgrading](installation.md#upgrading); the remote case differs only in that the binary swap happened over ssh first. Either way, declining always lands you on a working session — upgrading a remote can never strand you from your running agents.

## Failure modes

Before exec'ing `ssh -t`, `connect` runs a short version probe (`<install_path> --version` over ssh) so it can classify reachability failures up front and give you an actionable message instead of dropping a half-broken TUI on you. A version *difference* between the remote binary and the laptop client is **not** a failure — it never blocks the connect. An un-upgraded older remote connects normally, and when your laptop happens to be newer you get an optional one-step upgrade offer (see [Version skew and the upgrade nudge](#version-skew-and-the-upgrade-nudge)).

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

### SSH forwarding failed

Symptom: ssh reached the host and authenticated, but could not establish a forward your `~/.ssh/config` asked for. This only applies if you have set up a tunnel — see [Reaching networks only your laptop can see](remote-recipes.md#reaching-networks-only-your-laptop-can-see).

```
SSH forwarding failed for remote 'my-vm': remote port forwarding failed for listen port 1080
The remote port may already be bound, or `AllowTcpForwarding` may be disabled on the remote. Run `dot-agent-deck remote doctor my-vm` to distinguish the cause.
```

Both causes produce the *same* bytes from ssh, so the client genuinely cannot tell them apart and the message does not pretend to. `remote doctor` reads the remote's own sshd policy and the live bind state, which is what separates them.

### Remote binary missing

Symptom: ssh worked, but `dot-agent-deck` wasn't found at `~/.local/bin/dot-agent-deck` on the remote (or what's at that path isn't a real `dot-agent-deck` build).

```
Remote 'my-vm' is reachable but `dot-agent-deck` was not found at ~/.local/bin/dot-agent-deck. Run `dot-agent-deck remote upgrade my-vm` to (re)install.
```

What to do:

- Run `dot-agent-deck remote upgrade my-vm` to reinstall the binary at your local client's version. This re-runs the install flow that `remote add` did originally.
- If the upgrade fails, the install path itself is broken — check that the remote user has write access to `~/.local/bin/`.

### Empty dashboard on first connect

Not a failure, but worth calling out: a freshly-added remote has no agents yet, so the first `connect` drops you into an empty dashboard. Press `Ctrl+N` inside the TUI to start your first agent, and it will be there the next time you reconnect.

## Hooks on the remote

Agents emit hook events (delegate, work-done, etc.) by piping JSON to `dot-agent-deck hook`. On the remote, this resolves to the local socket the daemon serves — there is no network round-trip for hooks, and laptop disconnections do not lose events.

`dot-agent-deck hooks install` is run automatically by `remote add` and writes the agent-side hook configuration. If you provision agents on the remote out-of-band (manually editing `~/.claude/settings.json`, for example), run `hooks install` over ssh after the agent is installed so its hook payloads reach the daemon.

## Getting files to the remote

Everything except your terminal runs on the remote (see [Lifecycle model](#lifecycle-model)), so anything you want an agent to read — a screenshot, a log, a PDF — has to exist on the remote's filesystem. Your laptop's filesystem is not visible from over there.

This matters most for images, because two of the three ways Claude Code accepts an image are machine-local by nature:

| Method | Works over ssh? |
|---|---|
| Paste from the clipboard (`Ctrl+V`, or `Cmd+V` in iTerm2 on macOS) | **No** — reads the clipboard of the machine the agent runs on, which is the remote. Your screenshot is on your laptop's clipboard. |
| Drag and drop onto the terminal window | **No**, and it fails misleadingly — see below. |
| A file path — "Analyze this image: /path/to/image.png" | **Yes.** A plain filesystem read, so it crosses the network boundary fine. |

**Drag and drop is a trap here.** The drop target is your terminal emulator, running on your *laptop*, so it inserts a **laptop** path. That path is then meaningless to an agent running on the remote. It looks like it worked right up until the agent reports a file that does not exist.

None of this is specific to the deck. Plain `ssh remote` followed by `claude` behaves identically, and dragging an image into a deck running locally works fine. The deck has no clipboard-read and no image handling — nothing is intercepting your paste; the file simply is not on the machine the agent runs on.

### Copy the file over, then reference its path

Copy the file to the remote first, then give the agent the remote path:

```bash
scp ~/Desktop/screenshot.png my-vm:/tmp/
```

Then, in the agent pane:

```
look at /tmp/screenshot.png and tell me what's wrong
```

If the remote has a `Host` block in your `~/.ssh/config`, that `scp` needs no flags — `connect` does not pass `-F` or otherwise override ssh's configuration, so your ssh config applies to the deck exactly as it does to any other ssh invocation. If you registered the remote with `--port` or `--key`, pass the same values to `scp` (`-P` for the port, `-i` for the key) unless your ssh config already carries them.

It is two steps and a second terminal, which is worse than pasting. It works on any terminal and any OS.

## Limitations in v1

- **One transport.** v1 ships ssh only. A Kubernetes transport is being designed — see [issue #81](https://github.com/vfarcic/dot-agent-deck/issues/81).
- **No multi-user host isolation.** A remote is assumed to be a single user's host. Sharing one host between multiple unrelated users (each with their own credentials) is not supported in v1.
- **No bidirectional file sync.** Project files live on the remote; sync via git. The deck does not bundle mutagen/syncthing/sshfs. Moving a one-off file across — a screenshot you want an agent to look at, say — is covered in [Getting files to the remote](#getting-files-to-the-remote).
- **No laptop-to-remote clipboard or file transfer.** Pasting an image into an agent pane over ssh does not work, and there is no built-in transfer command. Copy the file over with `scp` and reference its path.

## See also

- [Remote Environment Requirements](remote-requirements.md) — what a host must provide before you can register it.
- [Remote Recipes](remote-recipes.md) — provisioning snippets for common cloud and local-VM hosts.
- [Installation › Recycling the local daemon](installation.md#recycling-the-local-daemon) — `dot-agent-deck daemon stop` is the local counterpart for recycling the daemon on your laptop after a binary upgrade. The remote lifecycle described above (per-attach daemon, ssh session governs cleanup) is independent.
