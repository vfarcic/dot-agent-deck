#!/usr/bin/env bash
# provision-upcloud-vm.sh
#
# Provision a single UpCloud VM ready for `dot-agent-deck remote add`, sized
# for the "budget" sweet spot (8 vCPU / 16 GB, ~$150/mo) discussed in the
# sizing notes for PRD #76. Idempotent: re-running with the same --name
# skips creation, prints the existing VM's IP, and re-runs the bootstrap.
#
# What it does (default flow):
#   1. Creates an UpCloud VM (plan HICPU-8xCPU-16GB, Ubuntu 24.04 LTS).
#   2. Injects a cloud-init payload that:
#        - creates a non-root `deck` user with your SSH pubkey
#        - hardens sshd (no root login, no password auth)
#        - installs Node.js 20.x (NodeSource), git, build tools
#        - installs Claude Code globally (npm)
#        - installs OpenCode for the `deck` user
#        - enables systemd lingering so user services survive logout
#   3. Waits for cloud-init to finish.
#   4. Prints the `dot-agent-deck remote add` command to copy-paste.
#
# Prerequisites on the laptop:
#   - `upctl` installed and authenticated (`upctl account show` works).
#       https://github.com/UpCloudLtd/upcloud-cli
#   - An SSH public key (default ~/.ssh/id_ed25519.pub).
#   - `jq` (optional, used for nicer parsing; falls back to grep).
#
# Usage:
#   ./provision-upcloud-vm.sh                       # provision with defaults
#   ./provision-upcloud-vm.sh --name my-vm          # override name
#   ./provision-upcloud-vm.sh --plan 4xCPU-8GB      # cheaper plan
#   ./provision-upcloud-vm.sh --destroy             # tear down the VM
#   ./provision-upcloud-vm.sh --print-cloudinit     # dump cloud-init YAML
#
# Notes:
#   - No secrets are baked into the VM. After provisioning, ssh in and
#     export ANTHROPIC_API_KEY in the deck user's environment (the script
#     prints instructions at the end).
#   - The VM is single-user (`deck`) and per-host, not per-project. PRD #76
#     assumes per-project environments; if you reuse this host for many
#     projects, see the "one VM for all" tradeoffs in the design notes.

set -euo pipefail

# -------- defaults --------------------------------------------------------

NAME="${NAME:-dad-vm}"
PLAN="${PLAN:-HICPU-8xCPU-16GB}"         # High-CPU 8 vCPU / 16 GB / 200 GB SSD; the budget pick.
                                         # Alternatives: PREMIUM-8xCPU-16GB (dedicated cores),
                                         # 8xCPU-32GB (more RAM + 640 GB), 6xCPU-16GB (cheaper, fewer cores).
ZONE="${ZONE:-de-fra1}"                  # Frankfurt; pick whichever is closest
OS="${OS:-Ubuntu Server 24.04 LTS (Noble Numbat)}"
DECK_USER="${DECK_USER:-deck}"
PUBKEY_PATH="${PUBKEY_PATH:-$HOME/.ssh/id_ed25519.pub}"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$HOME/.ssh/known_hosts")

ACTION="provision"
CLOUDINIT_TMP=""

# -------- helpers ---------------------------------------------------------

die() { echo "error: $*" >&2; exit 1; }
log() { echo "[$(date +%H:%M:%S)] $*"; }

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

# -------- arg parsing -----------------------------------------------------

while [[ $# -gt 0 ]]; do
    case "$1" in
        --name)              NAME="$2"; shift 2 ;;
        --plan)              PLAN="$2"; shift 2 ;;
        --zone)              ZONE="$2"; shift 2 ;;
        --os)                OS="$2"; shift 2 ;;
        --user)              DECK_USER="$2"; shift 2 ;;
        --pubkey)            PUBKEY_PATH="$2"; shift 2 ;;
        --destroy)           ACTION="destroy"; shift ;;
        --print-cloudinit)   ACTION="print-cloudinit"; shift ;;
        -h|--help)           usage; exit 0 ;;
        *)                   die "unknown arg: $1 (try --help)" ;;
    esac
done

# -------- cloud-init payload ---------------------------------------------

render_cloudinit() {
    local pubkey
    pubkey="$(cat "$PUBKEY_PATH")"
    cat <<EOF
#cloud-config
# Provisioning manifest for dot-agent-deck remote environment.
# Idempotent: re-applying is safe; cloud-init runs runcmd only on first boot.

users:
  - name: ${DECK_USER}
    gecos: dot-agent-deck remote user
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    groups: sudo
    ssh_authorized_keys:
      - ${pubkey}

ssh_pwauth: false
disable_root: true

write_files:
  - path: /etc/ssh/sshd_config.d/99-dot-agent-deck.conf
    content: |
      PermitRootLogin no
      PasswordAuthentication no
      PubkeyAuthentication yes
      ClientAliveInterval 60
      ClientAliveCountMax 30
    permissions: '0644'
  - path: /home/${DECK_USER}/.profile.d-bootstrap.sh
    content: |
      # Added by provision-upcloud-vm.sh
      export PATH="\$HOME/.local/bin:\$HOME/.opencode/bin:\$PATH"
    permissions: '0644'
    owner: ${DECK_USER}:${DECK_USER}

package_update: true
package_upgrade: false

packages:
  - curl
  - git
  - build-essential
  - ca-certificates
  - gnupg
  - unzip
  - jq

runcmd:
  # sshd hardening
  - systemctl reload ssh || systemctl reload sshd

  # Node.js 20.x from NodeSource (Ubuntu apt nodejs lags)
  - bash -c 'curl -fsSL https://deb.nodesource.com/setup_20.x | bash -'
  - apt-get install -y nodejs

  # Claude Code (global; works because Node is system-wide)
  - npm install -g @anthropic-ai/claude-code

  # OpenCode for the deck user (lands in ~/.opencode/bin)
  - runuser -u ${DECK_USER} -- bash -c 'curl -fsSL https://opencode.ai/install | bash'

  # Append our PATH bootstrap to the deck user's bashrc, idempotently
  - runuser -u ${DECK_USER} -- bash -c 'grep -q "profile.d-bootstrap" ~/.bashrc || echo "source ~/.profile.d-bootstrap.sh" >> ~/.bashrc'

  # Lingering: user services + the daemon survive logout
  - loginctl enable-linger ${DECK_USER}

  # Mark cloud-init done so the provision script can poll
  - touch /var/lib/cloud/instance/dot-agent-deck-ready
EOF
}

# -------- upctl wrappers --------------------------------------------------

vm_exists() {
    upctl server list -o json 2>/dev/null \
        | jq -e --arg n "$NAME" '.servers[] | select(.hostname == $n)' >/dev/null 2>&1
}

vm_uuid() {
    upctl server list -o json 2>/dev/null \
        | jq -r --arg n "$NAME" '.servers[] | select(.hostname == $n) | .uuid' \
        | head -n1
}

# `server list` doesn't include IPs; need `server show <uuid>` for that.
vm_ip() {
    local uuid
    uuid="$(vm_uuid)"
    [[ -n "$uuid" ]] || return 1
    upctl server show "$uuid" -o json 2>/dev/null \
        | jq -r '.ip_addresses[] | select(.access == "public" and .family == "IPv4") | .address' \
        | head -n1
}

# -------- main actions ----------------------------------------------------

provision() {
    require_cmd upctl
    require_cmd ssh
    require_cmd jq
    [[ -f "$PUBKEY_PATH" ]] || die "ssh pubkey not found at: $PUBKEY_PATH"

    if vm_exists; then
        log "VM '$NAME' already exists; skipping create"
    else
        log "creating VM '$NAME' (plan=$PLAN, zone=$ZONE, os=$OS)"
        CLOUDINIT_TMP="$(mktemp -t dad-cloudinit.XXXXXX.yaml)"
        trap 'rm -f "${CLOUDINIT_TMP:-}"' EXIT
        render_cloudinit > "$CLOUDINIT_TMP"

        # upctl's --user-data takes the script body inline (not a path).
        upctl server create \
            --hostname "$NAME" \
            --title "$NAME" \
            --plan "$PLAN" \
            --os "$OS" \
            --zone "$ZONE" \
            --ssh-keys "$PUBKEY_PATH" \
            --user-data "$(cat "$CLOUDINIT_TMP")" \
            --wait
    fi

    local ip
    ip="$(vm_ip)"
    [[ -n "$ip" ]] || die "could not resolve public IPv4 for $NAME"
    log "VM '$NAME' public IPv4: $ip"

    log "waiting for SSH to accept connections..."
    local attempt=0
    until ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes \
              "${DECK_USER}@${ip}" true 2>/dev/null; do
        attempt=$((attempt + 1))
        [[ $attempt -lt 60 ]] || die "SSH didn't come up within 5 min"
        sleep 5
    done
    log "SSH up"

    log "waiting for cloud-init to finish (may take 3-5 min on first boot)..."
    ssh "${SSH_OPTS[@]}" "${DECK_USER}@${ip}" \
        'sudo cloud-init status --wait' \
        || die "cloud-init reported failure; ssh in and check /var/log/cloud-init-output.log"

    log "verifying installed toolchain"
    ssh "${SSH_OPTS[@]}" "${DECK_USER}@${ip}" 'bash -lc "
        set -e
        echo \"node:      \$(node --version)\"
        echo \"npm:       \$(npm --version)\"
        echo \"claude:    \$(claude --version 2>/dev/null || echo MISSING)\"
        echo \"opencode:  \$(opencode --version 2>/dev/null || echo MISSING)\"
        echo \"git:       \$(git --version)\"
    "'

    cat <<EOF

==============================================================
VM '$NAME' is ready.

  IP:    $ip
  User:  $DECK_USER
  Plan:  $PLAN
  Zone:  $ZONE

Next steps:

  1. Set your Anthropic API key on the remote (one-time):

       ssh ${DECK_USER}@${ip}
       echo 'export ANTHROPIC_API_KEY=sk-ant-...' >> ~/.bashrc
       exit

  2. Register the host with the deck (from this laptop):

       dot-agent-deck remote add ${NAME} ${DECK_USER}@${ip}

  3. Connect:

       dot-agent-deck connect ${NAME}

To tear down later:

       $0 --destroy --name ${NAME}

==============================================================
EOF
}

destroy() {
    require_cmd upctl
    require_cmd jq

    if ! vm_exists; then
        log "no VM named '$NAME' — nothing to do"
        return
    fi

    local uuid
    uuid="$(vm_uuid)"
    log "stopping VM '$NAME' ($uuid)"
    upctl server stop "$uuid" --wait || true
    log "deleting VM and its storage"
    upctl server delete "$uuid" --delete-storages
    log "done"
}

# -------- dispatch --------------------------------------------------------

case "$ACTION" in
    provision)        provision ;;
    destroy)          destroy ;;
    print-cloudinit)  render_cloudinit ;;
    *)                die "unknown action: $ACTION" ;;
esac
