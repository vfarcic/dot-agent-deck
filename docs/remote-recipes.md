# Remote Recipes

Commands for getting a host ready for `dot-agent-deck remote add`, registering it, and lending it your laptop's network through an ssh tunnel. [Remote Environment Requirements](remote-requirements.md) is the full list of what a host must provide; [Remote Environments](remote-environments.md) covers connecting, detaching and upgrading.

Adapt these to your environment: the deck has no provisioner, and nothing in `remote add` depends on the cloud, hypervisor or distribution the host runs on.

## Getting a machine

- **One you already have**: a home server, a Raspberry Pi 5, a spare Mac, an old laptop left plugged in. Go straight to bootstrapping.
- **A local VM**, for example with Multipass:

  ```bash
  multipass launch 24.04 --name dad-dev --cpus 2 --memory 2G --disk 20G
  multipass shell dad-dev
  ```

- **A cloud VM** from any provider: the smallest instance that meets [the hardware requirements](remote-requirements.md#hardware), running a glibc-based Linux ([Which Linux distribution](remote-requirements.md#which-linux-distribution)), with your ssh public key installed. Note its address.

## Bootstrapping a Linux host

These commands use Debian/Ubuntu package names; substitute your distribution's package manager.

**1. As `root` on the host**, install the packages the deck and your agent need. This example installs Claude Code; for another agent, see the install table in [Software on the host](remote-requirements.md#software-on-the-host):

```bash
apt-get update
apt-get install -y curl git nodejs npm
npm install -g @anthropic-ai/claude-code
```

If you log in as a non-root user with `sudo`, prefix each command with `sudo` and skip to step 3.

**2. Create a non-root user** and turn off root and password logins. Agents run with the daemon's account, so under root they have full control of the host. Still as root:

```bash
adduser --disabled-password --gecos "" deck
mkdir -p /home/deck/.ssh
cp ~/.ssh/authorized_keys /home/deck/.ssh/
chown -R deck:deck /home/deck/.ssh
chmod 700 /home/deck/.ssh
chmod 600 /home/deck/.ssh/authorized_keys
```

From your laptop, check that `ssh deck@<address> true` works. Only then, as root on the host:

```bash
sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config
sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
systemctl restart ssh
```

The ssh service is `ssh` on Debian and Ubuntu and `sshd` on most other distributions. The `deck` user has no password, so it cannot use `sudo`; do later system-wide installs as root before you turn off root login, or give the user a password.

**3. As the user the deck will run as**, put `~/.local/bin` on `PATH` and log your agent in:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
mkdir -p ~/.local/bin
```

If your agent needs a newer Node.js than the distribution ships, install Node.js from [NodeSource](https://github.com/nodesource/distributions) or with `nvm` instead. Log the agent in on the host (for Claude Code, run `claude` once and follow its login), or give the daemon's user its API key; see [Credentials and one host per project](remote-requirements.md#credentials-and-one-host-per-project).

**4. From your laptop**, accept the host key, then register and connect:

```bash
ssh deck@<address> true
dot-agent-deck remote add dad-dev deck@<address>
dot-agent-deck connect dad-dev
```

If your key is not one ssh finds by default, add `--key ~/.ssh/<key>` to `remote add`; `connect` reuses it.

Check it worked: `remote add` ends with `Added remote 'dad-dev' …`, `dot-agent-deck remote list` shows the entry, and `connect` opens an empty dashboard where `Ctrl+N` starts an agent.

**5. Optional:** run the daemon as a systemd user service so it restarts with the host; see [Keep the daemon running](remote-requirements.md#keep-the-daemon-running).

On a home network, the host's mDNS name (`hostname.local`) works as the address. To reach the host from outside that network, set up a VPN such as Tailscale or ZeroTier, or a forwarded ssh port, before running `remote add`.

## Bootstrapping a macOS host

1. On the Mac, turn on **Remote Login** (System Settings → General → Sharing).
2. Install your agent CLI on the Mac. macOS includes `curl`, which `remote add` uses to download the deck.
3. Stop the Mac from sleeping (`pmset`, see [macOS as a remote host](remote-requirements.md#macos-as-a-remote-host)).
4. From your laptop: `ssh <user>@<mac>.local true`, then `dot-agent-deck remote add my-mac <user>@<mac>.local` and `dot-agent-deck connect my-mac`.
5. In the first Claude Code pane, run `/login` once.

## Reaching networks only your laptop can see

Sometimes the host reaches less than your laptop does: your laptop is on a corporate VPN that reaches an internal git server or package registry, and the host is not. Agents start fine, then the first `git clone` fails.

`connect` runs your system `ssh` and applies your `~/.ssh/config`, so a `Host` block there can lend the host your laptop's network through a reverse tunnel, with nothing to configure in the deck. This works while you are connected; see [Limits](#limits).

This has been tested with a service reachable only from the laptop, fetched from the host through the tunnel. It has not been tested against a real corporate VPN; whether your internal hosts are reachable this way depends on your network and its policy.

**On the host**, sshd must allow TCP forwarding (`AllowTcpForwarding yes`). That is OpenSSH's default, but some distribution packages (Alpine's among them) and most hardening baselines turn it off. Check with `sudo sshd -T | grep allowtcpforwarding` on the host, or with `dot-agent-deck remote doctor <name>` from your laptop. sshd uses the **first** value it finds for a keyword, so edit an existing `AllowTcpForwarding` line rather than appending a new one, then reload sshd.

### Reverse SOCKS proxy (recommended)

One rule covers HTTPS git, private package registries and internal APIs, and keeps host names intact. On your **laptop**, in `~/.ssh/config`, using the same host name you registered:

```
Host deck-vm.example
    RemoteForward 1080
    ExitOnForwardFailure yes
```

`RemoteForward` with a port and **no destination** opens a SOCKS proxy on the host's loopback at port 1080 that sends traffic out through your laptop. It needs OpenSSH 7.6 or newer on your laptop. `ExitOnForwardFailure yes` makes a session whose tunnel cannot be set up fail instead of connecting without it. Then, **on the host**:

```bash
git config --global http.proxy socks5h://127.0.0.1:1080
```

Use `socks5h`, not `socks5`: with `h`, host names are resolved at your laptop, which is the side that can resolve them, and TLS still sees the real host name.

Check it: connect with `dot-agent-deck connect <name>`, then in a pane on the host run `git ls-remote https://<internal-git-host>/<repo>.git`; from your laptop, `dot-agent-deck remote doctor <name>` should show `ForwardBound` as `PASS` while that session is open.

### Single host over ssh

When you need one git server that speaks ssh. On your **laptop**:

```
Host deck-vm.example
    RemoteForward 2222 git.company.com:22
    ExitOnForwardFailure yes
```

**On the host**, in `~/.ssh/config`, give the tunnel a name:

```
Host company-git
    HostName 127.0.0.1
    Port 2222
    User git
    HostKeyAlias git.company.com
```

Then `git clone company-git:team/repo.git`. `HostKeyAlias` stores the real server's key under its real name in `known_hosts`, so it does not collide with another server you tunnel through the same local port.

`DynamicForward` (and `ssh -D`) is the wrong direction for this: it opens a SOCKS listener on your laptop that exits through the host. Use `RemoteForward <port>` with no destination. `remote doctor`'s `DynamicForward` check reports this mistake.

### Authentication through the tunnel

The tunnel carries traffic, not credentials; the internal server still needs to authenticate the host.

- **A deploy key on the host, registered with your git server (recommended).** It can be limited to the repositories it needs and revoked on its own.
- **A personal access token in the host's environment.** Works for HTTPS, but leaves a long-lived token on the host.
- **`ForwardAgent yes` (avoid).** Every agent on the host can use your laptop's ssh-agent for as long as you are connected, with no way to limit or revoke one agent's access. `remote doctor` warns about it.

### Limits

- **The tunnel exists only while a `connect` session is open; your agents keep running without it.** While you are disconnected, a push through the tunnel fails and a clone, fetch or package download through it hangs. Stay connected while a task needs the tunnel. Each new session brings the tunnel back.
- **The `Host` block applies to every ssh the deck makes to that host**: `remote add`, `remote upgrade`, the checks before each `connect`, and each reconnect. With `ExitOnForwardFailure yes`, if a previous session's listener is still held on the host, the next session cannot bind it and `connect` fails with `SSH forwarding failed for remote …`. Set `ClientAliveInterval 15` and `ClientAliveCountMax 3` in the host's `sshd_config`, so sshd drops a dead session in about 45 seconds (sshd's default never checks), and do not run two `connect` sessions to one host with the same forward port.
- **Forward ports are per host.** Two laptops using `RemoteForward 1080` on the same host collide: the second one's forward fails. Give each laptop its own port.
- **`connect` sets `ConnectTimeout`, `ServerAliveInterval` and `ServerAliveCountMax` on the ssh command line**, which overrides those settings in your `Host` block. Forwarding options are not touched.

### Troubleshooting with `remote doctor`

```bash
dot-agent-deck remote doctor deck-vm
```

`remote doctor` looks the name up in your registry, runs ten checks in a fixed order, and prints each as `PASS`, `WARN`, `FAIL` or `UNKNOWN`, with the fix under any that is not `PASS`. It changes nothing: it edits no ssh config, `sshd_config`, registry entry or file on the host, and its own ssh sessions set up none of your forwards and do not forward your ssh-agent. It does open ssh connections under your own host-key settings, so with `StrictHostKeyChecking accept-new` (or `no`) a first connection to a new host adds its key to `known_hosts`, as any ssh would. For a reverse-dynamic forward (`RemoteForward <port>` with no destination), the `ForwardBound` check sends the three-byte SOCKS5 greeting to that port on the host to confirm the listener is a SOCKS proxy.

A healthy host with the reverse SOCKS recipe, checked while connected:

```
Diagnosing remote 'deck-vm' at deck@deck-vm.example:22 (read-only)

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

Exit status:

| Status | Meaning |
|---|---|
| `0` | Every check is `PASS` or `WARN`. |
| `1` | At least one check is `FAIL`, or the command could not run (unknown name, unreadable registry). |
| `2` | No `FAIL`, but at least one check is `UNKNOWN`. |

On a host **without** a reverse tunnel, `RemoteForward` is `FAIL`, `ExitOnForwardFailure` is `WARN` and `ForwardBound` is `UNKNOWN`, so the command exits `1`. For such a host only the first three checks matter.

| Check | What it reads | `FAIL` / `WARN` / `UNKNOWN` and what to do |
|---|---|---|
| `HostReachable` | an ssh session to the host | `FAIL`: ssh could not log in. Make `ssh <target> true` work without a prompt. `UNKNOWN`: `ssh` could not be started on your laptop. |
| `RemoteBinary` | `<binary> --version` on the host | `FAIL`: `dot-agent-deck remote upgrade <name>`. `UNKNOWN`: an earlier failure stopped it; fix that first. |
| `ProtocolCompatible` | `<binary> daemon hello` on the host | `FAIL`: `dot-agent-deck remote upgrade <name>`; `connect` refuses the host until this passes. |
| `RemoteForward` | `ssh -G` on your laptop | `FAIL`: no `RemoteForward` for this host. Add `RemoteForward 1080` to its `Host` block, or ignore this line if you do not use a tunnel. |
| `DynamicForward` | `ssh -G` | `FAIL` (or `WARN` next to a `RemoteForward`): a `DynamicForward` points the wrong way. Replace it with `RemoteForward <port>`. |
| `ExitOnForwardFailure` | `ssh -G` | `FAIL` with a tunnel, `WARN` without: add `ExitOnForwardFailure yes`. |
| `AllowTcpForwarding` | `sshd -T` on the host | `FAIL`: the host's sshd refuses reverse tunnels; set `AllowTcpForwarding yes` (edit the existing line) and reload sshd. `UNKNOWN`: `sshd -T` needs root; run `sudo sshd -T \| grep allowtcpforwarding` on the host. |
| `ClientAliveInterval` | `sshd -T` on the host | `WARN`: `ClientAliveInterval 0`; set `ClientAliveInterval 15` and `ClientAliveCountMax 3`. `UNKNOWN`: as above. |
| `ForwardBound` | a loopback connection to the forward's port on the host | See [Reading `ForwardBound`](#reading-forwardbound). |
| `ForwardAgent` | `ssh -G` | `WARN`: `ForwardAgent yes`. Prefer a deploy key; see [Authentication through the tunnel](#authentication-through-the-tunnel). |

#### `AllowTcpForwarding no` versus a port already in use

Both produce the same ssh error on your laptop, `remote port forwarding failed for listen port 1080`. The doctor tells them apart from the host's side.

When the host's sshd refuses forwarding:

```
FAIL    AllowTcpForwarding   the remote's sshd refuses reverse (`-R`) tunnels (`AllowTcpForwarding no`)
        -> Set `AllowTcpForwarding yes` in the remote's sshd_config and reload sshd. ...
FAIL    ForwardBound         port 1080 is not bound on the remote, which the sshd policy above explains
```

When sshd allows forwarding but nothing is listening on the port:

```
PASS    AllowTcpForwarding   the remote's sshd permits reverse (`-R`) tunnels (`AllowTcpForwarding yes`)
FAIL    ForwardBound         port 1080 is not bound on the remote, though its sshd permits the tunnel
```

If you are connected, your tunnel did not bind, usually because something else holds the port: pick another port for this laptop or stop whatever holds it. If you are not connected, this line is expected, because the tunnel exists only during a session; run the doctor again while connected.

#### Reading `ForwardBound`

`ForwardBound` looks at what is already listening on the host; the doctor never creates the forward itself. It probes only the first `RemoteForward` that `ssh -G` resolves.

| Result | Meaning | What to do |
|---|---|---|
| `PASS` … answered the SOCKS5 no-auth handshake | A SOCKS proxy is listening, as the recipe expects. It could be another SOCKS proxy on the same port. | Nothing. |
| `FAIL` … is not bound on the remote | Nothing listens there. | See the section above. |
| `FAIL` … is held by something else | Another service answered with bytes a SOCKS proxy does not send. | Use another port for this laptop, or stop that service. |
| `FAIL` … accepted the connection and then never answered | Another service holds the port and stayed silent. | Same as above. |
| `UNKNOWN` … a tunnel to a concrete destination carries no greeting | Your `RemoteForward` names a destination (`RemoteForward 1080 db.internal:5432`), so the doctor will not send anything to it and cannot tell whose listener it is. | Check on the host with `ss -ltnp`, or use the destination-less form. |
| `UNKNOWN` … the live bind state on the remote was not observed | No tunnel is configured, or the host lacks `bash`, `timeout`, `head` or `od`. | Install them, or ignore this line if you use no tunnel. |
| `UNKNOWN` … not a shape this probe will target | The listen address is not an IPv4 literal, a non-link-local IPv6 literal, or a host name of letters, digits, `.` and `-`. | Rewrite the `RemoteForward` listen address in `~/.ssh/config`. |

## Common first-time failures

`remote add` stops at the first failing step and prints why; [Remote Environments → Failure modes](remote-environments.md#failure-modes) lists the messages and fixes. The ones most often seen on a fresh host:

- **`ssh failed: host key not yet trusted for …`**: run `ssh <target> true` once and accept the key.
- **`ssh authentication to … failed`**: wrong user, or a key that needs a passphrase. Check the image's default user in its documentation, pass `--key`, or `ssh-add` the key.
- **`Failed to download dot-agent-deck …`**: the host has no `curl` or no outbound HTTPS to GitHub.
- **An agent command not found in a pane**: the agent is not on the `PATH` your login shell sets on the host. Check with `ssh <target> '$SHELL -ilc "command -v claude"'`.
