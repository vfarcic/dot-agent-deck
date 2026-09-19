---
sidebar_position: 7.5
title: Remote Environment Requirements
---

# Remote Environment Requirements

What a host must provide for a `dot-agent-deck` **remote environment** — a per-project, long-running host that runs the deck daemon and owns the project's agents. This is not a provisioning guide and not a daily-use guide; it lists the prerequisites a host must satisfy before the deck can register it as a remote. Throughout, the **host** is that machine and **your laptop** is whatever machine you connect *from* — a split of roles rather than of hardware, since a host may itself be a laptop. Everything below is written for Linux, which is the only host validated end to end; `remote add` also installs onto macOS, and [macOS as a remote host](#macos-as-a-remote-host) sets out what is and is not known about that.

For lifecycle, failure modes, and how connecting works see [Remote Environments](remote-environments.md). For provisioning recipes see [Remote Recipes](remote-recipes.md).

> **Status:** v1 requirements. The Required section reflects what was confirmed to work on a fresh Linux VM (Ubuntu 24.04 LTS, though nothing here requires it); the Recommended section reflects best-practice hardening that has not yet been re-validated end to end on a clean provision. macOS was validated end to end on Apple Silicon on 2026-09-19 ([#1158](https://github.com/vfarcic/dot-agent-deck/issues/1158)).

## How this page is organized

Requirements are split into two sections. **Required** is the strict minimum for the daemon to launch and an agent to run on a remote at all — confirmed empirically on a fresh Linux VM. **Recommended for persistent and safe use** is what hardens the install and delivers the deck's reason for existing as a remote: agents that keep running while your own machine sleeps or loses the network, and that come back after the *host* reboots. Without the recommended setup, the daemon will still start, but agents will not survive you disconnecting — which defeats the whole point of running the deck remotely.

> **Warning — do not stop at Required.** A host that satisfies only the Required section will function, but it runs the daemon as root, accepts default SSH configuration, and places the daemon socket in `/tmp` — none of which are safe defaults on a multi-user host or anything resembling production. Anyone running beyond a personal sandbox should follow the Recommended section.

## Required

The strict minimum for the daemon to launch and an agent to run.

### Operating system

| Host OS | Status |
|---|---|
| Linux (amd64, arm64) | Validated end to end. Any modern distribution — see [Which Linux distribution](#which-linux-distribution) |
| macOS | Validated end to end on Apple Silicon — see [macOS as a remote host](#macos-as-a-remote-host) for the two setup steps it needs |
| Windows | Not supported as a remote host (you can still use it as the local client) |

Windows is out for a concrete reason rather than a policy one: `remote add` does not recognise Windows as a host, so registering one fails before it gets as far as installing anything — and there is no Windows build of the daemon for it to install in any case.

#### Which Linux distribution

**Any modern one.** The deck never checks: `remote add` reads `uname -s -m` and cares only that it says `Linux` and an architecture it has a build for. Ubuntu 24.04 LTS is what the validation runs happened to use, not a requirement, and the `apt` commands in [Remote Recipes](remote-recipes.md) are one distribution's spelling of steps every distribution has.

Two things genuinely do vary, and neither is about which distribution you prefer:

- **glibc.** The published Linux binaries are `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` — glibc builds, with no musl variant. On a musl-based distribution such as Alpine, install from source instead; the [Nix flake](installation.md#nix) covers `x86_64-linux` and `aarch64-linux`.
- **systemd**, and only for the *recommended* setup rather than for the daemon itself. `systemd --user` plus `loginctl enable-linger` is how the daemon survives logout and restarts after a crash, and `XDG_RUNTIME_DIR` — which `logind` sets — is what keeps the socket out of `/tmp`. Without systemd you still get a working remote; what you lose is restart-on-boot, and the socket falls back to `/tmp/dot-agent-deck-{uid}.sock` — [the same position a macOS host is in](#macos-as-a-remote-host), for the same reason.

If you would like a specific distribution added to the test matrix, [open an issue](https://github.com/vfarcic/dot-agent-deck/issues).

### macOS as a remote host

**macOS works as a remote host.** The whole path was run end to end on 2026-09-19 — `remote add` over ssh, `connect`, a Claude agent working in a pane, a disconnect, and a reconnect with the session intact — on a Mac Studio running macOS 26.6.2 (`Darwin arm64`), with the deck at 0.41.0 on both ends. [#1158](https://github.com/vfarcic/dot-agent-deck/issues/1158) carries the run and its output, and `remote doctor` reports the same thing it does for a working Linux remote. (That was an Apple Silicon Mac. `remote add` installs a `darwin-amd64` build on an Intel one, which nobody has exercised.)

A Mac needs two setup steps a Linux host does not, and differs in two further ways worth knowing about.

**Log Claude Code in once, inside the pane.** Claude Code on macOS keeps its credentials in the login Keychain, and an ssh session cannot read them — ssh authenticates by key, so the login password is never presented to unlock the Keychain, and there is no graphical context in which to prompt for it. The first run therefore prints `Not logged in · Please run /login`. That message is the fix: run `/login` once inside the pane, and Claude Code writes `~/.claude/.credentials.json`, which later sessions authenticate from. This is specific to Claude Code — `codex` and `opencode` keep their credentials in files already (`~/.codex/auth.json`, `~/.local/share/opencode/auth.json`) and need no equivalent step. Setting `ANTHROPIC_API_KEY` in the daemon user's environment works too, but it bills as API usage rather than against a subscription, so treat it as the fallback rather than the first choice.

**Stop the host going to sleep.** A Mac that sleeps suspends its agents along with everything else. *Display* sleep is harmless — it is *system* sleep that suspends processes — so on a desktop Mac:

```bash
sudo pmset -a sleep 0          # never system-sleep; this is the load-bearing one
sudo pmset -a displaysleep 15  # monitors off, machine stays awake
sudo pmset -a disksleep 0
```

This is the opposite direction from [Surviving sleep/wake](remote-environments.md#surviving-sleepwake), which is about **your own machine** sleeping — ssh keepalive and automatic reconnection already handle that one. A sleeping **host** has no deck-side mechanism at all, which is why it is a `pmset` matter.

**The daemon socket lands in `/tmp`.** macOS sets no `XDG_RUNTIME_DIR`, so the socket falls through to `/tmp/dot-agent-deck-{uid}.sock` — `/tmp/dot-agent-deck-501.sock` on the validation host. The owner-only mode this page promises does hold there (`srw-------`), so the socket file itself is not readable by other users; what remains is the directory concern described under [Daemon socket security](#daemon-socket-security). Setting `$DOT_AGENT_DECK_SOCKET` to a path inside your home directory should work exactly as it does on Linux, but that override was not exercised on macOS.

**Nothing is set up to restart the deck after a reboot — you have to arrange it.** The persistence setup under [Recommended](#recommended-for-persistent-and-safe-use) is built around `systemd --user`, which macOS does not have, and the deck installs no launchd equivalent. Agents do survive you disconnecting — that was part of the validated run and does not depend on systemd. What happens across a reboot was not tested, so what follows is a starting point rather than a verified recipe; whether the deck should ship a plist of its own is [#1172](https://github.com/vfarcic/dot-agent-deck/issues/1172).

The launchd counterpart of a `systemd --user` unit is a **LaunchAgent**. Write `~/Library/LaunchAgents/ai.devopstoolkit.dot-agent-deck.plist`, substituting your own absolute paths — launchd does not expand `~`:

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

`RunAtLoad` and `KeepAlive` are the two that matter: they are what `systemd --user` gives you as start-on-login and restart-on-crash. Load it with:

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.devopstoolkit.dot-agent-deck.plist
```

Three things are easy to get wrong here, and each costs something different:

- **A LaunchAgent, not a LaunchDaemon.** A LaunchDaemon runs in the system context with no user session, so it forfeits the user's environment and credentials — including the `~/.claude/.credentials.json` that the `/login` step above exists to create. The daemon must run as you.
- **A LaunchAgent starts at GUI login, not at boot.** So an always-on Mac with nobody sitting at it needs **auto-login** enabled to come back unattended after a reboot, and auto-login interacts with FileVault: a FileVault-encrypted disk requires a password at startup before any login can happen automatically. That is a real security tradeoff, not a checkbox — decide it deliberately.
- **launchd does not source your shell profile.** The daemon repairs its own `PATH` at startup by capturing it from an interactive login shell, so `~/.local/bin` resolves either way — but nothing else does. Anything your agents need from the environment, an `ANTHROPIC_API_KEY` for instance, has to go in an `EnvironmentVariables` dict in the plist rather than in `~/.zshrc`.

Three things are still open, if you are in a position to check any of them: **reboot behaviour**, the **`$DOT_AGENT_DECK_SOCKET` override** on macOS, and **hook delivery asserted directly** rather than inferred from an agent reaching `Working`. [Report what you find](https://github.com/vfarcic/dot-agent-deck/issues).

### Hardware

Provisional sizing. Actual usage depends on workspace size and the number of concurrent agents.

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 2 vCPU | 4 vCPU |
| RAM | 2 GB | 8 GB |
| Disk | 10 GB | 40 GB |

The daemon and a single agent are lightweight. Disk is dominated by the project's git working tree plus build/test caches. RAM scales with the number of agents you run in parallel and what they invoke (compilers, language servers, container runtimes).

### Network

**Outbound (required):** HTTPS access to whatever the agents call. Typical destinations:

- Anthropic API (or whichever LLM provider the agents use)
- Package registries the project depends on (npm, PyPI, crates.io, Go module proxy, etc.)
- Git remotes (GitHub, GitLab, etc.) — git is the sync layer between your laptop's working copy and the remote

If you run in a network-restricted environment, allowlist only the specific destinations your agents and toolchains use rather than allowing wide-open outbound; the destinations vary per project.

If the remote *cannot* be given access to a destination your laptop can reach — an internal git host behind a corporate VPN, say — see [Reaching networks only your laptop can see](remote-recipes.md#reaching-networks-only-your-laptop-can-see) for how to lend the remote your laptop's access over a reverse tunnel.

**Inbound (required):** SSH only (port 22 by default), reachable from the laptop running the deck client. The daemon never listens on a TCP port — everything it serves stays on the host itself.

No other inbound ports are required.

### Required software

The host must have:

- `bash`
- An OpenSSH server (`sshd`)
- `git` — typically pre-installed on cloud Linux images. Only needs an explicit install if it's missing.
- A working PTY layer (standard on every Linux distribution)

**AI agent runtime.** The deck launches AI agents but does not bundle them — the agent and its runtime must already be on the host, otherwise the deck has nothing to spawn. You need:

- The agent CLI itself. Claude Code, OpenCode, Pi, Codex, and Devin have first-class event/status support in the deck today. Other agents may work if their CLI behaves like an ordinary interactive terminal program, but those five are the ones with first-class event support.
- The runtime that agent depends on (e.g. Node.js for npm-distributed agents like Claude Code).
- The agent's API credentials available in the user's environment (e.g. `ANTHROPIC_API_KEY` for Claude Code).

Install hints (pick whichever agent you use; install only what you need):

- Claude Code: `npm install -g @anthropic-ai/claude-code` (requires Node.js).
- OpenCode: `npm install -g opencode-ai` (requires Node.js).
- Pi: `npm install -g @earendil-works/pi-coding-agent` (requires Node.js).
- Codex: `npm install -g @openai/codex` (requires Node.js).
- Devin: follow the [Devin CLI install instructions](https://devin.ai/support) (the `devin` binary must be on `PATH`).
- Other agents: follow the agent's own install instructions.

The deck does not prescribe a specific agent or pin a specific install method — install whichever supported agent you plan to run, by whichever method that agent's documentation recommends.

**Credentials and host scope.** A few rules of thumb for where agent credentials should live and what a host should hold:

- Set credentials as environment variables in the daemon user's environment (e.g. `ANTHROPIC_API_KEY`). Do not commit them to disk in plaintext config, do not paste them into shell history, and do not place them in world-readable files.
- A remote environment is per-project. Do not reuse a single host for unrelated projects with their own credentials — agent isolation in v1 is at the host level, so agents that share a host share its credentials and filesystem.
- For `systemd --user` setups, put credentials in the unit's `Environment=` directive or in an `EnvironmentFile=` with mode `0600` — not in `~/.bashrc` or other shell rc files where they leak into every interactive shell and may be sourced by unrelated tooling.

Optional:

- A container runtime (Docker or Podman) — only required if your agents themselves run containers. The daemon does not need one.

### Daemon binary

The deck installs the `dot-agent-deck` binary to `~/.local/bin/dot-agent-deck` on the remote by default.

### Daemon runtime files

The daemon resolves its socket and config paths at startup from environment variables that follow the XDG Base Directory spec.

**Socket path** — checked in this order:

1. `$DOT_AGENT_DECK_SOCKET` if set (explicit override)
2. `$XDG_RUNTIME_DIR/dot-agent-deck.sock` if `XDG_RUNTIME_DIR` is set (the case on systemd hosts with `logind`, which is the typical case)
3. `/tmp/dot-agent-deck-{uid}.sock` as a last-resort fallback, where `{uid}` is your user id so two users on the same host never collide

The `/tmp` fallback is **not safe on multi-user hosts** — see [Daemon socket security](#daemon-socket-security) under Recommended for the override and directory-permission guidance. Step 2 works on any host that sets `XDG_RUNTIME_DIR`, but macOS does not set it for you, so a Mac lands on step 3 unless you set one of the two variables yourself — see [macOS as a remote host](#macos-as-a-remote-host).

Hooks running on the same host find the daemon the same way. Nothing crosses the network.

**Config directory:** `~/.config/dot-agent-deck/` (standard XDG config home).

## Recommended for persistent and safe use

Hardening, persistence, and best-practice setup. None of this is needed for the daemon to start, but skipping it means agents won't survive a laptop disconnect, the daemon won't restart on crash, and the host posture will be looser than it should be.

### Non-root user account

A dedicated non-root Linux user account for the daemon. Running the daemon as root works, but anything an agent does then runs with full system privileges; a non-root account is the recommended posture. The daemon runs as that user; agents run as children of the daemon and inherit the account.

`systemd --user` is the recommended persistence layer, so the daemon survives logout and restarts on crash. Enable lingering for the user so user-scoped units run without an active login session:

```bash
sudo loginctl enable-linger $USER
```

Neither of those exists on macOS, and the deck ships no launchd equivalent. A Mac's daemon does survive your ssh session ending — that was validated — but the deck installs nothing that would bring it back after a reboot; see [macOS as a remote host](#macos-as-a-remote-host) and [#1172](https://github.com/vfarcic/dot-agent-deck/issues/1172).

### `~/.local/bin` on PATH

When running the daemon as a non-root user, make sure `~/.local/bin` is on that user's `$PATH` so the installed binary is reachable from a fresh shell. If it isn't already, add it once:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
```

Root has its own PATH and typically doesn't need this step.

### SSH hardening

- Use key-based authentication only; disable password auth in `sshd_config` (`PasswordAuthentication no`).
- Disable root login (`PermitRootLogin no`); the daemon runs as a non-root user anyway.
- SSH hardening on the host is the operator's responsibility — follow your distribution's own SSH hardening guide rather than treating these bullets as exhaustive.

### Daemon socket security

The `/tmp/dot-agent-deck-{uid}.sock` fallback is intended for hosts that do not set `XDG_RUNTIME_DIR`. The `{uid}` suffix means two users on the same host get disjoint paths so neither can pre-create the other's socket path; the underlying directory is still world-writable, so the fallback remains **not safe on multi-user hosts** in the broader sense — another local user can still observe socket-path activity in `/tmp` and could attempt to connect. v1 assumes a single-user host: there are no multi-user access controls beyond the Linux user separation the OS already gives you. If the host is shared, set `$DOT_AGENT_DECK_SOCKET` to a path inside the user's home, for example `~/.local/state/dot-agent-deck/daemon.sock`, and ensure the parent directory is mode `0700`. The socket file itself is always created owner-only (`0600`), and is never briefly readable by anyone else while it is being created. Your own umask cannot weaken that — but it does still affect other files the daemon and its agents write, so a restrictive umask (e.g. `umask 077`) in the unit file or login profile remains a sound default on a shared host.

### Project filesystem layout

**One environment per project.** A remote environment is bound to a single project; agents inside the environment all operate on the same project tree. The `~/projects/` convention below is for users who run multiple environments side-by-side on the same host (one directory per environment, one environment per project), not for packing several projects into a single environment.

Project files live on the remote — agents read and write them in place. Recommended layout: one directory per project under `~/projects/`.

```bash
mkdir -p ~/projects
```

Git is the sync layer. Clone the repository on the remote, run agents against it there, and push/pull through your usual git remote. There is no bidirectional file sync between your laptop and the remote — the deck does not bundle mutagen, syncthing, or sshfs.

## See also

- [Remote Environments](remote-environments.md) — lifecycle model, stop vs detach, failure modes, hooks behavior.
- [Remote Recipes](remote-recipes.md) — how to get a Linux or macOS host bootstrapped for `remote add`.

## What is not required

To rule out common assumptions:

- **No inbound network port for the daemon** beyond SSH (port 22 by default). The daemon never opens a TCP listener.
- **No cloud-provider account.** Multipass, Hetzner, Fly, a bare-metal box on your desk — any of them works.
- **No specific provisioner.** There is no opinion in the product about how the VM gets created.
- **No Terraform, Pulumi, or other IaC** is shipped or required.
- **No bidirectional file sync** between laptop and remote. Use git.
- **No reverse tunnel** from the remote back to the laptop. The daemon is the long-lived process; the laptop is just a viewer.
