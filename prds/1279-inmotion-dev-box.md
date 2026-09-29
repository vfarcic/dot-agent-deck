# PRD #1279: On-demand InMotion Cloud dev box

**Status**: In progress — M1–M4 done; M5 awaits the maintainer's agent logins.
**Priority**: Medium
**Created**: 2026-09-24
**Issue**: [#1279](https://github.com/vfarcic/dot-agent-deck/issues/1279)

## Problem Statement

Agent-deck development runs on one hand-built Linux box. Everything that makes it work — the apt packages, the devbox toolchain, the agent CLIs, docker, Tailscale, the monitoring timers, the kernel and limit tuning — was accumulated by hand, and none of it is written down as something that can be replayed. There is no way to stand up a second, equivalent box when one is needed (more parallel agents, a clean environment, a machine to hand to a remote session) and remove it again when it is not.

The only provisioning script in the tree, `examples/provision-upcloud-vm.sh`, does not help. It was written for PRD #76, has not changed since `4b81c066`, and installs a toolchain the dev box no longer resembles — Node 20 from NodeSource, Claude Code through `npm`, and nothing of devbox, docker or the Rust toolchain. Its install steps also run inside cloud-init, which executes once on first boot and cannot be re-run to fix a partial failure or apply a change.

## Solution Overview

Two scripts with a hard boundary between them:

1. **`scripts/box/inmotion.sh`** — the only part that knows about InMotion Cloud. It creates and removes the VM and its surrounding resources through the OpenStack CLI. Its cloud-init is minimal: a user, SSH hardening, and the data volume mounted at `/home`. Nothing else.
2. **`scripts/box/bootstrap.sh`** — knows nothing about any cloud. Run over SSH against any Ubuntu box, it installs the dev box's capabilities, and re-running it brings the box up to date. It ends by printing which agents still need logging in.

`examples/provision-upcloud-vm.sh` is refactored onto the same `bootstrap.sh`, so its install list is deleted rather than maintained twice.

The install list comes from an inventory of the current dev box taken on 2026-09-24 (recorded under [The inventory](#the-inventory)), **not** from the UpCloud script.

## Decisions

All made with the maintainer on 2026-09-24.

| # | Decision | Reason |
| --- | --- | --- |
| 1 | **Ubuntu 26.04 LTS (server)** on every box, not NixOS | Matches the dev box. NixOS was considered and rejected. |
| 2 | **A bash script, not Ansible** | With the dev box excluded (decision 3) there is no multi-host inventory to manage, and the remaining list is short. |
| 3 | **The current dev box does not run these scripts** until it is wiped and rebuilt | Running them over a hand-built box would conflict with what is already there. Consequence: parity with the dev box is a one-time snapshot; anything added to it by hand from now on must also be added to `bootstrap.sh`. |
| 4 | **Three install channels**: apt for services and system pieces; `devbox global` for CLI tools wherever devbox has them; native installers for agent CLIs | Agents self-update and ship faster than nixpkgs; services need root daemons and systemd units. |
| 5 | **Authentication is a printed checklist**, never baked in | No credential is part of the setup definition, so it can live in a public repo. |
| 6 | **~~Tailscale only~~ → SSH and mosh from the operator's IP only** (revised the same day) | Tailscale was the first choice, but the maintainer's Tailscale trial has expired and cannot add nodes. The box gets a public IP; its security group allows tcp/22 and udp/60000–61000 from `--allow-cidr` (default: the operator's current public IPv4) and nothing else; `allow-ip` re-points it when that IP changes. sshd is key-only, no root, no passwords. |
| 7 | **`down` deletes the VM and keeps a data volume; `destroy` deletes everything** | Re-creating is cheap; repos, `target/` caches and agent logins survive. |
| 8 | **Flavor `m7i.4xlarge`** (16 vCPU, 64 GB RAM, 1 TB root) by default, overridable | The only flavor matching the dev box's 16 cores. Paid only while the box exists. |
| 9 | **Data volume: `NVME` type, ~~300 GB~~ → 2000 GB** by default, overridable (revised the same day) | Cargo builds and nextest are I/O-heavy; the dev box runs on an NVMe SSD. 300 GB was an under-estimate: each dispatched worktree builds its own `--features e2e` `target/` (measured 25–95 GB), and six concurrent units filled 300 GB in about two hours. The maintainer asked for more than enough for many agents (InMotion credits cover it). `grow` extends a live volume with no downtime. |
| 10 | **Upload Ubuntu's official 26.04 server cloud image** (`resolute-server-cloudimg-amd64.img`) once, reuse it after | InMotion offers only 20.04/22.04/24.04. |
| 11 | **Not installed**: `aether`, `dot-ai` | Removed from the dev box on 2026-09-24 as well; the project-local `dot-ai` skill went in #1277. |
| 12 | **Installed**: `mem-sampler`, auditd, `sem`, `devin`. **Dropped**: `boot-canary`, `heartbeat` (revised the same day) | `boot-canary` guarded this machine's wireless card through one kernel upgrade and had already disarmed itself; `heartbeat` judged health by Tailscale and treats silence as the alarm, so every intentional `down` would page. "We can always add more later" — the priority is that the deck works on the box. `tailscale-watchdog` and tailscale went with decision 6. |
| 15 | **Agent runtimes from apt `nodejs`/`npm`; codex and pi as global npm packages under `/usr/local`** — as on the dev box | `/usr/local/bin` is on every PATH, including the non-interactive ssh session the deck daemon starts from. Node is therefore **not** in `devbox global`: a devbox `node` would shadow apt's in interactive shells and point `npm -g` at the read-only nix store. |
| 16 | **Agent config seeding is deferred** | Not needed for the deck to work; the agents write their own config on first login. |
| 14 | **The script reaches the box with its own key**, `~/.ssh/dad-box_ed25519`, created on first use, **plus** every key in the operator machine's `~/.ssh/authorized_keys` | The dev box has no outbound SSH key of its own — `~/.ssh` holds only `authorized_keys` (`gh:vfarcic` and `dot-agent-deck`). Copying those means whoever can reach the dev box can reach the new ones. |
| 13 | **Work happens in a worktree**, `../dot-agent-deck-1279` | Another agent may be working in the main checkout. |

## Scope

### In Scope

- `scripts/box/inmotion.sh` with `up`, `down`, `destroy`, `ssh`, `ip` and `status`, idempotent on re-run.
- `scripts/box/bootstrap.sh`, re-runnable, provider-agnostic, optionally cloning a repo and running its `devbox install`.
- A checked-in `devbox global` definition for the box's CLI tools.
- Importing the dev box's timer scripts and units (currently only in `/usr/local/bin` and `/etc/systemd/system` on that box) into the repo, reviewed for anything machine- or secret-specific.
- OpenStack access through `.env.vals.yaml`, and `openstackclient` pinned in `devbox.json`.
- Refactoring `examples/provision-upcloud-vm.sh` onto `bootstrap.sh`.
- Developer docs under `docs/develop/` (rule 11), linked from `CONTRIBUTING.md`.

### Out of Scope

- Running the scripts on the current dev box (decision 3).
- Any change to the deck itself — `dot-agent-deck remote add` already works against any Linux host, and this PRD only produces hosts.
- The vendored `dot-ai-*` skills that call the removed `dot-ai` CLI — upstream's business (rule 13).
- Playwright's WebKit runtime dependencies (the fonts, GStreamer and ~30 `lib*` packages on the dev box). They are what `pnpm exec playwright install --with-deps` installs for the desktop's browser tests (`ci.yml:359`), so they belong to the project, not the box.
- Multiple regions or providers beyond InMotion and the refactored UpCloud example.

## Technical Approach

### Cloud access

InMotion Cloud is OpenStack. Authentication is an **application credential** (`dot`, ID `e463c4ab…`, roles `member` + `reader`, restricted) whose ID and secret live in GCP Secret Manager as `inmotion-id` and `inmotion-secret`, readable by the `dot-agent-deck-secrets` service account that `vals` uses. `.env.vals.yaml` gains:

```yaml
OS_AUTH_TYPE: v3applicationcredential
OS_AUTH_URL: https://iad4.inmotioncloud.net:5000/v3
OS_REGION_NAME: iad4
OS_APPLICATION_CREDENTIAL_ID: ref+gcpsecrets://vfarcic/inmotion-id
OS_APPLICATION_CREDENTIAL_SECRET: ref+gcpsecrets://vfarcic/inmotion-secret
```

With those set, the `openstack` CLI needs no `clouds.yaml`. Verified on 2026-09-24: `openstack token issue` succeeds against project `c10f1329…`.

What the project offers, as listed on 2026-09-24: flavors `m7i.medium` through `m7i.8xlarge`; images Ubuntu 20.04/22.04/24.04; external networks `External` and `ext` and no tenant network; volume types `NVME` and `HDD`; unlimited quotas (`-1`) for instances, cores, RAM, volumes, gigabytes, networks, routers and ports; no existing servers.

**Neither external network can host a server directly.** Both are `router:external`, owned by other projects and not shared with this one; `External` is the default (`is_default: true`, MTU 1500), `ext` is not (MTU 1492). So `up` creates a private network, a subnet and a router whose gateway is on `External`, and the box's public address is a floating IP from `External`.

### `inmotion.sh up`

1. **Image**: if no `ubuntu-26.04-dad` image exists, download the 26.04 server cloud image, verify it against Ubuntu's `SHA256SUMS`, and upload it.
2. **Network**: a private network, subnet and router with its gateway on `External`, created once and reused.
3. **Keypair and security group**: a keypair from the script's own key (decision 14) and a security group allowing only SSH and mosh from `--allow-cidr`.
4. **Volume**: create the `NVME` data volume if it does not exist; reuse it if it does.
5. **Server**: create it on the private network with the minimal cloud-init and the volume attached at boot (so cloud-init can mount it before creating the user), then attach a floating IP and wait for SSH and `cloud-init status --wait`.
6. **Hand-off**: run `bootstrap.sh` over SSH, then print the auth checklist it produces.

Every resource carries a name prefix derived from `--name`, so `down` and `destroy` find exactly what `up` created and nothing else.

### Access

Decision 6, as revised: a floating IP on `External`, and a security group whose only ingress is tcp/22 and udp/60000–61000 (mosh) from `--allow-cidr`. `up` detects the operator's public IPv4 through `api.ipify.org` when no CIDR is given; `allow-ip` replaces the rules when it changes. Each box keeps its own `known_hosts` under `~/.local/state/dot-agent-deck/box/<name>/`, cleared on every rebuild because the host key is new each time.

The first design joined a tailnet at first boot and exposed nothing; it was dropped because the maintainer's Tailscale trial has expired. If a tailnet becomes available again, that design is still the stronger one — the network already needs no public IP for outbound traffic, so only the floating IP and the security-group rules would go.

### The data volume

Holds the operator's home directory, so repos, worktrees, `target/` caches and agent logins survive `down`/`up`. It is attached **at server creation** (`--block-device … boot_index=-1,delete_on_termination=false`), so it is present on first boot. A `bootcmd` formats it only when `blkid -p` exits 2 (no signature at all — any other answer, errors included, leaves it alone), and cloud-init's `mounts` puts `LABEL=dad-home` on `/home`. Both run before `users_groups`, so the user is created on the volume and keeps UID 1000 across rebuilds. **cloud-init's own `fs_setup` is deliberately not used** — see the M2 Work Log entry. `down` stops the server before deleting it, so nothing unflushed is lost.

### `bootstrap.sh`

Ordered so each layer can assume the one before it:

1. **apt**: docker (Docker's own repo, plus the `docker` group), auditd, build-essential (the project's cargo links with the system C compiler — `devbox.json` carries none), and whatever the cloud image lacks of openssh and chrony.
2. **System settings**: `vm.swappiness=10`, `vm.dirty_ratio=10`, `vm.dirty_background_ratio=5`; an 8 GB swap file; `nofile` 524288; linger for the operator.
3. **Timers**: the imported `heartbeat`, `mem-sampler` and `boot-canary` units, plus `scripts/install-reaper-timer.sh` for the orphan reaper.
4. **nix + devbox**, then `devbox global install` from `scripts/box/devbox-global.json`. **PATH** is set three ways, because the deck daemon starts from a non-interactive ssh session and every agent inherits its PATH: a marked block at the **top** of `~/.bashrc` (Ubuntu's `.bashrc` returns early for non-interactive shells, so anything below that guard never runs), `devbox global shellenv` at the bottom for interactive shells, and `~/.config/environment.d/999-dad-box.conf` plus `systemctl --user set-environment` for systemd user units. The `999-` prefix matters: Ubuntu's `/usr/lib/environment.d/99-environment.conf` resets PATH from `/etc/environment`, and a `10-` file was silently overridden.
5. **Agents**: claude, opencode and devin through their own installers, codex and pi through `sudo npm -g` (decision 15), `sem`; `dot-agent-deck` from the latest release; then `dot-agent-deck hooks install --agent X` for `claude-code`, `opencode`, `codex` and `devin` — the command defaults to Claude Code alone, and so does `remote add`. Pi needs no step: the deck materializes its extension when it spawns a Pi pane.
6. **Agent config**: deferred (decision 16).
7. **Project** (with `--repo`): clone it onto the data volume and run `devbox install`.
8. **Auth checklist**: for each agent, detect whether it is already logged in and list only what is missing.

Re-running must be safe at every step: each one checks current state before changing it.

### The inventory

Taken read-only on the dev box on 2026-09-24. Classification:

| Channel | Items |
| --- | --- |
| apt | docker-ce + buildx/compose plugins, containerd, auditd, build-essential, nodejs, npm |
| `devbox global` | git, gh, jq, curl, rsync, unzip, mosh, xvfb-run, go, uv, rustup + cargo-audit/cargo-cross/rust-analyzer, kcl |
| Native installer | claude (`~/.local/bin`), opencode (`~/.opencode/bin`), devin, sem, dot-agent-deck; codex and pi via `npm -g` |
| Imported from the box | `mem-sampler` script and units (`scripts/box/system/`) |
| System settings | sysctl values, swap, limits, linger, `docker` group |
| Skipped — no tailnet (decision 6) | tailscale, `tailscale-watchdog` |
| Skipped — decision 12 | `boot-canary`, `heartbeat`; also `dad-debug`/`dad-watchdog` in `~/.local/bin`, incident-debugging helpers |
| Skipped — this machine's hardware | grub/EFI, nvme-cli, wpasupplicant, ModemManager, gpu-manager, vmtoolsd |
| Skipped — duplicates | apt `nodejs`/`npm` and `~/.local/lib/nodejs` (devbox provides node), apt `gh`, five rustup toolchains |
| Skipped — project-owned | Tauri/GTK/WebKit `lib*` (devbox's `tauri-deps`, #780); Playwright's fonts and GStreamer (`ci.yml:359`) |
| Auth checklist | claude (subscription + MCP logins), codex (ChatGPT), opencode (OpenAI + OpenRouter), pi (OpenAI Codex), gh, gcloud (for `vals`), devin |

### Testing

No TUI surface changes, so rule 4 does not apply. Validation is:

- `bash -n` and shellcheck on both scripts.
- A full real run on InMotion (M5): `up` → bootstrap → clone this repo → `devbox install` → `cargo test-fast` green on the box → an agent works in a deck pane → `down` → `up` (volume contents survive) → `destroy` (the project lists no leftover resources).
- Re-running `bootstrap.sh` on a finished box changes nothing.

## Success Criteria

- `scripts/box/inmotion.sh up` on an empty project produces a box reachable only by SSH from the allowed CIDRs, bootstrapped, with `cargo test-fast` passing on this repo.
- The auth checklist names exactly the agents not yet logged in.
- `down` then `up` preserves the data volume's contents; `destroy` leaves no resource behind.
- Re-running `up` or `bootstrap.sh` on a finished box is a no-op.
- The UpCloud example installs nothing of its own and delegates to `bootstrap.sh`.

## Milestones

- [x] **M1 — Cloud access wired.** `openstackclient` pinned in `devbox.json`; the `OS_*` entries in `.env.vals.yaml`; `openstack token issue` works from a devbox shell with `USE_VALS=1`.
- [x] **M2 — `inmotion.sh` lifecycle.** `up`/`down`/`destroy`/`ssh`/`ip`/`status` working and idempotent: image upload, network and router, SSH/mosh-only security group, NVME volume mounted at `/home`, floating IP.
- [x] **M3 — `bootstrap.sh` system layer.** apt, system settings, docker, linger, imported timers — re-runnable.
- [x] **M4 — `bootstrap.sh` user layer.** nix + devbox, `devbox global`, agents, `dot-agent-deck` + hooks, `--repo`, auth checklist. (Config seeding deferred — decision 16.)
- [ ] **M5 — End-to-end validation on InMotion.** The full run in [Testing](#testing), recorded in the Work Log with timings.
- [ ] **M6 — UpCloud example refactored** onto `bootstrap.sh`, its own install steps deleted.
- [ ] **M7 — Docs.** `docs/develop/` page covering prerequisites, commands, the auth checklist, costs and teardown; linked from `CONTRIBUTING.md`.

## Risks

- **The uploaded 26.04 image may not boot cleanly on InMotion** (virtio, config drive vs metadata service). Mitigation: validate early in M2; fall back to InMotion's 24.04 image and record the difference.
- **A public SSH port.** Mitigation: reachable only from `--allow-cidr`; key-only sshd with no root login. Cost: a changed operator IP locks the operator out until `allow-ip` runs.
- **Secrets in cloud-init user-data.** Mitigation: user-data carries only public keys; nothing secret goes in it.
- **Drift from the dev box** (decision 3). Mitigation: stated in the docs; the inventory is recorded here so a later diff has a baseline.
- **Agent installers change URLs or behaviour.** Mitigation: each installer is one isolated step; the auth checklist makes a missing agent visible.
- **The imported timer scripts may embed machine-specific values** (the heartbeat URL is set by a separate `heartbeat-set-url.sh`). Mitigation: review each before importing; anything secret becomes a `vals` reference.

## Open Questions

1. ~~Which external network should servers attach to?~~ Neither directly — see [Cloud access](#cloud-access). The router's gateway goes on `External`.
2. ~~The Tailscale auth key~~ — moot: no tailnet (decision 6).
3. Is an UpCloud account still available to validate M6 end to end? GCP Secret Manager holds an `upcloud-token`, which suggests so; to be confirmed before M6.
4. Does refactoring `examples/provision-upcloud-vm.sh` warrant a changelog fragment (rule 19)? The example is in the public repo but not on the docs site.
5. How does `devin` authenticate? Not inspected in the inventory.

## Work Log

### 2026-09-24 — Created

Planned with the maintainer in one session. The inventory above was taken read-only on the dev box. The same session removed `aether` and `dot-ai` from the dev box, filed #1277 / PR #1278 to remove the project-local `dot-ai` skill, granted the `vals` service account read access to `inmotion-id` and `inmotion-secret`, and verified the application credential against `https://iad4.inmotioncloud.net:5000/v3`. An earlier claim in that session that `/tmp` was not a tmpfs on the dev box was wrong — it is a 14 GB tmpfs, and CLAUDE.md rule 14 stands.

### 2026-09-24 — M1: cloud access wired

`openstackclient` 10.0.0 pinned in `devbox.json` beside `upcloud-cli` (nixpkgs' base client covers compute, network, image, volume and identity). `.env.vals.yaml` gained the seven `OS_*` entries shown under [Cloud access](#cloud-access), `OS_INTERFACE` and `OS_IDENTITY_API_VERSION` included. Verified from `USE_VALS=1 devbox run`: `openstack token issue` returns project `c10f1329…`, `openstack server list` returns nothing, and all seven variables are exported. Without `USE_VALS` nothing changes — the entries are read only by the existing `vals env` line in `init_hook`.

The network inspection that followed settled open question 1: both external networks are other projects' and not shared, so the box sits on its own private network behind a router, and never has a public address.

### 2026-09-24 — M2: `inmotion.sh` lifecycle

`scripts/box/inmotion.sh` with `up`, `down`, `destroy`, `ssh`, `ip`, `status` and `allow-ip`; `bash -n` and shellcheck clean. Validated against the real project with a small box (`--name dad-test --flavor m7i.large --volume-size 20`), destroyed afterwards:

- **Fresh `up`: 255 s** from an empty project — image upload ~2 min of it (the 864 MB download is cached under `~/.cache/dot-agent-deck/box/` and verified against Ubuntu's `SHA256SUMS`), then network, router, security group, keypair, volume, server, floating IP, SSH and `cloud-init status --wait`. The box runs Ubuntu 26.04.1 on `7.0.0-31-generic` — the dev box's kernel — with `/tmp` a tmpfs like the dev box.
- **`up` on a running box: 43 s, no changes.** `down` then `up`: 83 s.
- sshd reports `permitrootlogin no`, `passwordauthentication no`, `kbdinteractiveauthentication no`; cloud-init reports no errors; outbound HTTPS works through the router.
- `allow-ip` replaced the rules with two explicit CIDRs, then with the detected IP.
- **`destroy` leaves nothing**: 0 servers, volumes, subnets, routers, floating IPs and private images; the only networks and security group left are the pre-existing `External`, `ext` and `default`. *(Corrected later the same day: the keypair count in that check was always 0 because it asked `keypair list` for an `ID` column it does not have. Re-checked by name after the next `destroy`: `dad-test-key` was gone and only the live `inmotion-key` remained.)*

**Found and fixed: cloud-init's `fs_setup` wiped the data volume.** The first `down`/`up` lost a marker file in `/home`. `cloud-init.log` showed `fs_setup` with `partition: none` and `overwrite: false` running `mkfs.ext4 -L dad-home -F /dev/vdb` over the existing filesystem. Replaced by the guarded `bootcmd` described under [The data volume](#the-data-volume); re-tested both ways — an existing filesystem survived `down`/`up` (same creation time, mount count 2, no `mkfs` in the log) and a fresh empty volume was formatted and mounted. `down` also gained a graceful stop before the delete, since a bare `server delete` powers the guest off.

### 2026-09-24 — M3 and M4: `bootstrap.sh`, and the deck on the box

`scripts/box/bootstrap.sh`, `scripts/box/devbox-global.json` and `scripts/box/system/mem-sampler.*`; `inmotion.sh up` now tars `scripts/box/` to `~/.dad-box` on the box and runs it there. Validated on the `dad-test` box (`m7i.large`, 50 GB) with `--repo https://github.com/vfarcic/dot-agent-deck`:

- **First full bootstrap** installed every layer, cloned this repo, ran its `devbox install` (the full toolchain, Tauri deps and gcloud included) and installed its orphan-reaper timer. **Re-run: 48 s, every layer skipped.**
- Versions on the box: dot-agent-deck 0.41.2, claude 2.1.281, opencode 1.18.32, codex 0.156.1, pi 0.87.1, devin 3000.11.3, sem 0.36.0, docker 29.8.1, node 22.22.1, devbox 0.18.3, git 2.55.0, gh 2.98.0.
- **A non-interactive ssh shell — what the deck daemon inherits — resolves every agent**, dot-agent-deck, git, node, docker, devbox and mosh-server. The systemd user manager has the same PATH now and at its next start.
- **The deck works against the box.** From the dev box, with a scratch registry (`DOT_AGENT_DECK_REMOTES`): `remote add dad-test …` installed the client's 0.41.0 and hooks; `remote doctor` passed `HostReachable`, `RemoteBinary` and `ProtocolCompatible`. Its one `FAIL` (`RemoteForward`) and the `WARN` concern the optional reverse-tunnel recipe; its `UNKNOWN`s need root to read `sshd -T`, which was checked directly in M2.

Found and fixed on the way: the Devin installer ends by running `devin setup`, an interactive login that fails without a terminal and aborted the first run — bootstrap now feeds it no input and judges by whether the binary landed; apt's recommends pulled in alacritty and Mesa (`--no-install-recommends`); `hooks install` covers Claude Code alone by default (now one call per agent); the `environment.d` override described above; and `remote add` rejected the box's host key because the deck reads `~/.ssh/known_hosts`, not the script's per-box file — `up` now copies the box's key there after removing any stale entry for the reused IP, and `down` removes it.

**Not yet done — M5:** an agent logged in and working in a deck pane on the box. Logging agents in is the operator's step (decision 5), so this is the maintainer's manual check.

### 2026-09-24 — The data volume filled up: 300 GB → 2000 GB, and `grow`

After an hour or two of real use, a Pi pane on `inmotion` failed with `ENOSPC: no space left on device`. `/home` was at 269 of 295 GB while the 969 GB root disk had 948 GB free. `du` put 262 GB in `~/code`: six `dot-agent-deck-dispatch-*` worktrees at 95, 46, 40, 28, 28 and 25 GB — each its own `--features e2e` `target/` — five of them with live agents at the time.

Fixed without downtime: `openstack --os-volume-api-version 3.42 volume set --size 2000` (the project's block-storage API is at 3.70; 3.42 is where in-use extend arrived), then `resize2fs` on the mounted filesystem — `/home` went to 2.0 TB with 1.7 TB free while every agent kept running. `inmotion.sh` now defaults to 2000 GB and has `grow --volume-size N`, which refuses anything but an increase; `up` also runs `resize2fs` (a no-op when the filesystem already fills the device), so growing a volume while the box is down lands on the next `up`. Both paths were validated on a throwaway `m7i.medium` box: online 20 → 30 GB, and 30 → 40 GB while down, applied by the following `up`.
