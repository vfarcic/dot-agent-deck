# Remote Environments

A **remote environment** is a host that runs the deck's daemon and your agents, which you reach over ssh. Two words are used throughout this page: **your laptop** is the machine you connect *from*, and the **host** (or **remote**) is the machine the agents run on. That is a split of roles, not of hardware: the host can itself be a laptop.

- **TUI:** `dot-agent-deck connect <name>` opens an ssh session and runs the TUI **on the host**, attached to the host's daemon. Your laptop supplies ssh and a terminal. When you disconnect, the agents on the host keep running.
- **Desktop:** the desktop app opens an ssh tunnel to a daemon that is already running on the host and shows its agents beside your local ones. It installs nothing on the host and starts no daemon there. See [Using a remote from the desktop app](#using-a-remote-from-the-desktop-app).

Before registering a host, check it against [Remote Environment Requirements](remote-requirements.md). [Remote Recipes](remote-recipes.md) has bootstrap commands for a fresh Linux or macOS host and the reverse-tunnel setup. On the host, `~/.local/bin/dot-agent-deck docs <topic>` prints these pages for the version installed there.

## Register a remote

Run this on your laptop:

```bash
dot-agent-deck remote add my-vm deck@198.51.100.10
```

`remote add` does these steps in order and stops at the first failure, printing why (see [Failure modes](#failure-modes)):

1. Checks the name, the ssh target and `--version`, and refuses a name that is already registered.
2. Runs `uname -s -m` on the host over ssh and maps it to a build: `Linux x86_64` → `linux-amd64`, `Linux aarch64`/`arm64` → `linux-arm64`, `Darwin x86_64` → `darwin-amd64`, `Darwin arm64` → `darwin-arm64`. Anything else is refused.
3. Checks how the deck is already installed on the host. If a Homebrew formula owns it, that install is used (see [Hosts where Homebrew installed the deck](#hosts-where-homebrew-installed-the-deck)). Otherwise the host downloads the release binary from GitHub with `curl` into `~/.local/bin/dot-agent-deck`, replacing any file already there.
4. Runs `<binary> --version` on the host and checks it reports the expected version.
5. Runs `<binary> hooks install` on the host, which writes the Claude Code hooks into `~/.claude/settings.json` there. Hooks for other agents are written when the deck starts on the host; see [Hooks on the remote](#hooks-on-the-remote).
6. Adds the entry to your laptop's registry file (see [The registry file](#the-registry-file)).

Every ssh call `remote add` makes runs with `BatchMode=yes`, so ssh cannot prompt. Your key must work without a passphrase prompt (loaded into `ssh-agent`, or unencrypted) and the host's key must already be in `~/.ssh/known_hosts`. Run `ssh deck@198.51.100.10 true` once first to accept the host key.

On success the last line is:

```
Added remote 'my-vm' (ssh: deck@198.51.100.10, version 0.44.0). Run `dot-agent-deck connect my-vm` to attach.
```

Check it:

```bash
dot-agent-deck remote list
```

```
NAME   TYPE  HOST                 VERSION  ADDED_AT
my-vm  ssh   deck@198.51.100.10   0.44.0   5s ago
```

`remote list` reads only the local registry; it does not contact the host. A non-default port shows as `host:port`.

### `remote add` reference

```
dot-agent-deck remote add [OPTIONS] <NAME> <TARGET>
```

| Argument or flag | Default | Meaning |
|---|---|---|
| `<NAME>` | required | The name you type after `connect`. 1 to 64 characters: letters `a`–`z` and `A`–`Z`, digits, `.`, `-` and `_`, starting with a letter or digit. Must not already be registered. |
| `<TARGET>` | required | The ssh target, `[user@]host`. Without a user, ssh's own default (your ssh config, then your local user name) applies. |
| `--port <PORT>` | `22` | ssh port. |
| `--key <KEY>` | none | ssh identity file, passed to ssh as `-i`. Without it, ssh's default key search and your `~/.ssh/config` apply. |
| `--version <VERSION>` | this binary's version | Release to install on the host, `X.Y.Z` or `vX.Y.Z` (an optional `-suffix` is accepted). Usually leave it unset so the host matches your laptop. |
| `--no-install` | off | Download nothing. The host must already have `~/.local/bin/dot-agent-deck` (or a Homebrew install) reporting exactly `--version`, or the command fails with `Installed binary reports ... but expected ...`. |
| `--type <KIND>` | `ssh` | Transport. `ssh` is the only one implemented; `kubernetes` is accepted by the parser and refused with `Remote type 'kubernetes' is not yet implemented`. |

Example with a non-default key and port:

```bash
dot-agent-deck remote add my-vm deck@198.51.100.10 --key ~/.ssh/dot-agent-deck --port 2222
```

Other `remote` subcommands:

| Command | What it does |
|---|---|
| `dot-agent-deck remote list` | Prints the registry as a table. Offline. |
| `dot-agent-deck remote upgrade <NAME> [--version <V>] [--no-install]` | Reinstalls the binary on the host (see [Upgrade a remote](#upgrade-a-remote)). |
| `dot-agent-deck remote doctor <NAME>` | Read-only diagnosis of ssh, the install and reverse tunnels (see [Check a remote's health](#check-a-remotes-health)). |
| `dot-agent-deck remote remove <NAME>` | Removes the entry from the registry. The binary, hooks, daemon and agents on the host are left as they are. |
| `dot-agent-deck connect [NAME]` | Opens the TUI on the host (see [Connect](#connect)). |

## Connect

```bash
dot-agent-deck connect my-vm
```

With no name, `connect` picks for you: with one registered remote it connects to it; with several it prints a numbered list and reads a number (three invalid answers abort).

Before it hands over your terminal, `connect` checks the host in two steps. While each runs, a terminal shows `Connecting to 'my-vm'… checking the remote deck` and then `Connecting to 'my-vm'… waiting for the handshake`; the line is erased when the session starts. Piped or redirected, nothing extra is printed. The checks are:

1. `<binary> --version` over ssh, which must print `dot-agent-deck <version>`. This finds an unreachable host, a missing binary or something else at that path.
2. `<binary> daemon hello` over ssh, which must answer the attach handshake. This finds a binary too old to connect to, or a broken install.

Then it runs `ssh -t` with the host's binary as the remote command. The TUI you see is the host's; it attaches to the host's daemon, starting one if none is running.

`connect` stays in the foreground for the whole session and exits with the remote TUI's exit code. `connect` reads your `~/.ssh/config` and does not pass `-F`, so `Host` blocks, `ProxyJump` and forwards there apply. It does set `ConnectTimeout`, `ServerAliveInterval=15` and `ServerAliveCountMax=3` on the command line, so values for those three in your config have no effect on the session.

`BatchMode` is not set on the interactive session itself, but the two checks above use it, so a first-time host key or a passphrase prompt still fails the connect. Accept the host key with a plain `ssh` first.

### The first connect

A new remote has no agents, so the first `connect` shows an empty dashboard. Press `Ctrl+N` to start one; see [Keyboard Shortcuts](keyboard-shortcuts.md).

On a macOS host, the first Claude Code agent prints `Not logged in · Please run /login`. Run `/login` in that pane once; see [macOS as a remote host](remote-requirements.md#macos-as-a-remote-host).

## Detach, stop, and come back

Agents run as children of the daemon on the host, not of your ssh session. Ending the session in any of these ways leaves them running:

- **Detach**: press `Ctrl+C` on the dashboard and choose **Detach** (the default). The TUI tells the daemon you left on purpose and exits, and the ssh session ends.
- Closing the terminal window, killing `ssh`, your laptop sleeping, or the network dropping. The daemon sees the connection close and keeps the agents.

These stop agents:

- **Stop** in the same `Ctrl+C` dialog: it stops every agent the daemon manages and shuts the daemon down, asking once more first while agents are running.
- Closing one agent: press `Ctrl+D` to reach command mode, then `Ctrl+W`, and choose **Close**. That stops that agent and removes its card.
- `remote upgrade` followed by accepting the restart prompt (see [Upgrade a remote](#upgrade-a-remote)).
- Anything that stops the host: a reboot, a shutdown, or system sleep. After a reboot the agents are gone; the next `connect` recreates the saved workspace (panes, names, directories, commands) and starts each command again, without the agents' conversations (see [Resuming Sessions](session-management.md#resuming-sessions)). [Keep the daemon running](remote-requirements.md#keep-the-daemon-running) covers the daemon itself.

The `Ctrl+C` dialog looks like this:

```
  Quit dot-agent-deck?

  > Detach  — leave agents running on the daemon
    Stop    — shut down agents and daemon
    Cancel  — return to dashboard
```

To come back, run `dot-agent-deck connect my-vm` again, from the same laptop or another one. The new TUI restores your workspace (see [Resuming Sessions](session-management.md#resuming-sessions)): one pane per running agent, with its name and working directory, each replaying what the agent printed while you were away. The replay keeps the most recent 1 MiB of output per agent; anything older is dropped.

Check that the agents are still there without opening the TUI:

```bash
ssh deck@198.51.100.10 '~/.local/bin/dot-agent-deck daemon status'
```

It lists the daemon's agents, or reports that no daemon is reachable, and never starts one.

## Surviving sleep/wake

A `connect` session recovers from your laptop sleeping or the network dropping:

1. ssh probes the host every 15 seconds and ends the session after three unanswered probes, so a dead connection is noticed about 45 seconds after you wake.
2. When ssh exits with status 255 (a transport failure), `connect` prints `connection to 'my-vm' lost — reconnecting… (attempt 2/5)` to stderr, waits 2 seconds, checks the host again and starts a new session, which attaches to the same running agents.
3. It tries up to 5 sessions in total (the first plus 4 reconnects). When they are used up it prints `connection to 'my-vm' lost and could not be re-established after 5 attempts — giving up.`, resets your local terminal and exits with status 255.

The first connect retries in the same way: while the host is not reachable yet, `connect` prints `'my-vm' not reachable yet — retrying… (attempt 2/5)` and tries up to 5 times. The first connect and the reconnects have separate budgets.

Only a transport failure (ssh exit status 255, or a check that cannot reach the host) triggers a retry. Any other exit status ends `connect` immediately, including a Detach (status 0) and a TUI crash on the host; so do a missing binary, a failed handshake and a forward that could not bind. These timings and counts are fixed.

This covers **your laptop** sleeping. A host that sleeps suspends its agents; on a Mac host, prevent that with `pmset` (see [macOS as a remote host](remote-requirements.md#macos-as-a-remote-host)).

## Upgrade a remote

```bash
dot-agent-deck remote upgrade my-vm
```

`remote upgrade` repeats the install from `remote add` (arch check, install detection, download, version check, `hooks install`) and records the new version and an `upgraded_at` time in the registry. It prints `Upgraded remote 'my-vm' to version 0.44.0.` on success.

| Flag | Default | Meaning |
|---|---|---|
| `--version <VERSION>` | this binary's version | Release to install. Use it to move a host to an older release as well as a newer one. |
| `--no-install` | off | Download nothing; only check that the host's binary reports `--version` and update the registry. Use it after replacing the binary on the host yourself. |

`remote upgrade` replaces the file on disk. It does not touch the running daemon or its agents. The daemon changes version the next time you `connect`, when the new TUI on the host finds a daemon from a different build:

- **No agents running**: the TUI restarts the daemon onto the new version without asking.
- **Agents running**: the TUI shows a prompt naming the running agents. Press `S` to restart the daemon (the listed agents stop). Any other key keeps the current daemon.
- If the new release also changed the attach protocol, the TUI cannot attach to the old daemon at all. The prompt then says `[any other key] exit, leaving the daemon running`, and declining ends the session with the agents still running under the old daemon. To reach them, reinstall the old release with `dot-agent-deck remote upgrade my-vm --version <old-version>` and connect again. When you are ready to lose them, connect and press `S`.

### The upgrade offer on connect

When your laptop's version is newer than the version the host reports, and stdin is a terminal, `connect` asks once before starting the session:

```
Remote 'my-vm' runs 0.43.0; you have 0.44.0 (2 running agents). Upgrade and connect? [y/N]
```

- `y` or `yes` runs `remote upgrade my-vm` to your laptop's version, then connects. The restart rules above then apply.
- Anything else, including `Enter`, connects to the host's current version.
- The agent count appears when it is known.
- If the upgrade fails, `connect` prints `warning: upgrade of remote 'my-vm' failed: …` and `Connecting to the existing 0.43.0 install instead.`, and connects anyway.
- No offer is made when the host is the same or newer, or when stdin is not a terminal. A version difference never blocks `connect`: the host runs its own TUI and daemon, so your laptop's version does not affect the session.

### Hosts where Homebrew installed the deck

`remote add` and `remote upgrade` first look for a Homebrew install on the host: `brew list --formula dot-agent-deck` succeeding with `brew` found on the host's `PATH` or under `/opt/homebrew`, `/usr/local` or `/home/linuxbrew/.linuxbrew`. When one exists:

- Nothing is downloaded to `~/.local/bin`. The registry records the install method as `homebrew` and the binary as `<prefix>/bin/dot-agent-deck`, and `connect`, `remote doctor` and `hooks install` run that binary.
- `remote add` registers the version the Homebrew install reports. If it differs from `--version`, it says so.
- `remote upgrade` runs `brew upgrade dot-agent-deck` on the host. Homebrew installs its tap's latest release and cannot install a chosen one, so `--version` is not honoured; the command records the version that landed and says so if it differs.
- With `--no-install`, the Homebrew install must report exactly `--version`.

If the host has both a Homebrew install and `~/.local/bin/dot-agent-deck`, the command uses the Homebrew one, leaves the other file alone, and prints the `ssh … 'rm ~/.local/bin/dot-agent-deck'` command to remove it. Remove it, because an older `dot-agent-deck` client runs that path on `connect`.

An entry without a recorded install method runs `~/.local/bin/dot-agent-deck` until the next `remote upgrade`, which detects and records the install. An older `dot-agent-deck` client always runs `~/.local/bin/dot-agent-deck`, so it cannot `connect` to a host whose only install is Homebrew's.

## Check a remote's health

```bash
dot-agent-deck remote doctor my-vm
```

`remote doctor` runs ten read-only checks over ssh and prints one line per check, `PASS`, `WARN`, `FAIL` or `UNKNOWN`, with the fix under any that is not `PASS`. For whether the remote works at all, read the first three:

| Check | `PASS` means | If it fails |
|---|---|---|
| `HostReachable` | ssh connected and authenticated. | Fix ssh first: `ssh <target> true` must work without a prompt. |
| `RemoteBinary` | The deck answered `--version` on the host. | `dot-agent-deck remote upgrade my-vm`. |
| `ProtocolCompatible` | The host's binary answered the attach handshake. | `dot-agent-deck remote upgrade my-vm`; `connect` refuses the host until this passes. |

The other seven checks diagnose a **reverse tunnel** in your ssh config. On a remote you use without one, expect `RemoteForward` to report `FAIL` ("ssh resolved no reverse tunnel for this destination"), `ExitOnForwardFailure` `WARN` and `ForwardBound` `UNKNOWN`, which makes the overall result `FAIL` and the exit status `1` even though `connect` works. Those three lines are only relevant if you set up a tunnel; [Troubleshooting with `remote doctor`](remote-recipes.md#troubleshooting-with-remote-doctor) explains every check.

Exit status: `0` when every check is `PASS` or `WARN`; `1` when any check is `FAIL`, or the command could not run (unknown name, unreadable registry); `2` when nothing failed but at least one check is `UNKNOWN`.

## Failure modes

`remote add`, `remote upgrade` and `connect` print one of these messages and exit non-zero. [Troubleshooting](troubleshooting.md#a-remote-will-not-connect-or-an-ssh-tunnel-to-it-is-not-working) has the same list from the symptom side.

### Host unreachable

From `connect`:

```
Could not reach remote 'my-vm': <ssh's own error>
Check your ssh config (`~/.ssh/config`), the host is up, and the network path is open.
```

From `remote add` and `remote upgrade`, the same class of problem starts with `Could not reach <host>:<port>.`, `ssh authentication to <target> failed.` or `ssh failed: host key not yet trusted for <target>.`, and quotes ssh's own error. These two commands do not retry.

`connect` retries this class (see [Surviving sleep/wake](#surviving-sleepwake)), so one or more `not reachable yet — retrying…` lines above it are normal. Host-key and authentication failures are in this class for `connect` and are retried too.

What to do:

- Run `ssh <user>@<host> true` (with `-p` and `-i` if you registered them). If that fails, the problem is ssh or the network, not the deck.
- A host key that is not yet known: run that `ssh` once and accept the key. A host key that changed: find out why before running `ssh-keygen -R <host>` and reconnecting.
- A key with a passphrase: add it to `ssh-agent` (`ssh-add <key>`).
- A slow host (a VM starting up, a VPN coming up): raise the probe budget, for example `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS=30 dot-agent-deck connect my-vm`. It is in seconds, defaults to `10`, and is clamped to 1–3600.

### SSH forwarding failed

```
SSH forwarding failed for remote 'my-vm': remote port forwarding failed for listen port 1080
The remote port may already be bound, or `AllowTcpForwarding` may be disabled on the remote. Run `dot-agent-deck remote doctor my-vm` to distinguish the cause.
```

ssh reached the host and authenticated, but could not set up a `RemoteForward` your `~/.ssh/config` asks for, with `ExitOnForwardFailure yes` set. This only happens with a tunnel configured; see [Reaching networks only your laptop can see](remote-recipes.md#reaching-networks-only-your-laptop-can-see). Run `remote doctor`: its `AllowTcpForwarding` and `ForwardBound` checks tell a disabled sshd setting from a port already in use. Not retried.

### Remote binary missing

```
Remote 'my-vm' is reachable but `dot-agent-deck` was not found at ~/.local/bin/dot-agent-deck. Run `dot-agent-deck remote upgrade my-vm` to (re)install.
```

Nothing at the recorded path printed `dot-agent-deck <version>`. Run `dot-agent-deck remote upgrade my-vm`. If that fails with `Failed to download dot-agent-deck …`, see that message in the [table below](#remote-add-and-remote-upgrade-errors). Not retried.

### Handshake failed

```
Remote 'my-vm' did not answer the `daemon hello` handshake, so its `dot-agent-deck` is too old or the install is broken. Run `dot-agent-deck remote upgrade my-vm` to reinstall.
```

Run the suggested `remote upgrade`. A different message, `Remote 'my-vm' rejected the protocol handshake…`, means the host's deck answered with an error; look at the host's log (see [Enabling Debug Logs](troubleshooting.md#enabling-debug-logs)) and its free disk space before retrying.

### `remote add` and `remote upgrade` errors

| Message starts with | Cause | Fix |
|---|---|---|
| `A remote named '<name>' already exists` | The name is taken. | Pick another name, or `remote remove <name>` first. |
| `Invalid remote name` | The name breaks the rules in [the reference](#remote-add-reference). | Use letters, digits, `.`, `-`, `_`, starting with a letter or digit. |
| `Invalid remote address` | The target, port or key path was refused (for example a host starting with `-`). | Pass a plain `[user@]host`. |
| `Invalid --version` | Not `X.Y.Z` or `vX.Y.Z`. | Fix the version. |
| `No remote named '<name>'` | `remote upgrade`, `remote remove` or `connect` got an unregistered name. | `dot-agent-deck remote list`. |
| `Remote arch is …; supported: linux-{amd64,arm64}, darwin-{amd64,arm64}.` | The host is another OS or architecture. | Use a supported host. |
| `Failed to detect remote arch` | `uname -s -m` failed on the host. | Check the host's shell. |
| `Failed to download dot-agent-deck v<version> for <platform> from <url>.` | The host has no outbound HTTPS to GitHub, no `curl`, the release does not exist, or `~/.local/bin` is not writable. | Fix egress, install `curl`, or check the version. |
| `Installed binary reports … but expected …` | The binary on the host is another version (common with `--no-install`). | Drop `--no-install`, or pass the version the host has. |
| `` `dot-agent-deck hooks install` on remote failed`` | Writing `~/.claude/settings.json` on the host failed. | Read the error it quotes; it is often a malformed existing `settings.json`. |
| `` `brew upgrade dot-agent-deck` on remote failed`` | Homebrew on the host failed. | Run `brew upgrade dot-agent-deck` on the host and read its output. |
| `Could not tell how dot-agent-deck is installed on the remote` | The install check printed something unexpected. | Check that the host's login shell starts cleanly over non-interactive ssh (`ssh <target> true` prints nothing). |

## Hooks on the remote

Agents report their status and orchestration events to the daemon on the same host, over a local socket. Nothing crosses the network, so a dropped connection cannot lose them.

- `remote add` and `remote upgrade` run `hooks install`, which covers Claude Code.
- Every time the deck starts on the host (each `connect`, or `daemon serve`), it writes hooks or plugins for each of Claude Code, OpenCode, Codex and Devin that it detects on the host (Claude Code and OpenCode by their configuration directory, Codex and Devin by their command on `PATH`). Pi needs no hook: its extension is set up when a Pi agent starts.

If you install an agent on the host while connected, reconnect, or install its hooks on the host directly:

```bash
ssh deck@198.51.100.10 '~/.local/bin/dot-agent-deck hooks install --agent opencode'
```

`--agent` takes `claude-code` (the default), `opencode`, `codex` or `devin`. If a card never leaves its first status, see [Hooks](troubleshooting.md#hooks).

## Getting files to the remote

Agents read files on the host. A file on your laptop is not visible to them, which matters most for images:

| Method | Works over `connect`? |
|---|---|
| Paste from the clipboard (`Ctrl+V`, or `Cmd+V` in iTerm2) | No. The agent reads the clipboard of the host, not your laptop. |
| Drag and drop onto the terminal | No. Your terminal inserts a path on your laptop, which does not exist on the host; the agent reports a missing file. |
| A path on the host, for example "look at /tmp/screenshot.png" | Yes. |

Copy the file over, then give the agent the host path:

```bash
scp ~/Desktop/screenshot.png deck@198.51.100.10:/tmp/
```

```
look at /tmp/screenshot.png and tell me what's wrong
```

If you registered the remote with `--port` or `--key`, pass the same values to `scp` (`-P <port>`, `-i <key>`) unless a `Host` block in `~/.ssh/config` already sets them.

## Using a remote from the desktop app

The desktop app shows a remote daemon's agents alongside the local ones, reached through an ssh tunnel from your laptop. It needs the deck installed on the host and a daemon already running there:

1. Install the deck on the host, most simply with `dot-agent-deck remote add my-vm deck@198.51.100.10` from a machine with the CLI.
2. Keep a daemon running on the host. A daemon with no agents exits about 30 seconds after its last client disconnects, so either leave agents running under it (start them with `connect` and detach), or run a daemon with the idle shutdown off, as [Keep the daemon running](remote-requirements.md#keep-the-daemon-running) shows.
3. In the app, open **Settings → Daemons**, press **Add a daemon**, fill in **Host** (and **User**, **Port**, **Key file** or **Jump host** as needed), check the suggested **Deck name**, and press **Add this daemon**. Then press **Test connection**, which also finds the daemon's socket.
4. Pick the daemon in the **Daemon** selector on the Dashboard, or **All daemons**.

![Settings → Daemons with a remote daemon named build, its address build-box beside the name, chosen in the Daemon row beside All daemons and This machine; below it the Deck name field reads build with a Rename button, Host is filled in, the other fields show their placeholders, and Test connection is not yet pressed](/img/settings-daemons-desktop.png)

[Daemons](desktop/daemons.md) explains the fields and every **Test connection** result. The app runs ssh non-interactively too, so the same key and host-key rules as `remote add` apply.

### One list of remotes for both clients

The CLI and the desktop app read and write the same registry file. A remote added with `remote add` appears in the app, and a daemon added in the app appears in `remote list` and opens with `connect <name>`. Each client edits one entry at a time under a lock, so an edit from one does not undo the other's. Removing a daemon in the app is the same as `remote remove`: the host is not touched.

- The app shows each daemon by its name, the one `connect <name>` takes. When you add a daemon it suggests the host, lowercased, with characters a name cannot hold replaced by `-` (for example `build.example.com`); if that is taken, `<user>-<host>`, then `<host>-2`, `<host>-3` and so on. Keep it or type your own. A daemon can be renamed later in **Settings → Daemons**, and `connect` takes the new name straight away.
- `remote list` shows `unmanaged` as the version of a daemon added in the app, because the CLI did not install its binary. `dot-agent-deck remote upgrade <name>` installs and manages it from then on.
- `connect` does not use a **Jump host** set in the app. For a host reachable only through a bastion, add a `ProxyJump` line for it to `~/.ssh/config`; `connect` runs your system `ssh`, which reads it.
- Changing a daemon's **Host**, **User** or **Port** in the app forgets the recorded install method and binary path, so `connect` runs `~/.local/bin/dot-agent-deck` until the next `remote upgrade`. Changing **Key file** or **Jump host** keeps them. The CLI has no command that edits an entry.

Keep the CLI and the desktop app on the same release on each machine. A CLI older than the shared list rewrites the whole file when it changes it (`remote add`, `remote remove`, `remote upgrade`, or `connect` when a session ends cleanly) and drops the fields it does not know: the app may fall back to the local daemon, jump hosts need entering again, and **Test connection** needs running again. A desktop app older than the shared list shows no remote daemons once a newer one has moved them into it.

## The registry file

Remotes are stored on your laptop in `~/.config/dot-agent-deck/remotes.toml` (on Windows, `%APPDATA%\dot-agent-deck\remotes.toml`). Set `DOT_AGENT_DECK_REMOTES` to a file path to use a different file. Change it with the commands above rather than by hand. A file that does not parse makes every `remote` command and `connect` fail with `Failed to parse remotes file at …`.

```toml
[[remotes]]
name = "my-vm"
type = "ssh"
host = "deck@198.51.100.10"
port = 22
key = "/home/me/.ssh/dot-agent-deck"
version = "0.44.0"
added_at = "2026-09-29T10:00:00.000000000+00:00"
install = "local-bin"
```

| Key | Meaning |
|---|---|
| `name` | The remote's name. |
| `type` | `ssh`. |
| `host` | The ssh target as given to `remote add`, `[user@]host`. |
| `port` | ssh port. |
| `key` | Identity file, if one was given. |
| `version` | The version installed by the last `remote add` or `remote upgrade`, or `unmanaged` for an entry the desktop app added. |
| `added_at`, `upgraded_at`, `last_connected` | RFC 3339 timestamps. `last_connected` is set when a `connect` session ends with status 0. |
| `install` | `local-bin` or `homebrew`; absent on entries made before it existed. |
| `binary` | The binary's absolute path on the host, when it is not `~/.local/bin/dot-agent-deck`. |
| `id`, `user`, `jump_host`, `socket` | Written by the desktop app: its stable id, a login name that overrides the one in `host`, a `ProxyJump` host, and the daemon's socket path on the host. |

## Limitations

- **ssh only.** There is no other transport.
- **One user per host.** A remote is treated as one user's machine: agents on it share that account's files and credentials. Use one host (or one account) per project whose credentials should stay separate.
- **No file sync.** Project files live on the host; move changes with git. Copy one-off files with `scp` ([Getting files to the remote](#getting-files-to-the-remote)).
- **No clipboard or file transfer** between your laptop and an agent pane.
