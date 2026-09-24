#!/usr/bin/env bash
# Create and remove an on-demand dev box on InMotion Cloud (OpenStack) — PRD #1279.
#
# This is the only part that knows about InMotion. It builds the VM and what
# surrounds it, gives it a minimal cloud-init (a user, SSH hardening, the data
# volume mounted at /home), and hands over to scripts/box/bootstrap.sh, which
# knows nothing about any cloud.
#
#   scripts/box/inmotion.sh up        # create (or finish creating) the box, then bootstrap it
#   scripts/box/inmotion.sh down      # delete the VM and its public IP; KEEP the data volume
#   scripts/box/inmotion.sh destroy   # delete everything this box owns, the volume included
#   scripts/box/inmotion.sh ssh       # ssh into the box (extra args go to ssh)
#   scripts/box/inmotion.sh ip        # print the box's public IP
#   scripts/box/inmotion.sh status    # list what exists
#   scripts/box/inmotion.sh allow-ip  # point the SSH/mosh rules at your current IP
#   scripts/box/inmotion.sh grow --volume-size GB  # enlarge /home while the box runs
#
# Options (defaults in brackets):
#   --name NAME          resource prefix and hostname [inmotion]
#   --flavor FLAVOR      [m7i.4xlarge]
#   --volume-size GB     data volume size, mounted at /home [2000]
#   --volume-type TYPE   [NVME]
#   --user USER          login user on the box [$USER]
#   --allow-cidr CIDR    who may reach SSH/mosh; repeatable [your public IPv4/32]
#   --authorized-keys F  extra public keys to install [~/.ssh/authorized_keys]
#   --no-bootstrap       stop after the VM is up
#   --bootstrap-arg ARG  pass ARG to bootstrap.sh; repeatable
#
# Needs the OS_* variables from .env.vals.yaml, so run it from a vals shell:
#   USE_VALS=1 devbox run -- scripts/box/inmotion.sh up
#
# Access is SSH from --allow-cidr only; the security group opens nothing else.
# The script reaches the box with its own key, ~/.ssh/inmotion_ed25519, created
# on first use.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NAME=inmotion
FLAVOR=m7i.4xlarge
# Every dispatched worktree builds its own `--features e2e` target/, measured at
# 25-95 GB each; six of them filled a 300 GB volume in two hours (PRD #1279).
VOLUME_SIZE=2000
VOLUME_TYPE=NVME
BOX_USER="${USER:-$(id -un)}"
AUTHORIZED_KEYS="$HOME/.ssh/authorized_keys"
ALLOW_CIDRS=()
BOOTSTRAP=1
BOOTSTRAP_ARGS=()

EXTERNAL_NET=External
SUBNET_CIDR=10.42.0.0/24
IMAGE_NAME=ubuntu-26.04-server-dad
IMAGE_URL=https://cloud-images.ubuntu.com/releases/resolute/release/ubuntu-26.04-server-cloudimg-amd64.img
IMAGE_SUMS_URL=https://cloud-images.ubuntu.com/releases/resolute/release/SHA256SUMS
BOX_KEY="$HOME/.ssh/inmotion_ed25519"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/dot-agent-deck/box"

die() { echo "inmotion.sh: $*" >&2; exit 1; }
log() { echo "[$(date +%H:%M:%S)] $*"; }

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

ACTION="${1:-}"
[ -n "$ACTION" ] || { usage; exit 2; }
shift
while [ $# -gt 0 ]; do
  case "$1" in
    --name)            NAME="${2:?}"; shift 2 ;;
    --flavor)          FLAVOR="${2:?}"; shift 2 ;;
    --volume-size)     VOLUME_SIZE="${2:?}"; shift 2 ;;
    --volume-type)     VOLUME_TYPE="${2:?}"; shift 2 ;;
    --user)            BOX_USER="${2:?}"; shift 2 ;;
    --allow-cidr)      ALLOW_CIDRS+=("${2:?}"); shift 2 ;;
    --authorized-keys) AUTHORIZED_KEYS="${2:?}"; shift 2 ;;
    --no-bootstrap)    BOOTSTRAP=0; shift ;;
    --bootstrap-arg)   BOOTSTRAP_ARGS+=("${2:?}"); shift 2 ;;
    -h|--help)         usage; exit 0 ;;
    --)                shift; break ;;
    *)                 [ "$ACTION" = ssh ] && break; die "unknown argument: $1 (try --help)" ;;
  esac
done

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/dot-agent-deck/box/$NAME"
KNOWN_HOSTS="$STATE_DIR/known_hosts"
NET="$NAME-net"
SUBNET="$NAME-subnet"
ROUTER="$NAME-router"
SECGROUP="$NAME-sg"
KEYPAIR="$NAME-key"
VOLUME="$NAME-data"

SSH_OPTS=(-i "$BOX_KEY" -o IdentitiesOnly=yes -o UserKnownHostsFile="$KNOWN_HOSTS"
          -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=15)

# -------- openstack helpers -------------------------------------------------

os() { openstack "$@"; }

# Print the ID of the named resource, or nothing: `os_id security group NAME`.
# `show` by name exits non-zero when the resource is absent, which is the only
# signal needed here. The kind may arrive as one word or several.
os_id() {
  local name="${*: -1}" kind
  read -ra kind <<< "${*:1:$#-1}"
  os "${kind[@]}" show "$name" -f value -c id 2>/dev/null || true
}

require_cloud() {
  command -v openstack >/dev/null || die "openstack not on PATH — run from the devbox: USE_VALS=1 devbox run -- $0 $ACTION"
  command -v jq >/dev/null || die "jq not on PATH — run from the devbox"
  [ -n "${OS_AUTH_URL:-}" ] || die "OS_AUTH_URL is unset — run with USE_VALS=1 so .env.vals.yaml is loaded"
  os token issue -f value -c id >/dev/null 2>&1 || die "OpenStack rejected the credential (openstack token issue failed)"
}

floating_ip() {
  os floating ip list --long -f json \
    | jq -r --arg d "$NAME" '.[] | select(.Description == $d) | .["Floating IP Address"]' | head -n1
}

my_cidrs() {
  if [ ${#ALLOW_CIDRS[@]} -eq 0 ]; then
    local ip
    ip="$(curl -fsS --max-time 10 https://api.ipify.org)" || die "could not detect your public IP; pass --allow-cidr"
    [[ "$ip" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "unexpected public IP answer: $ip"
    ALLOW_CIDRS=("$ip/32")
  fi
  printf '%s\n' "${ALLOW_CIDRS[@]}"
}

# -------- ensure_* : each creates its resource only when it is missing ------

ensure_box_key() {
  [ -f "$BOX_KEY" ] && return
  log "creating SSH key $BOX_KEY"
  mkdir -p "$(dirname "$BOX_KEY")" && chmod 700 "$(dirname "$BOX_KEY")"
  ssh-keygen -q -t ed25519 -N '' -C "inmotion@$(hostname)" -f "$BOX_KEY"
}

ensure_image() {
  [ -n "$(os_id image "$IMAGE_NAME")" ] && return
  mkdir -p "$CACHE_DIR"
  local img want got
  img="$CACHE_DIR/$(basename "$IMAGE_URL")"
  want="$(curl -fsSL "$IMAGE_SUMS_URL" | awk -v f="*$(basename "$IMAGE_URL")" '$2 == f {print $1}')"
  [ -n "$want" ] || die "no checksum for $(basename "$IMAGE_URL") in $IMAGE_SUMS_URL"
  if [ ! -f "$img" ] || [ "$(sha256sum "$img" | cut -d' ' -f1)" != "$want" ]; then
    log "downloading $(basename "$IMAGE_URL")"
    curl -fL --progress-bar -o "$img.part" "$IMAGE_URL"
    mv "$img.part" "$img"
  fi
  got="$(sha256sum "$img" | cut -d' ' -f1)"
  [ "$got" = "$want" ] || die "checksum mismatch for $img (want $want, got $got)"
  log "uploading image $IMAGE_NAME (this takes a few minutes)"
  os image create "$IMAGE_NAME" --file "$img" --disk-format qcow2 --container-format bare \
    --private --property os_distro=ubuntu --property os_version=26.04 \
    --property dad_sha256="$want" >/dev/null
}

ensure_network() {
  if [ -z "$(os_id network "$NET")" ]; then
    log "creating network $NET"
    os network create "$NET" >/dev/null
  fi
  if [ -z "$(os_id subnet "$SUBNET")" ]; then
    log "creating subnet $SUBNET ($SUBNET_CIDR)"
    os subnet create "$SUBNET" --network "$NET" --subnet-range "$SUBNET_CIDR" \
      --dns-nameserver 1.1.1.1 --dns-nameserver 8.8.8.8 >/dev/null
  fi
  if [ -z "$(os_id router "$ROUTER")" ]; then
    log "creating router $ROUTER (gateway on $EXTERNAL_NET)"
    os router create "$ROUTER" >/dev/null
    os router set "$ROUTER" --external-gateway "$EXTERNAL_NET"
    os router add subnet "$ROUTER" "$SUBNET"
  fi
}

ensure_secgroup() {
  if [ -z "$(os_id security group "$SECGROUP")" ]; then
    log "creating security group $SECGROUP"
    os security group create "$SECGROUP" --description "dot-agent-deck box $NAME: SSH and mosh from allowed CIDRs only" >/dev/null
    set_allowed_cidrs
  fi
}

# Replace every ingress rule with SSH (tcp/22) and mosh (udp/60000-61000) from
# the allowed CIDRs. Egress keeps OpenStack's default allow-all.
set_allowed_cidrs() {
  local rule cidr
  for rule in $(os security group rule list "$SECGROUP" --ingress -f value -c ID); do
    os security group rule delete "$rule"
  done
  while read -r cidr; do
    log "allowing SSH and mosh from $cidr"
    os security group rule create "$SECGROUP" --ingress --ethertype IPv4 --protocol tcp --dst-port 22 --remote-ip "$cidr" >/dev/null
    os security group rule create "$SECGROUP" --ingress --ethertype IPv4 --protocol udp --dst-port 60000:61000 --remote-ip "$cidr" >/dev/null
  done < <(my_cidrs)
}

ensure_keypair() {
  os keypair show "$KEYPAIR" >/dev/null 2>&1 && return
  log "registering keypair $KEYPAIR"
  os keypair create "$KEYPAIR" --public-key "$BOX_KEY.pub" >/dev/null
}

ensure_volume() {
  [ -n "$(os_id volume "$VOLUME")" ] && return
  log "creating volume $VOLUME (${VOLUME_SIZE} GB, $VOLUME_TYPE)"
  os volume create "$VOLUME" --size "$VOLUME_SIZE" --type "$VOLUME_TYPE" >/dev/null
  for _ in $(seq 1 60); do
    [ "$(os volume show "$VOLUME" -f value -c status)" = available ] && return
    sleep 2
  done
  die "volume $VOLUME did not become available"
}

# -------- cloud-init ---------------------------------------------------------

render_cloudinit() {
  local volume_id="$1" keys
  # virtio-blk exposes the first 20 characters of the volume ID as its serial.
  local device="/dev/disk/by-id/virtio-${volume_id:0:20}"
  keys="$(cat "$BOX_KEY.pub"; [ -f "$AUTHORIZED_KEYS" ] && grep -E '^(ssh-|ecdsa-|sk-)' "$AUTHORIZED_KEYS" || true)"
  cat <<EOF
#cloud-config
hostname: ${NAME}

# The data volume holds /home, so the user's files survive \`down\`/\`up\`.
# It is formatted HERE rather than by cloud-init's fs_setup: fs_setup with
# partition:none ran mkfs -F over an existing filesystem despite
# overwrite:false (measured 2026-09-24, PRD #1279). blkid -p probes the device
# itself and exits 2 only when it finds no signature at all; any other answer,
# including an error, leaves the disk alone. bootcmd runs on every boot, before
# mounts and users_groups, so the user is created on the mounted volume and
# keeps UID 1000 across rebuilds.
bootcmd:
  - |
    dev=${device}
    for _ in \$(seq 1 30); do [ -b "\$dev" ] && break; sleep 1; done
    blkid -p "\$dev" >/dev/null 2>&1; rc=\$?
    if [ "\$rc" -eq 2 ]; then mkfs.ext4 -q -L dad-home "\$dev"; fi
mounts:
  - [ "LABEL=dad-home", /home, ext4, "defaults,nofail,discard", "0", "2" ]

users:
  - name: ${BOX_USER}
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    groups: [adm, sudo]
    ssh_authorized_keys:
$(printf '%s\n' "$keys" | sed 's/^/      - /')

ssh_pwauth: false
disable_root: true

# sshd takes the FIRST value it reads for each keyword, so this file sorts
# before cloud-init's own 50-cloud-init.conf.
write_files:
  - path: /etc/ssh/sshd_config.d/00-dot-agent-deck.conf
    permissions: '0644'
    content: |
      PermitRootLogin no
      PasswordAuthentication no
      KbdInteractiveAuthentication no
      PubkeyAuthentication yes
      ClientAliveInterval 15
      ClientAliveCountMax 3

runcmd:
  - [ systemctl, restart, ssh ]
EOF
}

# -------- actions ------------------------------------------------------------

wait_for_ssh() {
  local ip="$1"
  log "waiting for SSH on $ip"
  for _ in $(seq 1 60); do
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes "$BOX_USER@$ip" true 2>/dev/null && return
    sleep 5
  done
  die "SSH did not come up on $ip within 5 minutes (is your IP in --allow-cidr?)"
}

# `dot-agent-deck remote add`/`connect` use the operator's own known_hosts, not
# this script's per-box file. Floating IPs are reused across boxes, so drop any
# stale entry for the address before adding the current box's key.
trust_host_key() {
  local ip="$1"
  [ -s "$KNOWN_HOSTS" ] || return 0
  touch "$HOME/.ssh/known_hosts"
  ssh-keygen -R "$ip" -f "$HOME/.ssh/known_hosts" >/dev/null 2>&1 || true
  cat "$KNOWN_HOSTS" >> "$HOME/.ssh/known_hosts"
}

do_up() {
  require_cloud
  ensure_box_key
  ensure_image
  ensure_network
  ensure_secgroup
  ensure_keypair
  ensure_volume

  if [ -z "$(os_id server "$NAME")" ]; then
    local volume_id userdata
    volume_id="$(os_id volume "$VOLUME")"
    mkdir -p "$STATE_DIR"
    userdata="$STATE_DIR/user-data.yaml"
    render_cloudinit "$volume_id" > "$userdata"
    # The host key is new on every rebuild; forget the last one.
    rm -f "$KNOWN_HOSTS"
    log "creating server $NAME ($FLAVOR)"
    os server create "$NAME" --flavor "$FLAVOR" --image "$IMAGE_NAME" --network "$NET" \
      --security-group "$SECGROUP" --key-name "$KEYPAIR" --user-data "$userdata" \
      --block-device "uuid=$volume_id,source_type=volume,destination_type=volume,boot_index=-1,delete_on_termination=false" \
      --wait >/dev/null
  else
    log "server $NAME already exists"
  fi

  local ip
  ip="$(floating_ip)"
  if [ -z "$ip" ]; then
    log "allocating a public IP"
    ip="$(os floating ip create "$EXTERNAL_NET" --description "$NAME" -f value -c floating_ip_address)"
  fi
  local server_ips
  server_ips="$(os server show "$NAME" -f json | jq -r '.addresses | tostring')"
  if [[ "$server_ips" != *"$ip"* ]]; then
    os server add floating ip "$NAME" "$ip"
  fi
  log "public IP: $ip"

  wait_for_ssh "$ip"
  trust_host_key "$ip"
  log "waiting for cloud-init to finish"
  ssh "${SSH_OPTS[@]}" "$BOX_USER@$ip" 'sudo cloud-init status --wait >/dev/null; cloud-init status --long | sed -n "1,3p"; sudo resize2fs "$(findmnt -no SOURCE /home)" >/dev/null 2>&1; findmnt -no SOURCE,SIZE /home'

  if [ "$BOOTSTRAP" -eq 1 ]; then
    log "running bootstrap.sh on the box"
    # Ship the whole directory: bootstrap.sh reads the files beside it.
    local remote_cmd="rm -rf ~/.dad-box && mkdir -p ~/.dad-box && tar -xzf - -C ~/.dad-box && bash ~/.dad-box/bootstrap.sh"
    # Expanded here on purpose, each argument quoted for the remote shell.
    [ ${#BOOTSTRAP_ARGS[@]} -gt 0 ] && remote_cmd+="$(printf ' %q' "${BOOTSTRAP_ARGS[@]}")"
    # shellcheck disable=SC2029
    tar -czf - -C "$SCRIPT_DIR" --exclude=inmotion.sh . | ssh "${SSH_OPTS[@]}" "$BOX_USER@$ip" "$remote_cmd"
  fi

  cat <<EOF

Box '$NAME' is up.
  ssh:     $0 ssh --name $NAME
  deck:    dot-agent-deck remote add $NAME $BOX_USER@$ip --key $BOX_KEY
           dot-agent-deck connect $NAME
  pause:   $0 down --name $NAME      (keeps /home)
  remove:  $0 destroy --name $NAME   (deletes everything)
EOF
}

do_down() {
  require_cloud
  if [ -n "$(os_id server "$NAME")" ]; then
    # Stop first: a bare delete powers the guest off without a shutdown, and
    # whatever /home had not yet flushed to the volume would be lost.
    if [ "$(os server show "$NAME" -f value -c status)" = ACTIVE ]; then
      log "shutting down $NAME"
      os server stop "$NAME"
      for _ in $(seq 1 60); do
        [ "$(os server show "$NAME" -f value -c status)" = SHUTOFF ] && break
        sleep 3
      done
    fi
    log "deleting server $NAME (the data volume is kept)"
    os server delete "$NAME" --wait
  fi
  local ip
  ip="$(floating_ip)"
  if [ -n "$ip" ]; then
    log "releasing public IP $ip"
    os floating ip delete "$ip"
    ssh-keygen -R "$ip" -f "$HOME/.ssh/known_hosts" >/dev/null 2>&1 || true
  fi
  log "down: $VOLUME is kept; \`up\` reattaches it"
}

do_destroy() {
  do_down
  if [ -n "$(os_id volume "$VOLUME")" ]; then
    log "deleting volume $VOLUME"
    for _ in $(seq 1 60); do
      [ "$(os volume show "$VOLUME" -f value -c status)" = available ] && break
      sleep 2
    done
    os volume delete "$VOLUME"
  fi
  if [ -n "$(os_id router "$ROUTER")" ]; then
    log "deleting router $ROUTER"
    os router remove subnet "$ROUTER" "$SUBNET" 2>/dev/null || true
    os router unset --external-gateway "$ROUTER" 2>/dev/null || true
    os router delete "$ROUTER"
  fi
  [ -n "$(os_id subnet "$SUBNET")" ] && { log "deleting subnet $SUBNET"; os subnet delete "$SUBNET"; }
  [ -n "$(os_id network "$NET")" ] && { log "deleting network $NET"; os network delete "$NET"; }
  [ -n "$(os_id security group "$SECGROUP")" ] && { log "deleting security group $SECGROUP"; os security group delete "$SECGROUP"; }
  os keypair show "$KEYPAIR" >/dev/null 2>&1 && { log "deleting keypair $KEYPAIR"; os keypair delete "$KEYPAIR"; }
  # The image is shared by every box; remove it only when no server uses it.
  local image_id
  image_id="$(os_id image "$IMAGE_NAME")"
  if [ -n "$image_id" ] && [ -z "$(os server list --image "$image_id" -f value -c ID)" ]; then
    log "deleting image $IMAGE_NAME (no server uses it)"
    os image delete "$image_id"
  fi
  rm -rf "$STATE_DIR"
  log "destroyed $NAME"
}

# Extend the attached volume and then its filesystem, with no downtime. Cinder
# extends an in-use volume from API microversion 3.42; resize2fs grows a
# mounted ext4 filesystem online.
do_grow() {
  require_cloud
  local current
  current="$(os volume show "$VOLUME" -f value -c size 2>/dev/null)" || die "no volume $VOLUME"
  [ "$VOLUME_SIZE" -gt "$current" ] || die "$VOLUME is already ${current} GB; pass a larger --volume-size (volumes cannot shrink)"
  log "extending $VOLUME from ${current} GB to ${VOLUME_SIZE} GB"
  os --os-volume-api-version 3.42 volume set --size "$VOLUME_SIZE" "$VOLUME"
  for _ in $(seq 1 60); do
    [ "$(os volume show "$VOLUME" -f value -c size)" = "$VOLUME_SIZE" ] && \
      [ "$(os volume show "$VOLUME" -f value -c status)" != extending ] && break
    sleep 3
  done
  local ip
  ip="$(floating_ip)"
  if [ -n "$ip" ]; then
    log "growing the filesystem on the box"
    ssh "${SSH_OPTS[@]}" "$BOX_USER@$ip" 'sudo resize2fs "$(findmnt -no SOURCE /home)" >/dev/null && df -h /home'
  else
    log "box is down; \`up\` grows the filesystem when it next runs"
  fi
}

do_status() {
  require_cloud
  local what id
  printf '%-16s %s\n' image "$( [ -n "$(os_id image "$IMAGE_NAME")" ] && echo "$IMAGE_NAME" || echo -)"
  for what in "network:$NET" "subnet:$SUBNET" "router:$ROUTER" "security group:$SECGROUP" "volume:$VOLUME"; do
    id="$(os_id "${what%%:*}" "${what#*:}")"
    printf '%-16s %s\n' "${what%%:*}" "$( [ -n "$id" ] && echo "${what#*:}" || echo -)"
  done
  printf '%-16s %s\n' keypair "$(os keypair show "$KEYPAIR" >/dev/null 2>&1 && echo "$KEYPAIR" || echo -)"
  printf '%-16s %s\n' server "$(os server show "$NAME" -f value -c status 2>/dev/null || echo -)"
  printf '%-16s %s\n' "public IP" "$(floating_ip || true)"
  [ -n "$(os_id security group "$SECGROUP")" ] && \
    printf '%-16s %s\n' "allowed from" "$(os security group rule list "$SECGROUP" --ingress --protocol tcp -f value -c 'IP Range' | sort -u | paste -sd' ')"
  return 0
}

do_ssh() {
  require_cloud
  local ip
  ip="$(floating_ip)"
  [ -n "$ip" ] || die "box $NAME has no public IP — is it up?"
  exec ssh "${SSH_OPTS[@]}" "$BOX_USER@$ip" "$@"
}

case "$ACTION" in
  up)       do_up ;;
  down)     do_down ;;
  destroy)  do_destroy ;;
  status)   do_status ;;
  ip)       require_cloud; floating_ip ;;
  ssh)      do_ssh "$@" ;;
  grow)     do_grow ;;
  allow-ip) require_cloud; [ -n "$(os_id security group "$SECGROUP")" ] || die "no security group $SECGROUP"; set_allowed_cidrs ;;
  -h|--help|help) usage ;;
  *)        die "unknown action: $ACTION (try --help)" ;;
esac
