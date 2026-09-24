# PRD #1279: On-demand InMotion Cloud dev box

**Status**: Not started — this document is the plan, written before any implementation.
**Priority**: Medium
**Created**: 2026-09-24
**Issue**: [#1279](https://github.com/vfarcic/dot-agent-deck/issues/1279)

## Problem Statement

Agent-deck development runs on one hand-built Linux box. Everything that makes it work — the apt packages, the devbox toolchain, the agent CLIs, docker, Tailscale, the monitoring timers, the kernel and limit tuning — was accumulated by hand, and none of it is written down as something that can be replayed. There is no way to stand up a second, equivalent box when one is needed (more parallel agents, a clean environment, a machine to hand to a remote session) and remove it again when it is not.

The only provisioning script in the tree, `examples/provision-upcloud-vm.sh`, does not help. It was written for PRD #76, has not changed since `4b81c066`, and installs a toolchain the dev box no longer resembles — Node 20 from NodeSource, Claude Code through `npm`, and nothing of devbox, docker or the Rust toolchain. Its install steps also run inside cloud-init, which executes once on first boot and cannot be re-run to fix a partial failure or apply a change.

## Solution Overview

Two scripts with a hard boundary between them:

1. **`scripts/box/inmotion.sh`** — the only part that knows about InMotion Cloud. It creates and removes the VM and its surrounding resources through the OpenStack CLI. Its cloud-init is minimal: a user, SSH hardening, and joining the tailnet. Nothing else.
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
| 6 | **Tailscale only — the security group exposes no ports**, not even SSH | Removes the public attack surface entirely. |
| 7 | **`down` deletes the VM and keeps a data volume; `destroy` deletes everything** | Re-creating is cheap; repos, `target/` caches and agent logins survive. |
| 8 | **Flavor `m7i.4xlarge`** (16 vCPU, 64 GB RAM, 1 TB root) by default, overridable | The only flavor matching the dev box's 16 cores. Paid only while the box exists. |
| 9 | **Data volume: `NVME` type, 300 GB** by default, overridable | Cargo builds and nextest are I/O-heavy; the dev box runs on an NVMe SSD. |
| 10 | **Upload Ubuntu's official 26.04 server cloud image** (`resolute-server-cloudimg-amd64.img`) once, reuse it after | InMotion offers only 20.04/22.04/24.04. |
| 11 | **Not installed**: `aether`, `dot-ai` | Removed from the dev box on 2026-09-24 as well; the project-local `dot-ai` skill went in #1277. |
| 12 | **Installed**: the dev box's own timers (`heartbeat`, `mem-sampler`, `tailscale-watchdog`, `boot-canary`), auditd, `sem`, `devin` | Confirmed wanted on the cloud boxes. |
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

What the project offers, as listed on 2026-09-24: flavors `m7i.medium` through `m7i.8xlarge`; images Ubuntu 20.04/22.04/24.04; external networks `External` and `ext` and no tenant network; volume types `NVME` and `HDD`; no existing servers.

### `inmotion.sh up`

1. **Image**: if no `ubuntu-26.04-dad` image exists, download the 26.04 server cloud image, verify it against Ubuntu's `SHA256SUMS`, and upload it.
2. **Keypair and security group**: a keypair from the operator's public key (a fallback path only — normal access is over Tailscale) and a security group with **no ingress rules**.
3. **Volume**: create the `NVME` data volume if it does not exist; reuse it if it does.
4. **Server**: create it on the external network with the minimal cloud-init, attach the volume, wait for it to join the tailnet.
5. **Hand-off**: run `bootstrap.sh` over SSH via the Tailscale address, then print the auth checklist it produces.

Every resource carries a name prefix derived from `--name`, so `down` and `destroy` find exactly what `up` created and nothing else.

### Tailscale join

The VM must join the tailnet during first boot, before anything can reach it. That needs an auth key in cloud-init user-data, which anyone who can manage the InMotion project — and any process on the VM, through the metadata service — can read. So the key is **single-use, pre-approved, tagged and short-lived**, generated per `up` or held in GCP Secret Manager and pulled through `vals`. The fallback, if that proves impractical, is one ingress rule for SSH from the operator's current public IP that `up` removes after the join.

### The data volume

Holds the operator's home directory, so repos, worktrees, `target/` caches and agent logins survive `down`/`up`. How it is mounted before the user's first login — cloud-init `disk_setup`/`fs_setup`/`mounts` formatting it only when empty, versus mounting it at a fixed path and pointing the home directory there — is settled in M2 by testing, not assumed here.

### `bootstrap.sh`

Ordered so each layer can assume the one before it:

1. **apt**: docker (Docker's own repo, plus the `docker` group), tailscale (its own repo), auditd, build-essential (the project's cargo links with the system C compiler — `devbox.json` carries none), and whatever the cloud image lacks of openssh and chrony.
2. **System settings**: `vm.swappiness=10`, `vm.dirty_ratio=10`, `vm.dirty_background_ratio=5`; an 8 GB swap file; `nofile` 524288; linger for the operator.
3. **Timers**: the imported `heartbeat`, `mem-sampler`, `tailscale-watchdog` and `boot-canary` units, plus `scripts/install-reaper-timer.sh` for the orphan reaper.
4. **nix + devbox**, then `devbox global install` from the checked-in definition, and `devbox global shellenv` added to both `~/.profile` and `~/.bashrc`. `mosh-server` gets a symlink into `/usr/local/bin`, because a non-interactive SSH session never reads the shellenv.
5. **Agents**: claude, opencode, codex, pi and devin through their own installers; `dot-agent-deck` itself; then `dot-agent-deck hooks install` rather than copying hook files.
6. **Agent config**: optionally seeded once from the operator's machine (`~/.claude/settings.json` and plugins, codex `config.toml`, `opencode.jsonc`, pi settings) and never managed afterwards — the agents write to those files themselves.
7. **Project** (with `--repo`): clone it onto the data volume and run `devbox install`.
8. **Auth checklist**: for each agent, detect whether it is already logged in and list only what is missing.

Re-running must be safe at every step: each one checks current state before changing it.

### The inventory

Taken read-only on the dev box on 2026-09-24. Classification:

| Channel | Items |
| --- | --- |
| apt | docker-ce + buildx/compose plugins, containerd, tailscale, auditd, build-essential |
| `devbox global` | git, gh, jq, curl, rsync, unzip, mosh, xvfb, go, uv, rustup + cargo-audit/cross/rust-analyzer, kcl, node, pnpm |
| Native installer | claude (`~/.local/bin`), opencode (`~/.opencode/bin`), codex, pi, devin, sem, dot-agent-deck |
| Imported from the box | `heartbeat`, `mem-sampler`, `tailscale-watchdog`, `boot-canary` scripts and units |
| System settings | sysctl values, swap, limits, linger, `docker` group |
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

- `scripts/box/inmotion.sh up` on an empty project produces a box reachable only over Tailscale, bootstrapped, with `cargo test-fast` passing on this repo.
- The auth checklist names exactly the agents not yet logged in.
- `down` then `up` preserves the data volume's contents; `destroy` leaves no resource behind.
- Re-running `up` or `bootstrap.sh` on a finished box is a no-op.
- The UpCloud example installs nothing of its own and delegates to `bootstrap.sh`.

## Milestones

- [ ] **M1 — Cloud access wired.** `openstackclient` pinned in `devbox.json`; the `OS_*` entries in `.env.vals.yaml`; `openstack token issue` works from a devbox shell with `USE_VALS=1`.
- [ ] **M2 — `inmotion.sh` lifecycle.** `up`/`down`/`destroy`/`ssh`/`ip`/`status` working and idempotent: image upload, no-ingress security group, NVME volume, Tailscale join, volume mount settled.
- [ ] **M3 — `bootstrap.sh` system layer.** apt, system settings, docker, tailscale, linger, imported timers — re-runnable.
- [ ] **M4 — `bootstrap.sh` user layer.** nix + devbox, `devbox global`, agents, `dot-agent-deck` + hooks, config seeding, `--repo`, auth checklist.
- [ ] **M5 — End-to-end validation on InMotion.** The full run in [Testing](#testing), recorded in the Work Log with timings.
- [ ] **M6 — UpCloud example refactored** onto `bootstrap.sh`, its own install steps deleted.
- [ ] **M7 — Docs.** `docs/develop/` page covering prerequisites, commands, the auth checklist, costs and teardown; linked from `CONTRIBUTING.md`.

## Risks

- **The uploaded 26.04 image may not boot cleanly on InMotion** (virtio, config drive vs metadata service). Mitigation: validate early in M2; fall back to InMotion's 24.04 image and record the difference.
- **Secrets in cloud-init user-data.** Mitigation: the Tailscale key is single-use and short-lived; nothing else secret goes in user-data.
- **Drift from the dev box** (decision 3). Mitigation: stated in the docs; the inventory is recorded here so a later diff has a baseline.
- **Agent installers change URLs or behaviour.** Mitigation: each installer is one isolated step; the auth checklist makes a missing agent visible.
- **The imported timer scripts may embed machine-specific values** (the heartbeat URL is set by a separate `heartbeat-set-url.sh`). Mitigation: review each before importing; anything secret becomes a `vals` reference.

## Open Questions

1. Which external network — `External` or `ext` — should servers attach to?
2. The Tailscale auth key: generated per `up` through the Tailscale API, or a reusable tagged key held in GCP Secret Manager?
3. Is an UpCloud account still available to validate M6 end to end, or is that milestone validated by review only?
4. Does refactoring `examples/provision-upcloud-vm.sh` warrant a changelog fragment (rule 19)? The example is in the public repo but not on the docs site.
5. How does `devin` authenticate? Not inspected in the inventory.

## Work Log

### 2026-09-24 — Created

Planned with the maintainer in one session. The inventory above was taken read-only on the dev box. The same session removed `aether` and `dot-ai` from the dev box, filed #1277 / PR #1278 to remove the project-local `dot-ai` skill, granted the `vals` service account read access to `inmotion-id` and `inmotion-secret`, and verified the application credential against `https://iad4.inmotioncloud.net:5000/v3`. An earlier claim in that session that `/tmp` was not a tmpfs on the dev box was wrong — it is a 14 GB tmpfs, and CLAUDE.md rule 14 stands.
