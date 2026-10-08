# Remote Environment Requirements

What a host must provide before `dot-agent-deck remote add` can register it, and what to set up so the deck keeps running there. The **host** is the machine that runs the daemon and the agents; **your laptop** is the machine you connect from. For registering and connecting see [Remote Environments](remote-environments.md); for bootstrap commands see [Remote Recipes](remote-recipes.md).

## Checklist

Everything under [Required](#required) must hold before `remote add` succeeds and an agent can run. [Recommended for persistent and safe use](#recommended-for-persistent-and-safe-use) is what keeps the daemon running across disconnects and reboots and limits what agents can reach.

| Requirement | How to check it from your laptop |
|---|---|
| Linux or macOS, amd64 or arm64 | `ssh <target> uname -s -m` prints `Linux x86_64`, `Linux aarch64`, `Linux arm64`, `Darwin x86_64` or `Darwin arm64`. |
| Non-interactive ssh with a key | `ssh -o BatchMode=yes <target> true` exits 0 and prints nothing. |
| `curl` and outbound HTTPS to GitHub (unless Homebrew installed the deck) | `ssh <target> 'curl -fsSI https://github.com'` exits 0. |
| A writable `~/.local/bin` | `ssh <target> 'mkdir -p ~/.local/bin && test -w ~/.local/bin'` exits 0. |
| The agent CLI on the login `PATH`, with its credentials | `ssh <target> '$SHELL -ilc "command -v claude"'` prints a path (substitute your agent's command). |

## Required

### Operating system and architecture

| Host | Supported | Build `remote add` installs |
|---|---|---|
| Linux on x86_64 | Yes | `linux-amd64` |
| Linux on aarch64 / arm64 | Yes | `linux-arm64` |
| macOS on Apple Silicon | Yes; see [macOS as a remote host](#macos-as-a-remote-host) | `darwin-arm64` |
| macOS on Intel | Installs, but has not been exercised end to end | `darwin-amd64` |
| Windows | No. `remote add` refuses it (`Remote arch is …`), and there is no Windows daemon build. Windows works as the laptop. | none |

#### Which Linux distribution

To identify the host, `remote add` reads only `uname -s -m`; it does not check the distribution. Two things do vary:

- **glibc.** The Linux release binaries are glibc builds; there is no musl build. On a musl distribution such as Alpine, install the deck on the host from source or with [Nix](installation.md#nix), put the binary (or a symlink to it) at `~/.local/bin/dot-agent-deck`, and register with `--no-install --version <the version it reports>`.
- **systemd.** Only needed for the optional [systemd user service](#keep-the-daemon-running). Without systemd, and without `XDG_RUNTIME_DIR` set, the daemon's sockets go in the temp directory ([Where the deck puts its files](#where-the-deck-puts-its-files)).

### ssh access

- An OpenSSH server reachable from your laptop (port 22 unless you pass `--port`). The daemon opens no network port of its own.
- **Key-based login that needs no prompt.** `remote add`, `remote upgrade`, `remote doctor` and the checks `connect` makes before a session all run ssh with `BatchMode=yes`. A key with a passphrase must be loaded into `ssh-agent` on your laptop.
- **The host key already in your `~/.ssh/known_hosts`.** Run `ssh <target> true` once and accept the key before `remote add`.
- **A login shell that prints nothing** on a non-interactive ssh command. `remote add` parses the output of commands it runs over ssh; a shell startup file that echoes text can make it fail with `Could not tell how dot-agent-deck is installed on the remote`.

### Software on the host

- A POSIX shell and the standard tools `uname`, `mkdir`, `chmod` and `mv`.
- **`curl`**, used by `remote add` and `remote upgrade` to download the release binary. Not needed when Homebrew installed the deck, or with `--no-install`.
- **`git`**, for the project you clone there. The deck itself does not need it to start.
- **The agent CLI** for each agent you will run, the runtime it needs, and its credentials, all available to the account the daemon runs as. The deck starts agents but does not install them. Claude Code, OpenCode, Pi, Codex and Devin have status reporting in the deck; other interactive terminal programs can run in a pane without it.

  | Agent | Install (one option) |
  |---|---|
  | Claude Code | `npm install -g @anthropic-ai/claude-code` (needs Node.js) |
  | OpenCode | `npm install -g opencode-ai` (needs Node.js) |
  | Pi | `npm install -g @earendil-works/pi-coding-agent` (needs Node.js) |
  | Codex | `npm install -g @openai/codex` (needs Node.js) |
  | Devin | the vendor's install instructions; `devin` must end up on `PATH` |

  The daemon reads `PATH` from your login shell when it starts (it runs `$SHELL` as an interactive login shell), so a directory added to `PATH` in `~/.bashrc` or `~/.zshrc` is found. If an agent still fails to start with a bare command, see [A bare command fails to spawn](troubleshooting.md#a-bare-command-like-claude-opencode-pi-codex-or-devin-fails-to-spawn).

- Optional: **`bash`, `timeout`, `head` and `od`**, used by `remote doctor`'s `ForwardBound` check for a reverse tunnel. Without them that check reports `UNKNOWN`. `remote doctor`'s `AllowTcpForwarding` and `ClientAliveInterval` checks run `sshd -T`, which usually needs root; without it they report `UNKNOWN`.
- Optional: a container runtime, only if your agents run containers.

### Network

- **Inbound:** ssh only.
- **Outbound HTTPS** to:
  - GitHub release downloads (`github.com` and the storage host it redirects to), for `remote add` and `remote upgrade` to fetch the binary. Not needed with a Homebrew install or `--no-install`.
  - Your agents' model provider (for example the Anthropic API).
  - Package registries and git hosts your project uses.

On a restricted network, allow those destinations specifically. If the host cannot be given access to something your laptop can reach (an internal git server behind your VPN, say), see [Reaching networks only your laptop can see](remote-recipes.md#reaching-networks-only-your-laptop-can-see).

### Hardware

A starting point; actual use depends on project size and how many agents run at once.

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 2 vCPU | 4 vCPU |
| RAM | 2 GB | 8 GB |
| Disk | 10 GB | 40 GB |

Disk is mostly the project's working tree and build caches. RAM grows with the number of parallel agents and what they run (compilers, language servers, containers).

### Where the deck puts its files

| What | Where on the host |
|---|---|
| The binary | `~/.local/bin/dot-agent-deck`, or `<homebrew prefix>/bin/dot-agent-deck` on a Homebrew host (see [Hosts where Homebrew installed the deck](remote-environments.md#hosts-where-homebrew-installed-the-deck)) |
| Configuration | `~/.config/dot-agent-deck/` |
| Claude Code hooks | `~/.claude/settings.json` |
| Hook socket | `$DOT_AGENT_DECK_SOCKET` if set; else `$XDG_RUNTIME_DIR/dot-agent-deck.sock` if `XDG_RUNTIME_DIR` is set; else `dot-agent-deck-<uid>/hook.sock` in the temp directory (`$TMPDIR`, or `/tmp` when it is unset or empty) |
| Attach socket | `$DOT_AGENT_DECK_ATTACH_SOCKET` if set; else `$XDG_RUNTIME_DIR/dot-agent-deck-attach.sock`; else `dot-agent-deck-<uid>/attach.sock` in the temp directory |

To see which attach socket a running daemon is using, run `dot-agent-deck daemon endpoint` on the host; it prints the path only when a live daemon answers there. Processes that should talk to the same daemon (the TUI, hooks, the desktop app's tunnel) must see the same values of these variables; a process started with a different `XDG_RUNTIME_DIR` looks for a different socket. The temp-directory fallback is a directory the deck creates owner-only (mode `0700`). If another user already owns that name, the deck uses a sibling named `dot-agent-deck-<uid>.<16 hex digits>` instead and logs a warning naming both.

## Recommended for persistent and safe use

### Keep the daemon running

**Across disconnects.** The daemon that `connect` starts runs in its own session, detached from your ssh session, so it keeps running after you disconnect, as long as it has agents. That holds on a host with systemd-logind's default settings. If the host's `/etc/systemd/logind.conf` sets `KillUserProcesses=yes`, logind kills everything started from your ssh session when it ends, the daemon included; run the daemon as a systemd user service instead (below).

**With no agents.** A daemon exits about 30 seconds after its last client disconnects when it has no agents and no enabled schedules. That is fine for `connect`, which starts one when needed. The desktop app does not start one, so for it, and for schedules you want to fire while nobody is connected, run a daemon with the idle shutdown off (`DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0`).

**Across reboots.** Agents do not survive a host reboot, and the deck installs no service that restarts the daemon. On Linux with systemd, this user unit is a starting point; the project has not validated it end to end. Save it as `~/.config/systemd/user/dot-agent-deck.service`:

```ini
[Unit]
Description=dot-agent-deck daemon

[Service]
ExecStart=%h/.local/bin/dot-agent-deck daemon serve
Environment=DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0
Restart=on-failure

[Install]
WantedBy=default.target
```

Then enable it, and enable lingering so the user's services start at boot without a login:

```bash
systemctl --user daemon-reload
systemctl --user enable --now dot-agent-deck.service
sudo loginctl enable-linger "$USER"
```

Check it: `systemctl --user status dot-agent-deck.service` shows `active (running)`, and `~/.local/bin/dot-agent-deck daemon endpoint` prints a socket path. For `connect` to find this daemon, your ssh session must see the same `XDG_RUNTIME_DIR` as the service, which is normally the case on a systemd host, where `pam_systemd` sets it for ssh logins; check with `ssh <target> 'echo $XDG_RUNTIME_DIR'`. After `remote upgrade`, or the desktop app's **Upgrade**, there is nothing to do: the restart ends the service's daemon and systemd starts the service again on the new release. Keep `Restart=on-failure` (or `Restart=always`) in the unit, because that is what starts it again. The journal records the old daemon's exit as `status=75/TEMPFAIL` followed by `Scheduled restart job`; that is the upgrade's restart, not an error. A service starts with a minimal environment, and the daemon takes only `PATH` from your login shell, so put agent credentials in the unit (`Environment=`, or an `EnvironmentFile=` with mode `0600`).

On macOS, see [macOS as a remote host](#macos-as-a-remote-host) for the launchd equivalent.

### Non-root user account

Run the daemon as a dedicated non-root user. The deck runs as root, but agents run as children of the daemon under the same account, so under root every command an agent runs has full control of the host. [Remote Recipes](remote-recipes.md#bootstrapping-a-linux-host) shows how to create the user.

### `~/.local/bin` on PATH

`connect` and `remote add` call the binary by its full path, so they work without it. Add it anyway so you can run `dot-agent-deck` in a shell on the host:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
```

### SSH hardening

- Key-based authentication only: `PasswordAuthentication no` in `sshd_config`.
- No root login: `PermitRootLogin no`.
- Follow your distribution's ssh hardening guide beyond these two.

### Credentials and one host per project

- Give agents their credentials through the daemon user's environment (for example `ANTHROPIC_API_KEY`), or the agent's own login flow. Do not put them in world-readable files or in the project.
- Agents on one host share that account's files and credentials; the deck adds no isolation between them. Use a separate host, or a separate account, for projects whose credentials should not mix.
- Keep one directory per project, for example under `~/projects/`, and move changes with git. The deck does not sync files between your laptop and the host.

### Daemon socket security

The sockets are created owner-only (mode `0600`), and a client checks that the process listening on the socket runs as the same user before using it. Other unprivileged users on the host cannot use your sockets; root and any process running as you can, which is how hooks reach the daemon. On a host shared with other users, put the sockets in a directory you own by setting both variables for every process that runs the deck (the systemd unit, your shell profile):

```bash
export DOT_AGENT_DECK_SOCKET="$HOME/.local/state/dot-agent-deck/daemon.sock"
export DOT_AGENT_DECK_ATTACH_SOCKET="$HOME/.local/state/dot-agent-deck/attach.sock"
mkdir -p ~/.local/state/dot-agent-deck && chmod 700 ~/.local/state/dot-agent-deck
```

A restrictive `umask 077` in the unit or login profile is a sound default on a shared host: it does not change the sockets' mode, but it does apply to other files the daemon and agents write.

## macOS as a remote host

A Mac on Apple Silicon works as a host: `remote add`, `connect`, a Claude Code agent working in a pane, disconnecting and reconnecting have been run end to end. Four things differ from Linux.

**Enable Remote Login.** In System Settings → General → Sharing, turn on **Remote Login** so the Mac accepts ssh.

**Log Claude Code in once, inside the pane.** Claude Code on macOS stores its credentials in the login Keychain, which an ssh session cannot unlock. The first agent prints `Not logged in · Please run /login`. Run `/login` in that pane once; Claude Code then writes `~/.claude/.credentials.json`, which later sessions use. Codex and OpenCode keep their credentials in files and need no such step. Setting `ANTHROPIC_API_KEY` for the daemon's user also works, but bills as API usage rather than against a subscription.

**Stop the Mac from sleeping.** System sleep suspends the agents; display sleep does not. On the Mac:

```bash
sudo pmset -a sleep 0          # no system sleep
sudo pmset -a displaysleep 15  # screen off after 15 minutes
sudo pmset -a disksleep 0
```

**Sockets are in the temp directory.** macOS does not set `XDG_RUNTIME_DIR`, so the sockets use the temp-directory fallback in [Where the deck puts its files](#where-the-deck-puts-its-files) unless you set `DOT_AGENT_DECK_SOCKET` and `DOT_AGENT_DECK_ATTACH_SOCKET`. Those overrides have not been exercised on macOS.

**Restarting after a reboot** needs a LaunchAgent, which the deck does not install. This is a starting point that has not been tested across a reboot. Save it as `~/Library/LaunchAgents/ai.devopstoolkit.dot-agent-deck.plist`, replacing `YOU` (launchd does not expand `~`):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>ai.devopstoolkit.dot-agent-deck</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/YOU/.local/bin/dot-agent-deck</string>
    <string>daemon</string>
    <string>serve</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS</key>
    <string>0</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/Users/YOU/Library/Logs/dot-agent-deck.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/YOU/Library/Logs/dot-agent-deck.log</string>
</dict>
</plist>
```

Load it with `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.devopstoolkit.dot-agent-deck.plist`. Keep `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0`: without it, a daemon with no agents can exit on its idle timer and `KeepAlive` starts it again, over and over. `KeepAlive` is also what starts the daemon again on the new release after `remote upgrade` or the desktop app's **Upgrade**; that has not been tested on a Mac. Three constraints:

- **A LaunchAgent, not a LaunchDaemon.** A LaunchDaemon runs outside your user session, without your environment or the `~/.claude/.credentials.json` that `/login` writes.
- **A LaunchAgent starts at GUI login, not at boot.** An unattended Mac needs automatic login to come back after a reboot, and FileVault asks for a password at startup before any automatic login. Decide that trade-off deliberately.
- **launchd does not read your shell profile** except for the `PATH` the daemon captures itself. Put anything else the agents need, such as an `ANTHROPIC_API_KEY`, in the plist's `EnvironmentVariables`.

## What is not required

- No inbound port besides ssh.
- No particular cloud provider, hypervisor or provisioning tool. A machine that meets the requirements above can be anywhere, including on your desk.
- No file sync tool. Use git.
- No reverse tunnel, unless the host needs to reach a network only your laptop can reach.
