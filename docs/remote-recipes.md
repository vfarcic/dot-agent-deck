---
sidebar_position: 7.6
title: Remote Recipes
---

# Remote Recipes

Provisioning snippets that get a Linux host into a state where `dot-agent-deck remote add` will succeed. The deck itself doesn't ship a provisioner — these recipes are starting points you adapt to your environment.

For prerequisites the host must satisfy see [Remote Environment Requirements](remote-requirements.md). For lifecycle and connection semantics see [Remote Environments](remote-environments.md). The Kubernetes-as-host recipe lives in [issue #81](https://github.com/vfarcic/dot-agent-deck/issues/81) and is not yet shipped.

> **Status.** Validated on a fresh Ubuntu 24.04 LTS UpCloud VM. Other providers should work given the same OS and SSH posture, but have not been independently re-tested. If a provider's image needs different bootstrap steps, the differences are typically in the cloud-init / first-login section — the deck-side flow (`remote add`) is identical once SSH and a non-root user with the agent toolchain are in place.

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

UpCloud is the reference host. The flow is identical to Hetzner once the VM exists; the differences are at the IaaS layer.

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

## Reaching networks only your laptop can see

Sometimes the remote is *less* connected than your laptop: the laptop holds a corporate VPN that grants access to internal git and private registries, and the VM — on your home LAN, or in a restricted-egress segment — cannot reach them. Agents spawn fine and then the first `git clone` fails.

`connect` runs plain `ssh` and does not pass `-F` or otherwise override your ssh configuration, so a `Host` block in `~/.ssh/config` applies to the deck exactly as it does to any other ssh invocation. You can lend the VM your laptop's network access with a reverse tunnel, with no deck-side configuration at all.

> **Status.** The tunnel mechanism below is verified end to end (a service reachable only from the laptop's loopback, fetched from a remote through the tunnel). It has **not** been validated against a real corporate VPN — whether your internal git host is reachable this way depends on your network and your IT policy. See [issue #97](https://github.com/vfarcic/dot-agent-deck/issues/97).

**Prerequisite on the remote.** Its sshd must permit TCP forwarding — `AllowTcpForwarding yes`, which is the OpenSSH default but is disabled in some distributions' packages (Alpine's, for one) and by most hardening baselines. Check with `sshd -T | grep allowtcpforwarding`, or from the laptop with [`dot-agent-deck remote doctor <name>`](#troubleshooting-with-remote-doctor), which reads the same value over ssh and reports it as the `AllowTcpForwarding` check. If it is off, every forward fails with `remote port forwarding failed for listen port N`, which looks identical to a port collision — telling those two apart is the reason `remote doctor` exists. Note that `sshd_config` takes the *first* value it finds for a keyword, so appending `AllowTcpForwarding yes` to the end of the file does nothing if the key is already set above — rewrite the existing line.

### Reverse SOCKS proxy (recommended)

Covers HTTPS git, private package registries, and internal APIs in one rule, and preserves hostnames end to end. On the **laptop**, in `~/.ssh/config`:

```
Host deck-vm.example
    RemoteForward 1080
    ExitOnForwardFailure yes
```

`RemoteForward` with a port and **no destination** is reverse dynamic forwarding: ssh opens a SOCKS proxy on the VM's loopback at port 1080 and forwards whatever it requests out through your laptop. Requires OpenSSH 7.6 or newer on the laptop. Then on the **VM**:

```bash
git config --global http.proxy socks5h://127.0.0.1:1080
```

Use `socks5h`, not `socks5` — the `h` sends hostname resolution through the proxy, so the name is resolved at your laptop (the VM has no DNS for it) and TLS still sees the real hostname, so certificate validation and SNI work normally.

### Single host over ssh

Simpler when you only need one git host and it speaks the ssh protocol. On the **laptop**:

```
Host deck-vm.example
    RemoteForward 2222 git.company.com:22
    ExitOnForwardFailure yes
```

On the **VM**, give the tunnel a name so host-key checking stays meaningful:

```
Host company-git
    HostName 127.0.0.1
    Port 2222
    User git
    HostKeyAlias git.company.com
```

Then `git clone company-git:team/repo.git`. `HostKeyAlias` records the real host's key under its real name in `known_hosts`, instead of filing it under `[127.0.0.1]:2222` where it would collide with any other host you tunnel to that port.

> **`DynamicForward` is the wrong direction.** `DynamicForward` (and `ssh -D`) opens a SOCKS listener on *your laptop* that egresses via the remote — useful for reaching the remote's network from the laptop, which is the opposite of the problem here. Use `RemoteForward <port>` with no destination. This mistake is silent, so [`remote doctor`](#troubleshooting-with-remote-doctor) calls it out by name as the `DynamicForward` check.

### Authentication

The tunnel carries packets, not credentials. A reachable git endpoint still needs to authenticate, and the options are not equally good:

- **A deck-specific deploy key on the VM, registered with your git host — recommended.** Scoped to the repositories it needs, revocable on its own, and it leaves an audit trail distinct from your personal account. Usually needs a request to whoever administers the git host.
- **A PAT in the VM's environment.** Works for HTTPS-only flows, but the token is now durable on a machine that may be less protected than your laptop.
- **`ForwardAgent yes` — avoid.** It is the least effort and the worst trade: every agent on the VM can use your laptop's ssh-agent for as long as you are connected, with no per-agent scoping and no way to revoke one agent's access short of disconnecting.

### Limits worth knowing before you rely on this

**The tunnel lives and dies with the ssh session; your agents do not.** Agents survive detach by design — their access to laptop-tunneled resources does not. An agent that pushes while you are disconnected fails; one that clones, pulls, or fetches from a private registry blocks on bytes that will never arrive. Reads in particular are not deferrable. If a task needs the tunnel mid-flight, stay connected. On reconnect the forward comes back up with the new session.

**The `Host` block applies to every ssh the deck makes to that host** — the version probe, `remote add`, `remote upgrade`, and each automatic reconnect attempt, not just `connect`. Mostly harmless, but it interacts badly with `ExitOnForwardFailure yes`: if a previous session's listener is still held on the remote, the next connection fails to bind and exits. The deck used to report that as an unreachable host; since issue #344 it says `SSH forwarding failed` instead and points you at [`remote doctor`](#troubleshooting-with-remote-doctor), whose `ClientAliveInterval` check reads the reaping policy this paragraph asks you to set. Two mitigations, and you want both: set `ClientAliveInterval 15` / `ClientAliveCountMax 3` in the remote's `sshd_config` so it reaps dead sessions on roughly the same ~45s budget the deck's client-side keepalive uses (sshd's default is to never probe, so a listener orphaned by a laptop sleeping can linger for a long time), and do not run two `connect` sessions to the same remote with the same forward port.

**Forward ports are per-remote, not per-laptop.** Two laptops connecting to the same VM with the same `RemoteForward 1080` will collide — the second one's forward fails to bind. Give each laptop its own port. [`remote doctor`](#troubleshooting-with-remote-doctor)'s `ForwardBound` check is the one that catches this, and it is the check that separates a collision from a policy refusal.

**Three options the deck sets explicitly override your config.** `connect` passes `ConnectTimeout`, `ServerAliveInterval`, and `ServerAliveCountMax` on the command line, and ssh gives command-line `-o` precedence over the config file, so setting those in your `Host` block has no effect. Forwarding options are untouched.

### Troubleshooting with `remote doctor`

Every caveat above is something you can check from the laptop in one command:

```bash
dot-agent-deck remote doctor desk-vm
```

It resolves the name from your remote registry, runs a fixed ordered list of checks, and prints each as PASS / WARN / FAIL / UNKNOWN with the directive and the file to change. It is **read-only**: it issues no command that edits your ssh config, the remote's `sshd_config`, the registry, or anything else on the remote. Every remote command it runs is a query — `sshd -T` to read the resolved sshd policy, and a `/dev/tcp` connect to see whether the forward is actually listening. There is one thing it writes on purpose (the SOCKS greeting, below) and one thing ssh may do underneath it whatever the doctor asks (a first-use `known_hosts` entry, also below); both are stated here rather than hidden behind the word.

**One thing the doctor sends rather than reads**, and it is worth knowing about: when your `Host` block configures a reverse-*dynamic* forward — `RemoteForward <port>` with no destination, which is the recipe above — the liveness probe speaks SOCKS to it. It writes the three-byte SOCKS5 no-auth greeting `05 01 00` and looks for `05 00` back. That is the only way to tell *your* tunnel from an unrelated service that happens to hold the port: a plain connect answers "something is listening" and a squatter passes it. The greeting goes **only** to a port you declared to be SOCKS by omitting the destination. A `RemoteForward` with a concrete destination carries whatever you tunnelled — a database, an internal API — so the doctor connects to it and says nothing, and reports UNKNOWN rather than guessing (see [A concrete `RemoteForward` reports UNKNOWN, not PASS](#a-concrete-remoteforward-reports-unknown-not-pass)). Nothing the greeting does outlives the probe: no file, no configuration, no listener state — read-only here means your persistent state, not the bytes on a socket the doctor itself opened.

Read-only extends to the ssh sessions themselves. Every session the doctor opens passes `-o ClearAllForwardings=yes -o ControlMaster=no -o ControlPath=none -o PermitLocalCommand=no -o UpdateHostKeys=no`, so it creates none of the forwards your `Host` block asks for, leaves no persistent master connection behind, runs no `LocalCommand`, and does not rewrite `known_hosts` on a key rotation. Two consequences worth knowing: the doctor's own probes are immune to the forwarding problems it is diagnosing (so reachability is reported cleanly instead of cascading into UNKNOWN), and `ForwardBound` reports on **pre-existing** state rather than on a listener the doctor created for itself. The one command that does *not* get those options is `ssh -G`, which never connects and whose whole purpose is to show the forwards the others suppress.

**Those sessions also refuse to delegate your credentials**, and this is the part worth reading even if you skip the rest. `ClearAllForwardings` clears local, remote, dynamic and tunnel forwards — and nothing else, so it does not touch agent or X11 forwarding. Left alone, a `Host` block carrying `ForwardAgent yes` would hand your laptop's ssh-agent to the endpoint on *every* probe the doctor makes, before you have read the report's own advisory about it. An endpoint that has been compromised cannot pull private key material out of an agent socket, but it can *use* your key to authenticate or sign as you for as long as the probe runs — and `remote doctor` is exactly the command you run against a host you already suspect. So each session additionally passes `-o ForwardAgent=no -o ForwardX11=no -o ForwardX11Trusted=no -o GSSAPIDelegateCredentials=no -o AddKeysToAgent=no`. Nothing the doctor runs on the remote needs your credentials, your display, or a Kerberos ticket. This does not change what the report tells you: the `ForwardAgent` check reads your *configured* value out of `ssh -G`, which never gets these options, so a `Host` block with `ForwardAgent yes` still shows up as a WARN — you are told what your config does while the diagnostic itself declines to do it.

**What "read-only" does not promise: host-key verification is left exactly as you configured it.** The doctor never weakens `StrictHostKeyChecking`, and never strengthens it either. `UpdateHostKeys=no` stops the rotation-driven rewrite of `known_hosts`, but it does not stop **first use**: if your config sets `StrictHostKeyChecking accept-new` (or `no`), connecting to a host you have never connected to before appends its key to your `known_hosts`, and the doctor's session is a connection like any other. Forcing `yes` would be worse than the gap — a diagnostic is precisely what you reach for on a remote you have not connected to yet, and failing with "host key not known" would make the command useless exactly when you want it. So read the guarantee as: the doctor issues no mutation of its own and suppresses every delegation and persistence option it can without weakening verification, and ssh still does what *your* configuration tells it to do on any connection.

A healthy remote reads top to bottom, cause before symptom:

```
Diagnosing remote 'desk-vm' at deck@desk-vm.example:22 (read-only)

PASS    HostReachable        ssh connected and authenticated
PASS    RemoteBinary         the deck answered on the remote
PASS    ProtocolCompatible   the remote answered the attach handshake
PASS    RemoteForward        ssh resolved reverse-dynamic SOCKS on 1080
PASS    DynamicForward       no laptop-side SOCKS listener is configured
PASS    ExitOnForwardFailure `ExitOnForwardFailure yes` is set, so a tunnel that cannot bind aborts the session loudly
PASS    AllowTcpForwarding   the remote's sshd permits reverse (`-R`) tunnels (`AllowTcpForwarding yes`)
PASS    ClientAliveInterval  the remote's sshd probes idle sessions every 30s
PASS    ForwardBound         port 1080 answered the SOCKS5 no-auth handshake, so the listener is a SOCKS proxy, consistent with this recipe's tunnel
PASS    ForwardAgent         agent forwarding is off for this destination

Overall: PASS
```

There are **three exit codes**, so a script can tell the outcomes apart:

| Code | Meaning |
|---|---|
| `0` | Clear. Every check PASSed, or at most raised an advisory WARN. |
| `1` | A check FAILed — or the command could not run at all (unknown name, unreadable registry). |
| `2` | Incomplete. No FAIL, but at least one check is UNKNOWN. |

Both non-zero codes keep the promise that an UNKNOWN never reads as PASS: a diagnostic that reports "fine" when it could not actually look is worse than one that admits it does not know. Separating them is what makes the most common real-world outcome — a perfectly healthy tunnel on a host where `sshd -T` needs a root you do not have — a stable, scriptable `2` instead of something indistinguishable from a broken tunnel.

#### Which check covers which caveat

| Caveat | Check | What it reads |
|---|---|---|
| `AllowTcpForwarding` disabled on the remote | `AllowTcpForwarding` | `sshd -T` over ssh |
| A forward that fails silently | `ExitOnForwardFailure` | `ssh -G` |
| `DynamicForward` pointing the wrong way | `DynamicForward` | `ssh -G` |
| Nothing forwarded at all | `RemoteForward` | `ssh -G` |
| Two laptops on the same listen port | `ForwardBound` + `AllowTcpForwarding` | a loopback SOCKS5 handshake on the remote, read against the sshd policy |
| A listener orphaned by a sleeping laptop | `ClientAliveInterval` | `sshd -T` over ssh |
| `ForwardAgent yes` (advisory, never a failure) | `ForwardAgent` | `ssh -G` |
| The ordinary broken-ssh case | `HostReachable` | the deck's existing version probe |

#### `AllowTcpForwarding no` versus a port collision

These two produce **byte-identical** client errors — `Error: remote port forwarding failed for listen port 1080` and nothing else — so no amount of client-side error text can separate them. The remote's own sshd is the only witness, which is the whole reason this command exists.

When the remote refuses forwarding outright, the sshd policy is named and the unbound port is attributed to it:

```
PASS    HostReachable        ssh connected and authenticated
...
FAIL    AllowTcpForwarding   the remote's sshd refuses reverse (`-R`) tunnels (`AllowTcpForwarding no`)
        -> Set `AllowTcpForwarding yes` in the remote's sshd_config and reload sshd. sshd honours the FIRST value it finds for a keyword, so rewrite the existing line — appending a new one at the end does nothing. Alpine's openssh package and most hardening baselines ship this disabled.
PASS    ClientAliveInterval  the remote's sshd probes idle sessions every 30s
FAIL    ForwardBound         port 1080 is not bound on the remote, which the sshd policy above explains
        -> This is the remote's policy refusing the tunnel, not a busy port. Fix `AllowTcpForwarding` on the remote first, then re-run this command.
```

When the policy permits the tunnel and nothing answers on the port, the same client error produces a different report:

```
PASS    HostReachable        ssh connected and authenticated
...
PASS    AllowTcpForwarding   the remote's sshd permits reverse (`-R`) tunnels (`AllowTcpForwarding yes`)
PASS    ClientAliveInterval  the remote's sshd probes idle sessions every 30s
FAIL    ForwardBound         port 1080 is not bound on the remote, though its sshd permits the tunnel
        -> Nothing is listening there right now. If a session to this remote is up as you read this, its tunnel did not bind — that port is taken by something else, so give this laptop its own listen port or drop whatever still holds it (forward ports are per-remote, so two laptops on the same one collide). If you are not connected, expect this: the tunnel exists only while a session does, so re-run this while connected to learn anything more.
```

If instead something *does* answer on the port but is not your tunnel, the report is different again — see [A foreign service holding the port](#a-foreign-service-holding-the-port) below.

Note that `HostReachable` is PASS in both. ssh reached the host and authenticated fine; only the forward failed. Before issue #344 the deck classified this as an unreachable host and burned its reconnect budget against a network path that was never broken — which is the fourth failure mode, and the reason the doctor's own first check had to be fixed before the rest was worth building.

#### When `sshd -T` needs root

`sshd -T` typically requires root, and run as an ordinary user it either exits non-zero or prints a partial dump next to a permission complaint. Both become UNKNOWN, never PASS, and the rest of the report still renders:

```
UNKNOWN AllowTcpForwarding   could not read the remote's sshd policy
        -> `sshd -T` needs root on most hosts and is unavailable otherwise. Re-run it on the remote with elevated permission (`sudo sshd -T | grep allowtcpforwarding`), or ask whoever administers the host.
UNKNOWN ClientAliveInterval  could not read the remote's sshd keepalive policy
        -> `sshd -T` needs root on most hosts and is unavailable otherwise. Re-run it on the remote with elevated permission, or ask whoever administers the host.

Overall: UNKNOWN
```

The `ForwardBound` check degrades the same way when the remote is missing the tooling the probe needs — `bash` for the `/dev/tcp` connect, or `timeout`, `head` and `od` for the bounded reply it reads back: UNKNOWN, not a claim that the port is free and not a claim that a squatter holds it. It also refuses to probe a listen address that is not a plain IPv4 literal, IPv6 literal, or a hostname of letters, digits, `.` and `-` — a bind address ends up inside a command the remote's shell parses, so anything else is reported as UNKNOWN naming the value rather than guessed at.

#### A foreign service holding the port

The nastiest version of a collision is the one where *everything else is right*. Your `Host` block is correct, the remote permits forwarding, and some unrelated service got to port 1080 first. A probe that only checked whether the connect succeeded would call that healthy and exit 0 — which is exactly the scenario this command exists for. The SOCKS handshake is what catches it:

```
PASS    AllowTcpForwarding   the remote's sshd permits reverse (`-R`) tunnels (`AllowTcpForwarding yes`)
PASS    ClientAliveInterval  the remote's sshd probes idle sessions every 30s
FAIL    ForwardBound         port 1080 is held by something else — it answered the SOCKS5 handshake with bytes no SOCKS proxy sends
        -> A service that is not a SOCKS proxy already owns that port on the remote, so your tunnel cannot bind it. Give this laptop its own listen port, or stop whatever holds this one — forward ports are per-remote, so two laptops on the same one collide.
```

A squatter that accepts the connection and then says nothing at all — which many services do, since they speak only when spoken to in their own protocol — is the same collision seen through a quieter service, and reads the same way:

```
FAIL    ForwardBound         port 1080 is held by something that accepted the connection and then never answered the SOCKS5 handshake
        -> A live SOCKS proxy replies in microseconds over loopback, so silence means the port belongs to another service. Give this laptop its own listen port, or stop whatever holds this one — forward ports are per-remote, so two laptops on the same one collide.
```

That second case is why the probe carries its own short read deadline instead of relying on ssh's: `bash`'s `/dev/tcp` never times a read out, so a listener that stays silent would otherwise hold the probe open for the whole `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS` window.

#### A concrete `RemoteForward` reports UNKNOWN, not PASS

If your reverse forward names a destination — `RemoteForward 1080 db.internal.test:5432` rather than the destination-less reverse-dynamic form — an accepting listener is reported as UNKNOWN and the command exits 2:

```
PASS    RemoteForward        ssh resolved 1080 to db.internal.test:5432
...
UNKNOWN ForwardBound         port 1080 has a listener, but a tunnel to a concrete destination carries no greeting this probe may use to attribute it
        -> Your configuration is right and something is listening; the deck just cannot prove that something is yours. Only the reverse-dynamic form (`RemoteForward <port>` with no destination) puts a SOCKS proxy there, whose no-auth handshake the deck can safely speak. Confirm ownership on the remote yourself (`ss -ltnp`), or switch to the reverse-dynamic form this recipe uses.
```

This is not a failure being reported as a mystery, and it is not a change in behaviour — it is how the check has always worked. The reasoning is worth stating, because UNKNOWN looks like a cop-out and here it is the only honest answer. PASS and WARN both exit 0, so either would be a confident all-clear about a listener nobody verified. FAIL would be worse: for a concrete forward an accepting listener usually *is* your tunnel working, and a tool that calls a healthy setup broken is a tool people learn to ignore. The deck could only do better by writing a probe into your database's port, which it will not do. UNKNOWN says the true thing — your configuration is right, and ownership of the listener could not be established from here.

#### What `ForwardBound` does and does not tell you

Because the doctor's sessions create no forwards, this check answers a question about **pre-existing** state: what is already listening on that port on the remote? Three limits follow, and none is fixable without the doctor binding the port itself, which is the mutation it refuses:

- A verified PASS means the listener answered a SOCKS5 handshake, so it is a SOCKS proxy — which is what the recipe puts there. It does not distinguish *your* SOCKS proxy from another one on the same port, and if you deliberately run one for something else, expect a PASS.
- Nothing listening is the normal state when no session is up. Run the doctor **while connected** if you want this line to say something about your tunnel.
- Only the **first** reverse forward `ssh -G` resolved is probed. A `Host` block with several is listed in full by `RemoteForward`, but liveness is checked for one listener — enough for the single-tunnel recipe above, which is the case this was built for.

The two causes this command exists to separate do not depend on any of that: `AllowTcpForwarding` is read from the remote's own sshd and is independent of the liveness probe.

#### Reproducing the failure modes yourself

[`scripts/reverse-tunnel-validation.sh`](https://github.com/vfarcic/dot-agent-deck/blob/main/scripts/reverse-tunnel-validation.sh) is the manual, container-based validation path. It runs sshd in a container with a service reachable only from the laptop's loopback and reproduces every failure mode above deterministically, which is how the indistinguishable-error case was discovered in the first place. Two false-pass traps it guards against, both of which cost real debugging time: an auth failure exits 255 exactly like a forward collision, and a wholesale forwarding refusal produces the same error text as a port collision — so a collision assertion has to verify that the *first* session actually bound before drawing any conclusion.

## What to watch for

If `remote add` fails, the deck distinguishes three failure classes; see [Remote Environments → Failure modes](remote-environments.md#failure-modes) for what each one means and how to recover.

The most common first-time failures are:

- **Wrong user.** If the cloud image's default user isn't `root`, the install steps above need to run under the right account. Check the provider's image documentation.
- **`~/.local/bin` not on `PATH`.** The remote-side install lands the binary there, but a fresh non-interactive ssh session may not source `~/.bashrc`. The deck handles this — `remote add` invokes the binary by absolute path during install — but later commands assume a login shell with `PATH` set.
- **Node.js too old.** Ubuntu's `apt` Node.js is sometimes lagging; if your agent's CLI requires a newer version, install via [NodeSource](https://github.com/nodesource/distributions) or `nvm` instead of `apt`.

## See also

- [Remote Environment Requirements](remote-requirements.md) — what a host must provide.
- [Remote Environments](remote-environments.md) — lifecycle, failure modes, hooks behavior.
