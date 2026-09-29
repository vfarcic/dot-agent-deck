# Daemons

The desktop app can watch several daemons at once: the one on this machine and any number of remote ones reached over ssh. The TUI attaches to one daemon at a time, and reaches a remote one with `dot-agent-deck connect` (see [Remote Environments](../remote-environments.md)).

## The daemon selector

The **Daemon** selector under the Dashboard's title chooses what the Dashboard shows:

- **All daemons** — every daemon at once, one section each. The **DAEMONS** counter then says how many of them answered.
- **This machine** — the daemon on this computer. The app finds it at the same default address the TUI uses; if you point the TUI at another socket with `DOT_AGENT_DECK_ATTACH_SOCKET`, launch the app with the same variable.
- One entry per remote daemon added in Settings, named after its user, host and port.

The selector's menu only chooses. Daemons are added and removed in **Settings → Daemons**.

## Settings → Daemons

Open **Settings** in the rail and choose **Daemons**. The **Daemon** row lists the same choices as the selector; choose one to see or change its settings, or press **Add a daemon** to add a remote one. The trash icon beside a remote daemon removes it from the app; it does nothing on the host.

![Settings → Daemons with a remote daemon, build-box, chosen in the Daemon row beside All daemons and This machine, its Host filled in, the other fields showing their placeholders, and Test connection below, not yet pressed](/img/settings-daemons-desktop.png)

A remote daemon has these fields:

| Field | What it is |
| --- | --- |
| **Host** | The host name or address to ssh to. |
| **User** | The ssh user. Leave it empty to take it from your ssh config. |
| **Port** | The ssh port, `22` by default. |
| **Key file** | The ssh identity file, for example `~/.ssh/id_ed25519`. Leave it empty for ssh's defaults. |
| **Jump host** | A bastion to go through, as ssh's `ProxyJump` takes it. |
| **Daemon socket** | The path of the daemon's socket on the host. Leave it empty: **Test connection** finds it and fills it in. |

The app reaches a remote daemon through an ssh tunnel, using the `ssh` program on this computer and your ssh config. It runs ssh non-interactively, so ssh to the host has to work without a password prompt, and the host's key has to be in your `known_hosts` already.

## Test connection

**Test connection** checks the chosen daemon and says what it found, in one sentence, with a command to run when there is one. The results include:

| Result | What to do |
| --- | --- |
| `<daemon> answered and is compatible with this app.` | Nothing: the daemon is ready. |
| `The ssh connection to <daemon> works, but nothing is listening on its daemon socket over there.` | Start a daemon on the host (below), then test again. |
| `This machine has not verified <daemon>'s host key.` | Run the `ssh` command shown under it once in a terminal, then test again. |
| `ssh reached <daemon> and the login was refused.` | Check **User** and **Key file**. |
| `ssh could not reach <daemon>.` | Check **Host**, **Port** and your network. |
| `<daemon> has no daemon socket path yet, and this test could not discover one.` | Check that `dot-agent-deck` is installed on the host, in `~/.local/bin` or on its `PATH`, or fill in **Daemon socket**. |
| `<daemon> speaks a different protocol version.` | Run the same release on the host as the app. |
| `No ssh program was found on this machine…` | Install an OpenSSH client. |

A test also lists anything your ssh config adds to the tunnel (for example port forwards) and where host keys are checked, because the tunnel carries those for as long as it is open.

**All daemons** has nothing to test; choose one daemon first.

## What a remote daemon must already have

The desktop app does not install anything on a remote host and does not start a daemon there. Before adding a host:

1. **Install the deck on the host.** The simplest way is from a machine with the CLI: `dot-agent-deck remote add <name> <user@host>` installs `dot-agent-deck` into `~/.local/bin` on the host and sets up the agent hooks. [Remote Environment Requirements](../remote-requirements.md) and [Remote Recipes](../remote-recipes.md) cover what the host needs.
2. **Have a daemon running on it.** A daemon exits about 30 seconds after its last client disconnects when it has no agents and no enabled schedules, so a daemon started and left alone does not stay up. Either keep agents running on it (start them with `dot-agent-deck connect <name>` and detach, or from the desktop app while the daemon is up), or run it with the idle shutdown turned off, for example on the host:

   ```bash
   DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0 nohup ~/.local/bin/dot-agent-deck daemon serve >/dev/null 2>&1 &
   ```

   To have it come back after a logout, a crash or a reboot, run it under `systemd --user` or a macOS LaunchAgent as [Remote Environment Requirements](../remote-requirements.md#recommended-for-persistent-and-safe-use) describes.

While the desktop app is connected to a daemon, that connection counts as a client, so the daemon does not idle out under it.
