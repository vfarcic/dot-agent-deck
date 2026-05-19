---
sidebar_position: 7.6
title: Remote Recipes
---

# Remote Recipes

Provisioning snippets that get a Linux host into a state where `dot-agent-deck remote add` will succeed. The deck itself doesn't ship a provisioner — these recipes are starting points you adapt to your environment.

For prerequisites the host must satisfy see [Remote Environment Requirements](remote-requirements.md). For lifecycle and connection semantics see [Remote Environments](remote-environments.md). The Kubernetes-as-host recipe lives in [PRD #81](https://github.com/vfarcic/dot-agent-deck/issues/81) and is not yet shipped.

> **Status.** Validated on a fresh Ubuntu 24.04 LTS UpCloud VM (the M0.2 reference host). Other providers should work given the same OS and SSH posture, but have not been independently re-tested. If a provider's image needs different bootstrap steps, the differences are typically in the cloud-init / first-login section — the deck-side flow (`remote add`) is identical once SSH and a non-root user with the agent toolchain are in place.

## Common shape

Every recipe converges on the same end state:

1. A Linux VM running Ubuntu 24.04 LTS (or equivalent), reachable over ssh.
2. A non-root user with `~/.local/bin` on `PATH` and the agent CLI installed.
3. Outbound HTTPS to the LLM provider, package registries, and your git remote.
4. From your laptop:

   ```bash
   dot-agent-deck remote add <name> <user>@<host>
   ```

The recipes below differ only in steps 1–3.

## Multipass (local VM, macOS or Linux)

For a fully local dev setup with no cloud account.

```bash
# Launch an Ubuntu 24.04 LTS VM with sensible defaults.
multipass launch 24.04 --name dad-dev --cpus 2 --memory 2G --disk 20G

# Get into the VM as the default `ubuntu` user.
multipass shell dad-dev
```

Inside the VM:

```bash
# Make sure ~/.local/bin is on PATH for future shells.
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
mkdir -p ~/.local/bin

# Install Node.js (for npm-distributed agents like Claude Code).
sudo apt-get update
sudo apt-get install -y nodejs npm

# Install your agent. Example: Claude Code.
npm install -g @anthropic-ai/claude-code

# Set the agent's API key in your shell rc.
echo 'export ANTHROPIC_API_KEY=sk-ant-...' >> ~/.bashrc

# Enable systemd lingering so user services survive your shell exit.
sudo loginctl enable-linger $USER
exit
```

Back on the laptop:

```bash
# Multipass exposes the VM's IP via `multipass info`.
IP=$(multipass info dad-dev | awk '/IPv4/ {print $2; exit}')

# Multipass installs your laptop's authorized key by default; if not, use
# `multipass exec dad-dev -- bash -c 'echo <pubkey> >> ~/.ssh/authorized_keys'`.
dot-agent-deck remote add dad-dev ubuntu@$IP
dot-agent-deck connect dad-dev
```

## Hetzner Cloud

Cheap, reliable, simple API. Replace `<your-ssh-key-name>` with the key registered in Hetzner Cloud Console.

```bash
# Create the server. CX22 is the smallest tier that comfortably runs an
# agent + the workspace; bump to CX32 for parallel agents or heavier tools.
hcloud server create \
    --name dad-dev \
    --type cx22 \
    --image ubuntu-24.04 \
    --ssh-key <your-ssh-key-name>

# Wait for it, then read the public IP.
IP=$(hcloud server ip dad-dev)
```

First login as `root` (Hetzner's default for cloud images) — create a non-root user, install the toolchain, then never log in as root again:

```bash
ssh root@$IP
adduser --disabled-password --gecos "" deck
usermod -aG sudo deck
mkdir -p /home/deck/.ssh
cp ~/.ssh/authorized_keys /home/deck/.ssh/
chown -R deck:deck /home/deck/.ssh
chmod 700 /home/deck/.ssh
chmod 600 /home/deck/.ssh/authorized_keys

# Disable password auth and root login (sshd hardening).
sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config
sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
systemctl restart ssh
exit
```

Then as `deck`:

```bash
ssh deck@$IP
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
mkdir -p ~/.local/bin
sudo apt-get update
sudo apt-get install -y nodejs npm git
npm install -g @anthropic-ai/claude-code
echo 'export ANTHROPIC_API_KEY=sk-ant-...' >> ~/.bashrc
sudo loginctl enable-linger deck
exit
```

Back on the laptop:

```bash
dot-agent-deck remote add hetzner-1 deck@$IP
dot-agent-deck connect hetzner-1
```

If your ssh identity isn't at one of ssh's default search paths, pass it explicitly:

```bash
dot-agent-deck remote add hetzner-1 deck@$IP \
  --key ~/.ssh/dot-agent-deck
```

## UpCloud

UpCloud is the M0.2 reference host. The flow is identical to Hetzner once the VM exists; the differences are at the IaaS layer.

```bash
# Create the VM via the upctl CLI (or the web console). Pick whichever
# template / plan fits — anything ≥ 2 vCPU / 2 GiB RAM running Ubuntu
# 24.04 LTS is sufficient.
upctl server create \
    --hostname dad-dev \
    --plan 2xCPU-2GB \
    --os "Ubuntu Server 24.04 LTS" \
    --ssh-keys "$(cat ~/.ssh/id_ed25519.pub)" \
    --zone <your-zone>
```

Then bootstrap the VM the same way as the Hetzner recipe (non-root user, `~/.local/bin` on PATH, Node.js + agent install, `enable-linger`). The only UpCloud-specific note: cloud-init sets `root` as the default user; create a non-root user before running `remote add` so the daemon doesn't run as root.

## Bare metal / desk-side box

Any always-on Linux box on your network works — a homelab server, a Raspberry Pi 5, an old laptop. The flow is just the bootstrap section of the cloud recipes minus the IaaS step:

1. Install Ubuntu 24.04 LTS (or your distribution of choice — see [Remote Environment Requirements](remote-requirements.md) for what's required).
2. Create a non-root user, add your laptop's ssh key to its `~/.ssh/authorized_keys`.
3. Install Node.js + the agent CLI; set the agent's API key in the user's environment.
4. `sudo loginctl enable-linger $USER`.
5. From the laptop: `dot-agent-deck remote add desk-pi user@hostname.local`.

mDNS (`hostname.local`) is convenient on a home LAN. For routed access from outside the LAN, set up a tunnel (Tailscale, ZeroTier, or a port-forwarded ssh) before running `remote add`.

## What to watch for

If `remote add` fails, the deck distinguishes three failure classes; see [Remote Environments → Failure modes](remote-environments.md#failure-modes) for what each one means and how to recover.

The most common first-time failures are:

- **Wrong user.** If the cloud image's default user isn't `root`, the install steps above need to run under the right account. Check the provider's image documentation.
- **`~/.local/bin` not on `PATH`.** The remote-side install lands the binary there, but a fresh non-interactive ssh session may not source `~/.bashrc`. The deck handles this — `remote add` invokes the binary by absolute path during install — but later commands assume a login shell with `PATH` set.
- **Node.js too old.** Ubuntu's `apt` Node.js is sometimes lagging; if your agent's CLI requires a newer version, install via [NodeSource](https://github.com/nodesource/distributions) or `nvm` instead of `apt`.

## See also

- [Remote Environment Requirements](remote-requirements.md) — what a host must provide.
- [Remote Environments](remote-environments.md) — lifecycle, failure modes, hooks behavior.
