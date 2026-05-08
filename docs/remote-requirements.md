---
sidebar_position: 7.5
title: Remote Environment Requirements
---

# Remote Environment Requirements

What a host must provide for a `dot-agent-deck` **remote environment** — a per-project, long-running Linux host that runs the deck daemon and owns the project's agents. This is not a provisioning guide and not a daily-use guide; it lists the prerequisites a Linux VM must satisfy before the deck can register it as a remote.

> **Status:** v1 best-guess requirements. They will be refined in PRD #76 M0.3 once a clean re-provision validates them. Numbers and tested-distro lists may change.

## Supported operating systems

Linux is the only supported host for a remote environment. macOS and Windows are not supported as remote hosts (you can still use them as the local client).

| Distribution | Status |
|---|---|
| Ubuntu 24.04 LTS | Tested |

Other modern systemd-based Linux distributions are likely to work but have not been exercised. If you'd like a specific distribution validated, [open an issue](https://github.com/vfarcic/dot-agent-deck/issues) and we'll add it to the test matrix.

## Hardware

Provisional sizing. Actual usage depends on workspace size and the number of concurrent agents.

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 2 vCPU | 4 vCPU |
| RAM | 2 GB | 8 GB |
| Disk | 10 GB | 40 GB |

The daemon and a single agent are lightweight. Disk is dominated by the project's git working tree plus build/test caches. RAM scales with the number of agents you run in parallel and what they invoke (compilers, language servers, container runtimes).

## Network

**Outbound (required):** HTTPS access to whatever the agents call. Typical destinations:

- Anthropic API (or whichever LLM provider the agents use)
- Package registries the project depends on (npm, PyPI, crates.io, Go module proxy, etc.)
- Git remotes (GitHub, GitLab, etc.) — git is the sync layer between your laptop's working copy and the remote

If you run in a network-restricted environment, allowlist only the specific destinations your agents and toolchains use rather than allowing wide-open outbound; the destinations vary per project.

**Inbound (required):** SSH only (port 22 by default), reachable from the laptop running the deck client. The daemon itself does not listen on any TCP port — it serves a Unix domain socket on the remote, and hooks reach it over localhost.

No other inbound ports are required.

**SSH hardening (recommended):**

- Use key-based authentication only; disable password auth in `sshd_config` (`PasswordAuthentication no`).
- Disable root login (`PermitRootLogin no`); the daemon runs as a non-root user anyway.
- SSH hardening on the host is the operator's responsibility — follow your distribution's own SSH hardening guide rather than treating these bullets as exhaustive.

## User account

A non-root Linux user account. The daemon runs as that user; agents run as children of the daemon and inherit the account.

`systemd --user` is the recommended persistence layer, so the daemon survives logout and restarts on crash. Enable lingering for the user so user-scoped units run without an active login session:

```bash
sudo loginctl enable-linger $USER
```

## Required software

The host must have:

- `bash`
- An OpenSSH server (`sshd`)
- `git`
- A working PTY layer (standard on every Linux distribution)

Optional:

- A container runtime (Docker or Podman) — only required if your agents themselves run containers. The daemon does not need one.

## Install path

The deck installs the `dot-agent-deck` binary to `~/.local/bin/dot-agent-deck` on the remote by default. Make sure that directory is on the user's `$PATH`. If it isn't already, add it once:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
```

## Daemon runtime files

The daemon resolves its socket and config paths at startup from environment variables that follow the XDG Base Directory spec.

**Socket path** — checked in this order:

1. `$DOT_AGENT_DECK_SOCKET` if set (explicit override)
2. `$XDG_RUNTIME_DIR/dot-agent-deck.sock` if `XDG_RUNTIME_DIR` is set (the case on systemd hosts with `logind`, which is the typical case)
3. `/tmp/dot-agent-deck.sock` as a last-resort fallback

**Socket security:** the `/tmp/dot-agent-deck.sock` fallback is intended for hosts that do not set `XDG_RUNTIME_DIR` and is **not safe on multi-user hosts** — `/tmp` is world-writable, and another local user could pre-create the path or attach to the socket. v1 assumes a single-user host (the PRD's stated scope: "no multi-user access controls beyond Linux user separation"). If the host is shared, set `$DOT_AGENT_DECK_SOCKET` to a path inside the user's home, for example `~/.local/state/dot-agent-deck/daemon.sock`, and ensure the parent directory is mode `0700`. The socket file itself should be readable and writable only by the daemon's user (mode `0600`); the daemon currently relies on the process umask for this, so set a restrictive umask (e.g. `umask 077`) in the unit file or login profile when on a shared host.

Hooks running on the same host resolve the same path the same way and connect via Unix socket. Nothing crosses the network.

**Config directory:** `~/.config/dot-agent-deck/` (standard XDG config home).

## Project filesystem layout

**One environment per project.** A remote environment is bound to a single project; agents inside the environment all operate on the same project tree. The `~/projects/` convention below is for users who run multiple environments side-by-side on the same host (one directory per environment, one environment per project), not for packing several projects into a single environment.

Project files live on the remote — agents read and write them in place. Recommended layout: one directory per project under `~/projects/`.

```bash
mkdir -p ~/projects
```

Git is the sync layer. Clone the repository on the remote, run agents against it there, and push/pull through your usual git remote. There is no bidirectional file sync between your laptop and the remote — the deck does not bundle mutagen, syncthing, or sshfs.

## What is not required

To rule out common assumptions:

- **No inbound network port for the daemon** beyond SSH (port 22 by default). The daemon never opens a TCP listener.
- **No cloud-provider account.** Multipass, Hetzner, Fly, a bare-metal box on your desk — any of them works.
- **No specific provisioner.** There is no opinion in the product about how the VM gets created.
- **No Terraform, Pulumi, or other IaC** is shipped or required.
- **No bidirectional file sync** between laptop and remote. Use git.
- **No reverse tunnel** from the remote back to the laptop. The daemon is the long-lived process; the laptop is just a viewer.
