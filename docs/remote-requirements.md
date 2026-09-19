---
sidebar_position: 7.5
title: Remote Environment Requirements
---

# Remote Environment Requirements

What a host must provide for a `dot-agent-deck` **remote environment** — a per-project, long-running host that runs the deck daemon and owns the project's agents. This is not a provisioning guide and not a daily-use guide; it lists the prerequisites a host must satisfy before the deck can register it as a remote. Everything below is written for Linux, which is the only host validated end to end; `remote add` also installs onto macOS, and [macOS as a remote host](#macos-as-a-remote-host) sets out what is and is not known about that.

For lifecycle, failure modes, and how the TUI attaches see [Remote Environments](remote-environments.md). For provisioning recipes see [Remote Recipes](remote-recipes.md).

> **Status:** v1 requirements. The Required section reflects what was confirmed to work on a fresh Ubuntu 24.04 LTS UpCloud VM, which has been the reference target throughout; the Recommended section reflects best-practice hardening that has not yet been re-validated end to end on a clean provision. macOS is a third state — `remote add` installs there and the release ships a daemon build for it, but nobody has run the path end to end, so it is documented as what it is rather than as supported or unsupported.

## How this page is organized

Requirements are split into two sections. **Required** is the strict minimum for the daemon to launch and an agent to run on a remote at all — confirmed empirically on a fresh Ubuntu 24.04 LTS VM. **Recommended for persistent and safe use** is what hardens the install and delivers the deck's reason for existing as a remote: persistence across laptop sleep, network drops, and reboots. Without the recommended setup, the daemon will still start, but agents will not survive your laptop disconnecting — which defeats the whole point of running the deck remotely.

> **Warning — do not stop at Required.** A host that satisfies only the Required section will function, but it runs the daemon as root, accepts default SSH configuration, and places the daemon socket in `/tmp` — none of which are safe defaults on a multi-user host or anything resembling production. Anyone running beyond a personal sandbox should follow the Recommended section.

## Required

The strict minimum for the daemon to launch and an agent to run.

### Operating system

| Host OS | Status |
|---|---|
| Linux (amd64, arm64) | Validated end to end. The reference target throughout. |
| macOS (Intel, Apple Silicon) | `remote add` installs and the release ships a daemon build, but the path has not been validated end to end — see [macOS as a remote host](#macos-as-a-remote-host) |
| Windows | Not supported as a remote host (you can still use it as the local client) |

Windows is out for a concrete reason rather than a policy one: the platform detection `remote add` runs has no Windows arm, so registration fails before it reaches an install, and the release publishes no Windows daemon binary for it to install anyway.

| Linux distribution | Status |
|---|---|
| Ubuntu 24.04 LTS | Tested |

Other modern systemd-based Linux distributions are likely to work but have not been exercised. If you'd like a specific distribution validated, [open an issue](https://github.com/vfarcic/dot-agent-deck/issues) and we'll add it to the test matrix.

### macOS as a remote host

`dot-agent-deck remote add` accepts a Mac and installs onto it. Whether you should rely on one as a remote host is a different question, and the honest answer today is that the path has never been validated end to end — no test covers it and no run of it is recorded. So this section separates what the code and release pipeline make verifiable from what genuinely needs a Mac to find out.

**What is verified.** `remote add` probes the host with `uname -s -m` and applies no OS gate: `Darwin x86_64` resolves to the published `darwin-amd64` release asset and `Darwin arm64` to `darwin-arm64`, both of which the release workflow builds and uploads alongside the Linux pair. Each piece of the remote flow that was checked is written for Unix rather than for Linux specifically — the daemon detaches from the ssh session with `setsid(2)`, `hooks install` has a single Unix implementation, and the daemon's peer-credential check carries a macOS branch of its own. That is three spot-checks, not an audit of every line the flow touches. macOS is also a supported platform for the deck locally, and `daemon serve` is a subcommand of that same binary, so what is untested here is this *arrangement* — install over ssh, run headless, attach from another machine — rather than whether the daemon runs on macOS at all.

**What is not verified.** The whole arrangement: installing onto a Mac over ssh, starting the daemon there, spawning an agent under it, and connecting to it from a second machine. Nothing in the list below is a known failure. It is a list of the places a Mac differs from the Ubuntu host the rest of this page was written against, and therefore where to look first if something does not work.

**No `systemd --user`, so nothing restarts the daemon.** The persistence guidance under [Recommended](#recommended-for-persistent-and-safe-use) is systemd-shaped and none of it applies. The daemon still survives the ssh session that started it, because it detaches with `setsid(2)` and macOS supports that, so agents keep running after you disconnect. It does **not** come back after a reboot or a crash: the deck ships no launchd equivalent of the `systemd --user` unit, and writing one is left to you.

**`XDG_RUNTIME_DIR` is unset by default, so the socket lands in `/tmp`.** `XDG_RUNTIME_DIR` is set by `systemd`'s `logind`, which macOS does not run. The daemon still honours the variable wherever it finds one — that branch of `socket_path()` is not Linux-gated — so a Mac with it exported behaves like a Linux host; it is the *default* that differs. Left unset, the daemon falls through to `/tmp/dot-agent-deck-{uid}.sock` — the fallback this page flags as unsafe on multi-user hosts. On a Mac that is the default path rather than a last resort, so set `$DOT_AGENT_DECK_SOCKET` to a path inside the user's home as described under [Daemon socket security](#daemon-socket-security).

**Agent credentials may sit in the login Keychain, which a non-interactive ssh session may not be able to read.** Claude Code 2.x on macOS stores its credentials as a login-Keychain item rather than in `~/.claude/.credentials.json`, which may be absent entirely. The login Keychain is unlocked by a console login, so a Mac that has rebooted unattended leaves it locked. This combination has not been tested over ssh; if agents fail to authenticate on a Mac remote while the same credentials work locally on it, check this first. Putting the provider's API key in the daemon user's environment, as [Credentials and host scope](#required-software) already recommends, sidesteps the Keychain question altogether.

**Sleep suspends the host.** A Mac that goes to sleep suspends its agents with everything else, where a Linux VM generally does not sleep at all. If you intend to use a Mac as an always-on remote, configure it not to.

If you do run this path, please [report what happened](https://github.com/vfarcic/dot-agent-deck/issues) — validating it end to end is tracked in [#1158](https://github.com/vfarcic/dot-agent-deck/issues/1158).

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

**Inbound (required):** SSH only (port 22 by default), reachable from the laptop running the deck client. The daemon itself does not listen on any TCP port — it serves a Unix domain socket on the remote, and hooks reach it over localhost.

No other inbound ports are required.

### Required software

The host must have:

- `bash`
- An OpenSSH server (`sshd`)
- `git` — typically pre-installed on cloud Linux images (Ubuntu 24.04 cloud images ship with it). Only needs an explicit install if it's missing.
- A working PTY layer (standard on every Linux distribution)

**AI agent runtime.** The deck launches AI agents but does not bundle them — the agent and its runtime must already be on the host, otherwise the deck has nothing to spawn. You need:

- The agent CLI itself. Claude Code, OpenCode, Pi, Codex, and Devin have first-class event/status support in the deck today. Other agents may work if their CLI follows the same PTY pattern, but those five are the ones with first-class event support.
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
3. `/tmp/dot-agent-deck-{uid}.sock` as a last-resort fallback (where `{uid}` is the user's POSIX uid, included so two users on the same host get disjoint sockets — the XDG path at step 2 is already per-user)

The `/tmp` fallback is **not safe on multi-user hosts** — see [Daemon socket security](#daemon-socket-security) under Recommended for the override and directory-permission guidance. On a macOS host step 2 is normally skipped rather than unavailable: `XDG_RUNTIME_DIR` is a `logind` variable and macOS does not set it, but `socket_path()` honours it on every Unix target, so a Mac that has the variable exported — by you or by whatever started the daemon — takes step 2 exactly as a Linux host would. Left alone, step 3 is where a Mac's socket lands — see [macOS as a remote host](#macos-as-a-remote-host).

Hooks running on the same host resolve the same path the same way and connect via Unix socket. Nothing crosses the network.

**Config directory:** `~/.config/dot-agent-deck/` (standard XDG config home).

## Recommended for persistent and safe use

Hardening, persistence, and best-practice setup. None of this is needed for the daemon to start, but skipping it means agents won't survive a laptop disconnect, the daemon won't restart on crash, and the host posture will be looser than it should be.

### Non-root user account

A dedicated non-root Linux user account for the daemon. Running the daemon as root works, but anything an agent does then runs with full system privileges; a non-root account is the recommended posture. The daemon runs as that user; agents run as children of the daemon and inherit the account.

`systemd --user` is the recommended persistence layer, so the daemon survives logout and restarts on crash. Enable lingering for the user so user-scoped units run without an active login session:

```bash
sudo loginctl enable-linger $USER
```

On a macOS host neither of those exists. There is no `systemd --user` and the deck ships no launchd equivalent, so the daemon survives the ssh session but not a reboot — see [macOS as a remote host](#macos-as-a-remote-host).

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

The `/tmp/dot-agent-deck-{uid}.sock` fallback is intended for hosts that do not set `XDG_RUNTIME_DIR`. The `{uid}` suffix means two users on the same host get disjoint paths so neither can pre-create the other's socket path; the underlying directory is still world-writable, so the fallback remains **not safe on multi-user hosts** in the broader sense — another local user can still observe socket-path activity in `/tmp` and could attempt to connect. v1 assumes a single-user host (the PRD's stated scope: "no multi-user access controls beyond Linux user separation"). If the host is shared, set `$DOT_AGENT_DECK_SOCKET` to a path inside the user's home, for example `~/.local/state/dot-agent-deck/daemon.sock`, and ensure the parent directory is mode `0700`. The socket file itself is created at mode `0600` directly by the daemon — `bind(2)` runs with a narrowed umask of `0o177` so the socket inode is never world-readable for any window, and a defense-in-depth `chmod 0600` follows. The user's process umask does not affect the socket mode, but it still affects other files the daemon and its agents create, so a restrictive umask (e.g. `umask 077`) in the unit file or login profile remains a sound default on a shared host.

### Project filesystem layout

**One environment per project.** A remote environment is bound to a single project; agents inside the environment all operate on the same project tree. The `~/projects/` convention below is for users who run multiple environments side-by-side on the same host (one directory per environment, one environment per project), not for packing several projects into a single environment.

Project files live on the remote — agents read and write them in place. Recommended layout: one directory per project under `~/projects/`.

```bash
mkdir -p ~/projects
```

Git is the sync layer. Clone the repository on the remote, run agents against it there, and push/pull through your usual git remote. There is no bidirectional file sync between your laptop and the remote — the deck does not bundle mutagen, syncthing, or sshfs.

## See also

- [Remote Environments](remote-environments.md) — lifecycle model, stop vs detach, failure modes, hooks behavior.
- [Remote Recipes](remote-recipes.md) — provisioning snippets for multipass, Hetzner, UpCloud, bare metal.

## What is not required

To rule out common assumptions:

- **No inbound network port for the daemon** beyond SSH (port 22 by default). The daemon never opens a TCP listener.
- **No cloud-provider account.** Multipass, Hetzner, Fly, a bare-metal box on your desk — any of them works.
- **No specific provisioner.** There is no opinion in the product about how the VM gets created.
- **No Terraform, Pulumi, or other IaC** is shipped or required.
- **No bidirectional file sync** between laptop and remote. Use git.
- **No reverse tunnel** from the remote back to the laptop. The daemon is the long-lived process; the laptop is just a viewer.
