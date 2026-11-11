#!/usr/bin/env bash
# Bring an Ubuntu box up to the dot-agent-deck dev box's capabilities — PRD #1279.
#
# Knows nothing about any cloud: scripts/box/inmotion.sh ships this directory
# to the box and runs it, but any Ubuntu 26.04 host with passwordless sudo
# works. Re-running it brings the box up to date; every step checks what is
# already there before changing anything.
#
#   bash bootstrap.sh                        # the box itself
#   bash bootstrap.sh --repo URL [--dir D]   # also clone URL (default dir ~/code/<name>), devbox install it and install its orphan-reaper timer
#   bash bootstrap.sh --use-vals             # also export USE_VALS=true, so devbox loads .env.vals.yaml, and install google-cloud-cli
#   bash bootstrap.sh --relay                # also run the local telemetry relay and the per-run resource collector, and point the environment at the relay
#   bash bootstrap.sh --remote-exec          # also export DAD_REMOTE_EXEC=rwx, so the agents send their compiles to an RWX sandbox
#   bash bootstrap.sh --desktop              # also install what the desktop app's browser and driver test tiers need to run (with --repo, Playwright's browsers too)
#
# Layers, each assuming the one before:
#   1. apt: docker (Docker's repo), auditd, build-essential, nodejs/npm (runtime for npm-distributed agents);
#      with --use-vals google-cloud-cli (Google's repo), with --desktop xvfb and the driver tier's runtime packages
#   2. system: sysctl tuning, 8 GB swap, linger, the mem-sampler timer
#   3. nix + devbox, then `devbox global` from devbox-global.json
#   4. PATH (and, with --use-vals, USE_VALS; with --relay, OTEL_EXPORTER_OTLP_ENDPOINT;
#      with --remote-exec, DAD_REMOTE_EXEC)
#      for non-interactive shells and systemd --user,
#      so the deck daemon finds every agent and every agent inherits the same environment
#   5. agents (claude, opencode 2.x, codex, pi, devin; a pinned opencode 1.x off PATH for tests), sem, the rwx CLI,
#      dot-agent-deck, `hooks install`
#   6. with --relay: the relay container and the dot-agent-deck-collector user service
#   7. the optional repo (with --desktop, Playwright's browsers for it)
#   8. which agents and tools still need logging in
#
# No key is kept in this directory, and the script prints none. Logging the agents,
# gh, rwx and gcloud in is left to the operator, and so is everything that needs
# the operator's own accounts: how the box is reached (tailscale, if you use it,
# with your tailnet), the relay's keys (below), and the repositories you work in
# beyond the one --repo clones.
#
# --use-vals is for a box whose operator can read the secrets .env.vals.yaml
# points at: it makes every devbox shell, and every agent the deck starts, run
# that file through `vals`, which fails without access. It installs
# google-cloud-cli, which `vals` needs a login from for GCP Secret Manager
# (`gcloud auth application-default login`). It sticks: a re-run
# without the flag keeps it while the marked block it wrote is in ~/.bashrc. To
# turn it off, delete that block and re-run. Either way a deck daemon that is
# already running keeps the environment it started with, and so does the
# successor of an in-app daemon restart: its agents see the change only once it
# is stopped and a new one is started from a fresh login (`dot-agent-deck
# connect` from your machine does that), not from a shell opened before the run.
#
# --relay runs the box's local OpenTelemetry relay: the Docker container
# otelcol-hostmetrics (relay/config.yaml beside this script, installed as
# ~/.config/otelcol/config.yaml), an OpenTelemetry Collector that receives OTLP
# on 127.0.0.1:4318, drops the agents' high-volume internal spans, adds the
# box's host metrics and forwards everything to Honeycomb and, when its keys are
# there, Dash0. Its keys live in env files only the operator can read, which
# the run checks for before it changes anything:
#   ~/.config/otelcol/honeycomb.env  HONEYCOMB_API_KEY=...                       (required)
#   ~/.config/otelcol/dash0.env      DASH0_ENDPOINT=...  DASH0_AUTH_TOKEN=...    (optional)
# one NAME=value per line, unquoted (Docker's --env-file format), each a regular
# file of the operator's that no other user can read. A missing file
# is written (mode 600) from the same variables in the environment this script
# runs with (HONEYCOMB_INGEST_KEY, .env.vals.yaml's name, works for the first),
# so running it from a vals shell fills both; an existing file is never
# rewritten. Without dash0.env the relay runs without its Dash0 exporter, except
# that a box whose installed config still has it stops with an error rather
# than dropping Dash0 silently. The container is recreated only when its image,
# config, parameters or key files change, and the previous one and its config
# are put back when the new one does not stay up. It also installs
# dot-agent-deck-collector, the per-run resource collector, as the systemd user
# service dot-agent-deck-collector (user/ beside this script), sending to the
# relay through ~/.config/dot-agent-deck/collector.env.
# And it makes the relay the endpoint that every shell reading ~/.bashrc, and
# every agent the deck starts, inherits by default; devbox.json's init_hook
# keeps an endpoint the shell already has, so a vals shell inherits the relay
# instead of the endpoint .env.vals.yaml names. A later setting still wins: an
# OTEL_EXPORTER_OTLP_ENDPOINT in the repository's .env (the init_hook sources it
# last) or an export further down ~/.bashrc. It works with or without
# --use-vals, and it sticks and reaches a running daemon exactly as --use-vals
# does, through its own marked block; a re-run without the flag reinstalls the
# relay only on a box where this script started it, so a block written before
# the script installed relays keeps pointing at whatever relay the box runs. Turning it off (delete the block and
# re-run) drops the endpoint from ~/.bashrc and environment.d, and unsets it in
# the running systemd --user manager only while its value there is still the
# relay address above; an endpoint with any other value is left alone. It does
# not stop the relay or the collector: `docker rm -f otelcol-hostmetrics` and
# `systemctl --user disable --now dot-agent-deck-collector` do.
#
# --remote-exec is for a box whose agents should compile somewhere else (issue
# #2097). It exports DAD_REMOTE_EXEC=rwx, which makes `cargo xtask
# affected-checks --run` send every cargo command but `cargo fmt` to an RWX
# sandbox, and tells the agents, through CLAUDE.md rule 5 and the role
# templates, to run their other compiling commands as `rwx sandbox exec -- ...`
# (docs/develop/rwx.md). The sandbox's compute is billed to the account the rwx
# CLI is logged in as, which is why the flag is opt-in. Layer 5 installs the CLI
# with or without it, and logging it in stays the operator's step; until it is
# logged in, the helper runs locally and prints a note saying why. It sticks,
# reaches a running daemon and turns off exactly as --use-vals does, through its
# own marked block, except that turning it off unsets the variable in the
# running systemd --user manager only while its value there is still `rwx`.
#
# --desktop does not stick: give it on every run that should check those
# packages.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_URL=""
REPO_DIR=""
USE_VALS_FLAG=0
RELAY_FLAG=0
REMOTE_EXEC_FLAG=0
DESKTOP_FLAG=0
RELAY_ENDPOINT=http://127.0.0.1:4318
# The relay (layer 6). The name, image, mounts and network are the ones the
# relay on the first box ran with before this script installed it (issue #2180).
RELAY_CONTAINER=otelcol-hostmetrics
RELAY_IMAGE=otel/opentelemetry-collector-contrib:0.120.0
RELAY_DIR="$HOME/.config/otelcol"
RELAY_CONFIG="$RELAY_DIR/config.yaml"
HONEYCOMB_ENV="$RELAY_DIR/honeycomb.env"
DASH0_ENV="$RELAY_DIR/dash0.env"
# Written once this script has started the relay, so a later run knows it
# manages it without asking Docker, which may not be running yet.
RELAY_MANAGED="$RELAY_DIR/.dad-box-managed"

while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO_URL="${2:?}"; shift 2 ;;
    --dir)  REPO_DIR="${2:?}"; shift 2 ;;
    --use-vals) USE_VALS_FLAG=1; shift ;;
    --relay)    RELAY_FLAG=1; shift ;;
    --remote-exec) REMOTE_EXEC_FLAG=1; shift ;;
    --desktop)  DESKTOP_FLAG=1; shift ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "bootstrap.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

step() { echo; echo "==> $*"; }
have() { command -v "$1" >/dev/null 2>&1; }

sudo -n true 2>/dev/null || { echo "bootstrap.sh: needs passwordless sudo" >&2; exit 1; }
export DEBIAN_FRONTEND=noninteractive
ME="$(id -un)"
case "$(uname -m)" in x86_64) BIN_ARCH=amd64 ;; aarch64) BIN_ARCH=arm64 ;; *) BIN_ARCH="" ;; esac

# The ~/.bashrc blocks of layer 4. Every block is found by these exact lines and nothing else.
PATH_OPEN='# >>> dad-box PATH (scripts/box/bootstrap.sh) >>>'
PATH_CLOSE='# <<< dad-box PATH <<<'
VALS_OPEN='# >>> dad-box USE_VALS (scripts/box/bootstrap.sh --use-vals) >>>'
VALS_CLOSE='# <<< dad-box USE_VALS <<<'
RELAY_OPEN='# >>> dad-box RELAY (scripts/box/bootstrap.sh --relay) >>>'
RELAY_CLOSE='# <<< dad-box RELAY <<<'
REXEC_OPEN='# >>> dad-box REMOTE_EXEC (scripts/box/bootstrap.sh --remote-exec) >>>'
REXEC_CLOSE='# <<< dad-box REMOTE_EXEC <<<'
# --use-vals, --relay and --remote-exec stick while their block is there, so the layers
# before 4 already act on them (layer 4 checks the markers before it writes).
RELAY_GIVEN="$RELAY_FLAG"
if [ -f "$HOME/.bashrc" ]; then
  grep -qxF "$VALS_OPEN" "$HOME/.bashrc" && USE_VALS_FLAG=1
  grep -qxF "$RELAY_OPEN" "$HOME/.bashrc" && RELAY_FLAG=1
  grep -qxF "$REXEC_OPEN" "$HOME/.bashrc" && REMOTE_EXEC_FLAG=1
fi

# Has env file $1 a non-empty value for every name after it? Reads names only.
env_file_sets() {
  local file="$1" name; shift
  for name in "$@"; do grep -qE "^${name}=." "$file" || return 1; done
}

# Whether this run installs and runs the relay (layer 6). --relay given now
# does; a RELAY block carried over from an earlier run does only when this
# script started the relay then ($RELAY_MANAGED). A box whose block predates the relay being installed here, from when
# --relay only pointed the environment at a relay the box ran itself, keeps
# that meaning until --relay is given again.
RELAY_INSTALL=0
if [ "$RELAY_GIVEN" = 1 ]; then
  RELAY_INSTALL=1
elif [ "$RELAY_FLAG" = 1 ] && [ -f "$RELAY_MANAGED" ]; then
  RELAY_INSTALL=1
fi

# --relay: refuse before anything changes when the relay could not start.
if [ "$RELAY_INSTALL" = 1 ]; then
  relay_fail() {
    echo "bootstrap.sh: --relay: $*" >&2
    echo "bootstrap.sh: changed nothing; see --relay in \`bash $0 --help\`" >&2
    exit 1
  }
  # A key file is the operator's own regular file that nobody else can read,
  # in a directory of the operator's that nobody else can write to.
  if [ -e "$RELAY_DIR" ] || [ -L "$RELAY_DIR" ]; then
    { [ -d "$RELAY_DIR" ] && [ ! -L "$RELAY_DIR" ] && [ -O "$RELAY_DIR" ]; } || relay_fail "$RELAY_DIR must be a directory (not a link) owned by $ME"
    (( (8#$(stat -c %a "$RELAY_DIR") & 8#022) == 0 )) || relay_fail "$RELAY_DIR can be written by other users; run: chmod go-w $RELAY_DIR"
  fi
  for f in "$HONEYCOMB_ENV" "$DASH0_ENV"; do
    if [ -e "$f" ] || [ -L "$f" ]; then
      { [ -f "$f" ] && [ ! -L "$f" ] && [ -O "$f" ]; } || relay_fail "$f must be a regular file (not a link) owned by $ME"
      (( (8#$(stat -c %a "$f") & 8#077) == 0 )) || relay_fail "$f can be read by other users; run: chmod 600 $f"
    fi
  done
  if [ -f "$HONEYCOMB_ENV" ]; then
    env_file_sets "$HONEYCOMB_ENV" HONEYCOMB_API_KEY || relay_fail "$HONEYCOMB_ENV does not set HONEYCOMB_API_KEY"
  elif [ -z "${HONEYCOMB_API_KEY:-}${HONEYCOMB_INGEST_KEY:-}" ]; then
    relay_fail "no Honeycomb key: create $HONEYCOMB_ENV (HONEYCOMB_API_KEY=..., mode 600), or run with HONEYCOMB_API_KEY or HONEYCOMB_INGEST_KEY set (a vals shell sets the second)"
  fi
  if [ -f "$DASH0_ENV" ]; then
    env_file_sets "$DASH0_ENV" DASH0_ENDPOINT DASH0_AUTH_TOKEN || relay_fail "$DASH0_ENV must set both DASH0_ENDPOINT and DASH0_AUTH_TOKEN"
  elif [ -n "${DASH0_ENDPOINT:-}" ] && [ -n "${DASH0_AUTH_TOKEN:-}" ]; then
    :
  elif [ -n "${DASH0_ENDPOINT:-}${DASH0_AUTH_TOKEN:-}" ]; then
    relay_fail "only one of DASH0_ENDPOINT and DASH0_AUTH_TOKEN is set; set both, or neither to run the relay without Dash0"
  elif [ -f "$RELAY_CONFIG" ] && grep -qF 'otlphttp/dash0' "$RELAY_CONFIG"; then
    relay_fail "$RELAY_CONFIG sends to Dash0 but there is no $DASH0_ENV; create it (DASH0_ENDPOINT=... and DASH0_AUTH_TOKEN=..., mode 600) to keep Dash0, or delete $RELAY_CONFIG to drop it"
  fi
  # host networking: another listener on the relay's port would leave the
  # container failing to bind. A running relay container is taken to be that
  # listener; a stopped one holds no port.
  if [ "$(have docker && sudo docker container inspect -f '{{.State.Running}}' "$RELAY_CONTAINER" 2>/dev/null)" != true ] \
     && [ -n "$(ss -Hltn 'sport = :4318' 2>/dev/null)" ]; then
    relay_fail "something other than a running $RELAY_CONTAINER container already listens on port 4318 (\`sudo ss -ltnp 'sport = :4318'\` names it); stop it first"
  fi
fi

# Write $2 to root-owned file $1 only when the content differs; succeed with
# status 0 when it changed and 1 when it did not, so callers can react.
put_root_file() {
  local path="$1" content="$2"
  if [ -f "$path" ] && [ "$(sudo cat "$path")" = "$content" ]; then return 1; fi
  printf '%s\n' "$content" | sudo tee "$path" >/dev/null
}

# -------- 1. apt --------------------------------------------------------------

step "apt packages"
codename="$(. /etc/os-release && echo "$VERSION_CODENAME")"
sudo install -d -m 0755 /etc/apt/keyrings
if [ ! -s /etc/apt/keyrings/docker.asc ]; then
  sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
  sudo chmod a+r /etc/apt/keyrings/docker.asc
fi
put_root_file /etc/apt/sources.list.d/docker.list \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $codename stable" || true
APT_PACKAGES=(build-essential auditd nodejs npm xz-utils ca-certificates curl git lsof
              docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin)
# --use-vals: `vals` reads .env.vals.yaml's secrets from GCP Secret Manager with
# the credentials `gcloud auth application-default login` writes. Google's repo
# is added only while the package is missing, so a box that already has it from
# a source list of its own (another file name or keyring) is not given a second
# one, which apt refuses as a conflicting Signed-By.
if [ "$USE_VALS_FLAG" = 1 ] && ! dpkg -s google-cloud-cli >/dev/null 2>&1; then
  if [ ! -s /etc/apt/keyrings/cloud.google.asc ]; then
    sudo curl -fsSL https://packages.cloud.google.com/apt/doc/apt-key.gpg -o /etc/apt/keyrings/cloud.google.asc
    sudo chmod a+r /etc/apt/keyrings/cloud.google.asc
  fi
  put_root_file /etc/apt/sources.list.d/google-cloud-sdk.list \
    "deb [signed-by=/etc/apt/keyrings/cloud.google.asc] https://packages.cloud.google.com/apt cloud-sdk main" || true
  APT_PACKAGES+=(google-cloud-cli)
fi
# --desktop: what the desktop app's driver tier needs at run time besides the
# WebKitGTK build set devbox provides (docs/develop/desktop-gui.md): the set
# ci.yml's desktop-driver job installs, less the -dev packages. The
# WebKitWebDriver package is webkitgtk-webdriver from Ubuntu 26.04 and
# webkit2gtk-driver before it. The browser tier's runtime libraries and fonts
# come from Playwright itself, in layer 7.
if [ "$DESKTOP_FLAG" = 1 ]; then
  if dpkg -s webkitgtk-webdriver >/dev/null 2>&1; then webdriver_pkg=webkitgtk-webdriver
  elif dpkg -s webkit2gtk-driver >/dev/null 2>&1; then webdriver_pkg=webkit2gtk-driver
  else
    sudo apt-get update -q   # a candidate is only known from current lists
    webdriver_pkg=webkitgtk-webdriver
    case "$(apt-cache policy "$webdriver_pkg" 2>/dev/null | sed -n 's/^ *Candidate: //p')" in
      ""|"(none)") webdriver_pkg=webkit2gtk-driver ;;
    esac
  fi
  APT_PACKAGES+=("$webdriver_pkg" xvfb xclip xdotool openssh-server libnss-wrapper)
fi
missing=()
for p in "${APT_PACKAGES[@]}"; do dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p"); done
if [ ${#missing[@]} -gt 0 ]; then
  sudo apt-get update -q
  sudo apt-get install -y -q --no-install-recommends "${missing[@]}"
else
  echo "all present"
fi
id -nG "$ME" | tr ' ' '\n' | grep -qx docker || sudo usermod -aG docker "$ME"
sudo systemctl enable --now docker auditd >/dev/null 2>&1

# -------- 2. system settings -------------------------------------------------

step "system settings"
if put_root_file /etc/sysctl.d/90-dad-box.conf "$(printf '%s\n' \
    '# dot-agent-deck dev box (PRD #1279): the dev box'"'"'s values.' \
    'vm.swappiness = 10' 'vm.dirty_ratio = 10' 'vm.dirty_background_ratio = 5')"; then
  sudo sysctl --system >/dev/null
fi
if ! swapon --show=NAME --noheadings | grep -qx /swap.img; then
  [ -f /swap.img ] || { sudo fallocate -l 8G /swap.img; sudo chmod 600 /swap.img; sudo mkswap /swap.img >/dev/null; }
  sudo swapon /swap.img
  grep -q '^/swap.img ' /etc/fstab || echo '/swap.img none swap sw 0 0' | sudo tee -a /etc/fstab >/dev/null
fi
[ -e "/var/lib/systemd/linger/$ME" ] || sudo loginctl enable-linger "$ME"

changed=0
sudo install -m 0755 "$HERE/system/mem-sampler.sh" /usr/local/bin/mem-sampler.sh
for u in mem-sampler.service mem-sampler.timer; do
  cmp -s "$HERE/system/$u" "/etc/systemd/system/$u" || { sudo install -m 0644 "$HERE/system/$u" "/etc/systemd/system/$u"; changed=1; }
done
[ "$changed" -eq 1 ] && sudo systemctl daemon-reload
sudo systemctl enable --now mem-sampler.timer >/dev/null 2>&1
echo "sysctl, swap, linger, mem-sampler: done"

# -------- 3. nix + devbox ----------------------------------------------------

step "nix and devbox"
if [ ! -d /nix/store ]; then
  curl -fsSL https://nixos.org/nix/install | sh -s -- --daemon --yes
fi
# shellcheck source=/dev/null
[ -e /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
if ! have devbox; then
  curl -fsSL https://get.jetify.com/devbox | bash -s -- -f
fi
GLOBAL_DIR="$HOME/.local/share/devbox/global/default"
mkdir -p "$GLOBAL_DIR"
cmp -s "$HERE/devbox-global.json" "$GLOBAL_DIR/devbox.json" || cp "$HERE/devbox-global.json" "$GLOBAL_DIR/devbox.json"
devbox global install
DEVBOX_BIN="$GLOBAL_DIR/.devbox/nix/profile/default/bin"
# mosh's client starts mosh-server over a non-interactive ssh session.
[ -x "$DEVBOX_BIN/mosh-server" ] && sudo ln -sfn "$DEVBOX_BIN/mosh-server" /usr/local/bin/mosh-server
if have rustup || [ -x "$DEVBOX_BIN/rustup" ]; then
  PATH="$DEVBOX_BIN:$PATH" rustup default >/dev/null 2>&1 || PATH="$DEVBOX_BIN:$PATH" rustup default stable
fi

# -------- 4. PATH ------------------------------------------------------------

step "PATH for interactive, non-interactive and systemd --user processes"
# The deck daemon is started over a non-interactive ssh session and every agent
# inherits its PATH. Ubuntu's .bashrc returns early for non-interactive shells,
# so this block goes at the TOP of it, before that guard.
PATH_BLOCK="$PATH_OPEN
$(cat <<'EOF'
for d in /nix/var/nix/profiles/default/bin "$HOME/.local/share/devbox/global/default/.devbox/nix/profile/default/bin" "$HOME/.opencode/bin" "$HOME/.local/bin"; do
  case ":$PATH:" in *":$d:"*) ;; *) [ -d "$d" ] && PATH="$d:$PATH" ;; esac
done
export PATH
EOF
)
$PATH_CLOSE"
touch "$HOME/.bashrc"
# The option blocks below are found by their markers, everything from an
# opening marker to its close is replaced, and they are written back after the
# PATH block's close, so check every marker before this layer writes anything,
# whichever flags were given. A block left open, a stray close, a close of
# another kind or a second PATH block stops the run here, because skipping to
# the end of the file would drop whatever follows the marker. So does a line
# that starts like a marker (in any case, with spaces or tabs around its `#` and
# arrows) but is not exactly one of the eight markers at the top of this script, trailing whitespace or a CRLF
# ending included, since the rewrite would read it as ordinary text.
if ! awk -v file="$HOME/.bashrc" \
    -v po="$PATH_OPEN" -v pc="$PATH_CLOSE" -v vo="$VALS_OPEN" -v vc="$VALS_CLOSE" -v ro="$RELAY_OPEN" -v rc="$RELAY_CLOSE" \
    -v xo="$REXEC_OPEN" -v xc="$REXEC_CLOSE" '
  function fail(msg) { print "bootstrap.sh: " file ": " msg > "/dev/stderr"; bad = 1; exit 1 }
  BEGIN {
    kind[po] = "PATH"; kind[vo] = "USE_VALS"; kind[ro] = "RELAY"; kind[xo] = "REMOTE_EXEC"
    opens[po] = opens[vo] = opens[ro] = opens[xo] = 1
    kind[pc] = "PATH"; kind[vc] = "USE_VALS"; kind[rc] = "RELAY"; kind[xc] = "REMOTE_EXEC"
  }
  tolower($0) ~ /^[ \t]*#[ \t]*(>>>|<<<)[ \t]*dad-box/ {
    if (!($0 in kind)) {
      why = ""; if ($0 ~ /[ \t\r]$/) why = " (it ends in whitespace or a carriage return)"
      shown = $0; gsub(/\r/, "\\r", shown)
      fail("line " NR " looks like a dad-box marker but is not one this script writes" why ": " shown)
    }
    k = kind[$0]
    if ($0 in opens) {
      if (open != "") fail("the dad-box " open " block opened on line " at " has no closing marker before line " NR)
      if (k == "PATH" && path_at) fail("line " NR " opens a second dad-box PATH block; the first opened on line " path_at)
      if (k == "PATH") path_at = NR
      open = k; at = NR; next
    }
    if (open == "") fail("line " NR " closes a dad-box " k " block that no marker opened")
    if (k != open) fail("the dad-box " open " block opened on line " at " is closed by a dad-box " k " marker on line " NR)
    open = ""
  }
  END {
    if (bad) exit 1
    if (open != "") fail("the dad-box " open " block opened on line " at " has no closing marker")
  }
' "$HOME/.bashrc"; then
  echo "bootstrap.sh: left $HOME/.bashrc unchanged; fix or delete that block by hand, then re-run" >&2
  exit 1
fi
if ! grep -qxF "$PATH_OPEN" "$HOME/.bashrc"; then
  # A copy that fails part-way is removed and ~/.bashrc is left as it was.
  if ! { printf '%s\n\n' "$PATH_BLOCK" && cat "$HOME/.bashrc"; } > "$HOME/.bashrc.dad-box"; then
    rm -f "$HOME/.bashrc.dad-box"
    echo "bootstrap.sh: could not add the dad-box PATH block to $HOME/.bashrc and left it unchanged; fix the cause above, then re-run" >&2
    exit 1
  fi
  mv "$HOME/.bashrc.dad-box" "$HOME/.bashrc"
fi
# USE_VALS gets the same placement for the same reason: the daemon's environment
# is every agent's, and devbox.json's init_hook loads .env.vals.yaml only when
# it is set. So does OTEL_EXPORTER_OTLP_ENDPOINT with --relay: the init_hook
# keeps an endpoint the shell already has, so a vals shell inherits the local
# relay rather than the direct endpoint unless a later setting overrides it
# (the repository's .env, or an export further down ~/.bashrc). So does
# DAD_REMOTE_EXEC with --remote-exec, which the agents the daemon starts and
# `cargo xtask affected-checks` read. Each sits in its own marked block right
# after the PATH block, and they are rewritten in place.
OPTION_BLOCKS=()
if [ "$USE_VALS_FLAG" = 1 ] || grep -qxF "$VALS_OPEN" "$HOME/.bashrc"; then
  USE_VALS_FLAG=1
  OPTION_BLOCKS+=("$VALS_OPEN
export USE_VALS=true
$VALS_CLOSE")
fi
if [ "$RELAY_FLAG" = 1 ] || grep -qxF "$RELAY_OPEN" "$HOME/.bashrc"; then
  RELAY_FLAG=1
  OPTION_BLOCKS+=("$RELAY_OPEN
export OTEL_EXPORTER_OTLP_ENDPOINT=$RELAY_ENDPOINT
$RELAY_CLOSE")
fi
if [ "$REMOTE_EXEC_FLAG" = 1 ]; then
  OPTION_BLOCKS+=("$REXEC_OPEN
export DAD_REMOTE_EXEC=rwx
$REXEC_CLOSE")
fi
if [ ${#OPTION_BLOCKS[@]} -gt 0 ]; then
  # Drops any earlier copy of the blocks and the blank lines around them, then
  # writes them back after the PATH block, so a re-run leaves the file unchanged.
  # The markers were checked at the top of this layer, so every opening marker
  # here has its own close and there is exactly one PATH close to write after.
  # A rewrite that fails part-way leaves the blocks in ~/.bashrc as they were
  # and removes its partial copy; a PATH block this run added just above stays,
  # so the file is not always byte-identical.
  if ! awk -v block="$(printf '%s\n\n' "${OPTION_BLOCKS[@]}")" \
      -v pc="$PATH_CLOSE" -v vo="$VALS_OPEN" -v vc="$VALS_CLOSE" -v ro="$RELAY_OPEN" -v rc="$RELAY_CLOSE" \
      -v xo="$REXEC_OPEN" -v xc="$REXEC_CLOSE" '
    $0 == vo || $0 == ro || $0 == xo { open = 1; next }
    $0 == vc || $0 == rc || $0 == xc { open = 0; next }
    open { next }
    gap && /^$/ { next }
    gap { print ""; gap = 0 }
    { print }
    $0 == pc { print ""; print block; gap = 1 }
  ' "$HOME/.bashrc" > "$HOME/.bashrc.dad-box"; then
    rm -f "$HOME/.bashrc.dad-box"
    echo "bootstrap.sh: could not rewrite the dad-box blocks in $HOME/.bashrc and left them as they were (a dad-box PATH block added by this run stays); fix the cause above, then re-run" >&2
    exit 1
  fi
  mv "$HOME/.bashrc.dad-box" "$HOME/.bashrc"
fi
# shellcheck disable=SC2016  # written literally, expanded by the interactive shell
grep -qF 'devbox global shellenv' "$HOME/.bashrc" || \
  printf '\n# dad-box: devbox global tools in interactive shells\neval "$(devbox global shellenv)"\n' >> "$HOME/.bashrc"
mkdir -p "$HOME/.config/environment.d"
# Named to sort LAST: Ubuntu's /usr/lib/environment.d/99-environment.conf resets
# PATH from /etc/environment, and files are applied in name order.
rm -f "$HOME/.config/environment.d/10-dad-box.conf"
cat > "$HOME/.config/environment.d/999-dad-box.conf" <<'EOF'
PATH=${HOME}/.local/bin:${HOME}/.opencode/bin:${HOME}/.local/share/devbox/global/default/.devbox/nix/profile/default/bin:/nix/var/nix/profiles/default/bin:${PATH}
EOF
[ "$USE_VALS_FLAG" = 1 ] && echo 'USE_VALS=true' >> "$HOME/.config/environment.d/999-dad-box.conf"
[ "$RELAY_FLAG" = 1 ] && echo "OTEL_EXPORTER_OTLP_ENDPOINT=$RELAY_ENDPOINT" >> "$HOME/.config/environment.d/999-dad-box.conf"
[ "$REMOTE_EXEC_FLAG" = 1 ] && echo 'DAD_REMOTE_EXEC=rwx' >> "$HOME/.config/environment.d/999-dad-box.conf"
# The user manager reads environment.d only when it starts, and linger started
# it before this file existed: apply the same PATH to the running manager too.
systemctl --user set-environment "PATH=$HOME/.local/bin:$HOME/.opencode/bin:$HOME/.local/share/devbox/global/default/.devbox/nix/profile/default/bin:/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" || echo "  warning: could not update the running systemd --user manager (applies at next login)"
if [ "$USE_VALS_FLAG" = 1 ]; then
  systemctl --user set-environment USE_VALS=true || echo "  warning: could not set USE_VALS in the running systemd --user manager (applies at next login)"
else
  systemctl --user unset-environment USE_VALS || true
fi
if [ "$RELAY_FLAG" = 1 ]; then
  systemctl --user set-environment "OTEL_EXPORTER_OTLP_ENDPOINT=$RELAY_ENDPOINT" || echo "  warning: could not set OTEL_EXPORTER_OTLP_ENDPOINT in the running systemd --user manager (applies at next login)"
elif [ "$(systemctl --user show-environment 2>/dev/null | sed -n 's/^OTEL_EXPORTER_OTLP_ENDPOINT=//p')" = "$RELAY_ENDPOINT" ]; then
  # Off: clear only the value this script writes, never an endpoint the
  # operator set in the manager some other way.
  systemctl --user unset-environment OTEL_EXPORTER_OTLP_ENDPOINT || true
fi
if [ "$REMOTE_EXEC_FLAG" = 1 ]; then
  systemctl --user set-environment DAD_REMOTE_EXEC=rwx || echo "  warning: could not set DAD_REMOTE_EXEC in the running systemd --user manager (applies at next login)"
elif [ "$(systemctl --user show-environment 2>/dev/null | sed -n 's/^DAD_REMOTE_EXEC=//p')" = rwx ]; then
  # Off: clear only the value this script writes, as for the relay endpoint.
  systemctl --user unset-environment DAD_REMOTE_EXEC || true
fi
# shellcheck source=/dev/null
eval "$(awk -v po="$PATH_OPEN" -v pc="$PATH_CLOSE" '$0 == po { on = 1 } on { print } $0 == pc { on = 0 }' "$HOME/.bashrc")"

# -------- 5. agents and the deck --------------------------------------------

step "agents"
[ -x "$HOME/.local/bin/claude" ]    || curl -fsSL https://claude.ai/install.sh | bash
# OpenCode 2.x on PATH (PRD #2050): this repo's agents run OpenCode 2.
# Installed from npm, pinned to the release the deck's OpenCode 2 support was
# measured on, so npm checks each tarball against the registry's integrity
# hash (no unverified `curl | bash`). `@opencode/cli`'s postinstall puts the
# platform binary from its pinned optional dependency in place and runs it
# `--version`. It goes under its own prefix, and ~/.opencode/bin/opencode —
# already on PATH — becomes a link to it. 2.x prints `opencode v2.2.5`, 1.x
# prints `1.19.5`. The link is made only when there is no binary there, the
# one there positively reports 1.x, or it is already this link: a 2.x
# installed some other way is left alone, and a binary that answers nothing
# readable is reported, not replaced, since a broken 2.x cannot be told from
# anything else that way.
OPENCODE_2_VERSION=2.2.5
OPENCODE_2_DIR="$HOME/.local/share/dot-agent-deck/opencode-2"
OPENCODE_2_BIN="$OPENCODE_2_DIR/node_modules/.bin/opencode"
OC_BIN="$HOME/.opencode/bin/opencode"
oc_major() { "$OC_BIN" --version 2>/dev/null | head -n1 | sed 's/^opencode //; s/^v//; s/\..*//'; }
oc2_version() { "$OPENCODE_2_BIN" --version 2>/dev/null | head -n1 | sed 's/^opencode //; s/^v//'; }
install_opencode_2() {
  if [ "$(oc2_version)" != "$OPENCODE_2_VERSION" ]; then
    mkdir -p "$OPENCODE_2_DIR"
    npm install --prefix "$OPENCODE_2_DIR" --no-fund --no-audit --no-save "@opencode/cli@$OPENCODE_2_VERSION"
    # As for 1.x below: an npm that holds install scripts back leaves the
    # package's placeholder; the user's `ignore-scripts` policy is honoured
    # and reported, not overridden.
    if [ "$(oc2_version)" != "$OPENCODE_2_VERSION" ]; then
      if [ "$(npm config get ignore-scripts 2>/dev/null)" = true ]; then
        echo "  warning: npm's ignore-scripts is set, so @opencode/cli's postinstall did not run; to finish the OpenCode $OPENCODE_2_VERSION install, run it yourself: (cd $OPENCODE_2_DIR/node_modules/@opencode/cli && node ./postinstall.mjs)"
      else
        (cd "$OPENCODE_2_DIR/node_modules/@opencode/cli" && node ./postinstall.mjs)
      fi
    fi
  fi
  if [ "$(oc2_version)" = "$OPENCODE_2_VERSION" ]; then
    mkdir -p "$(dirname "$OC_BIN")"
    ln -sfn "$OPENCODE_2_BIN" "$OC_BIN"
  else
    echo "  warning: $OPENCODE_2_BIN is not OpenCode $OPENCODE_2_VERSION; $OC_BIN left as it is"
  fi
}
if [ ! -e "$OC_BIN" ] || [ "$(readlink "$OC_BIN" 2>/dev/null)" = "$OPENCODE_2_BIN" ] \
   || [ "$(oc_major)" = 0 ] || [ "$(oc_major)" = 1 ]; then
  install_opencode_2
elif ! [ "$(oc_major)" -ge 2 ] 2>/dev/null; then
  echo "  warning: $OC_BIN does not report a version; left as it is (to replace it with OpenCode $OPENCODE_2_VERSION, remove it and run this script again)"
fi
# OpenCode 1.x, pinned and kept OFF PATH, for the lane-2 tests that keep the
# deck's OpenCode 1 support verifiable (PRD #2050). The binary they run is
#   ~/.local/share/dot-agent-deck/opencode-1/bin/opencode
# (select it with DOT_AGENT_DECK_OPENCODE_TEST_BIN): a stable link into npm's
# layout under that prefix, which no PATH includes.
OPENCODE_1_VERSION=1.19.5
OPENCODE_1_DIR="$HOME/.local/share/dot-agent-deck/opencode-1"
oc1_version() { "$OPENCODE_1_DIR/bin/opencode" --version 2>/dev/null | head -n1; }
if [ "$(oc1_version)" != "$OPENCODE_1_VERSION" ]; then
  mkdir -p "$OPENCODE_1_DIR/bin"
  npm install --prefix "$OPENCODE_1_DIR" --no-fund --no-audit --no-save "opencode-ai@$OPENCODE_1_VERSION"
  ln -sfn ../node_modules/.bin/opencode "$OPENCODE_1_DIR/bin/opencode"
  # The package's postinstall puts the platform binary in place. An npm that
  # holds install scripts back leaves its placeholder; when that is the
  # user's `ignore-scripts` policy it is honoured and reported, not overridden.
  if [ "$(oc1_version)" != "$OPENCODE_1_VERSION" ]; then
    if [ "$(npm config get ignore-scripts 2>/dev/null)" = true ]; then
      echo "  warning: npm's ignore-scripts is set, so opencode-ai's postinstall did not run; to finish the OpenCode $OPENCODE_1_VERSION install, run it yourself: (cd $OPENCODE_1_DIR/node_modules/opencode-ai && node ./postinstall.mjs)"
    else
      (cd "$OPENCODE_1_DIR/node_modules/opencode-ai" && node ./postinstall.mjs)
    fi
  fi
  [ "$(oc1_version)" = "$OPENCODE_1_VERSION" ] ||
    echo "  warning: $OPENCODE_1_DIR/bin/opencode is not OpenCode $OPENCODE_1_VERSION; the OpenCode 1.x lane-2 tests will skip"
fi
npm_missing=()
have codex || npm_missing+=(@openai/codex)
have pi    || npm_missing+=(@earendil-works/pi-coding-agent)
[ ${#npm_missing[@]} -eq 0 ] || sudo npm install -g --no-fund --no-audit "${npm_missing[@]}"
# An installed Codex older than the repo's minimum (require-codex-version.sh,
# beside this script) is upgraded, not only a missing one: `codex-big` needs it
# for its model. The upgrade replaces the root-global npm install; if the Codex
# on PATH is still too old afterwards, a user-local copy shadows it, and that is
# reported rather than passed over.
hash -r
if have codex && ! "$HERE/require-codex-version.sh" 2>/dev/null; then
  sudo npm install -g --no-fund --no-audit @openai/codex@latest
  hash -r
  "$HERE/require-codex-version.sh" ||
    echo "  warning: the Codex on PATH ($(command -v codex)) is still too old after upgrading the global install; upgrade that copy"
fi
# Devin's installer ends by running `devin setup`, an interactive login, which
# fails without a terminal. The install itself is complete by then, so feed it
# no input and judge by whether the binary landed.
if ! have devin; then
  bash -c "$(curl -fsSL https://cli.devin.ai/install.sh)" </dev/null || [ -x "$HOME/.local/bin/devin" ]
fi
have sem   || curl -fsSL https://storage.googleapis.com/sem-cli-releases/get.sh | bash
# The rwx CLI, for this repo's RWX CI backend and remote sandbox
# (docs/develop/rwx.md). Its release publishes no checksums; logging in
# (`rwx login`) is the operator's step.
if ! have rwx; then
  case "$(uname -m)" in
    x86_64|aarch64)
      mkdir -p "$HOME/.local/bin"
      curl -fsSL -o "$HOME/.local/bin/.rwx.part" \
        "https://github.com/rwx-cloud/cli/releases/latest/download/rwx-linux-$(uname -m)"
      chmod +x "$HOME/.local/bin/.rwx.part"
      mv "$HOME/.local/bin/.rwx.part" "$HOME/.local/bin/rwx" ;;
    *) echo "  warning: no rwx CLI release for $(uname -m); skipped" ;;
  esac
fi
hash -r

step "dot-agent-deck"
if [ ! -x "$HOME/.local/bin/dot-agent-deck" ]; then
  [ -n "$BIN_ARCH" ] || { echo "unsupported arch $(uname -m)" >&2; exit 1; }
  mkdir -p "$HOME/.local/bin"
  curl -fsSL -o "$HOME/.local/bin/.dot-agent-deck.part" \
    "https://github.com/vfarcic/dot-agent-deck/releases/latest/download/dot-agent-deck-linux-$BIN_ARCH"
  chmod +x "$HOME/.local/bin/.dot-agent-deck.part"
  mv "$HOME/.local/bin/.dot-agent-deck.part" "$HOME/.local/bin/dot-agent-deck"
fi
# `hooks install` defaults to Claude Code alone (so does `remote add`); install
# for every agent the deck ingests hooks from. Pi needs no step: the deck
# materializes its extension each time it spawns a Pi pane.
for agent in claude-code opencode codex devin; do
  "$HOME/.local/bin/dot-agent-deck" hooks install --agent "$agent" | sed "s/^/  [$agent] /"
done

# -------- 6. the relay and the resource collector (--relay) ------------------

if [ "$RELAY_FLAG" = 1 ] && [ "$RELAY_INSTALL" = 0 ]; then
  echo; echo "Note: the RELAY block points this box at $RELAY_ENDPOINT, but this script did not start the relay here; give --relay to have it install and run one (it refuses while something else listens on port 4318)."
fi
if [ "$RELAY_INSTALL" = 1 ]; then
  step "telemetry relay (container $RELAY_CONTAINER, listening on $RELAY_ENDPOINT)"
  [ -d "$RELAY_DIR" ] || install -d -m 0700 "$RELAY_DIR"
  # A key file the preflight found missing is written from the environment,
  # through a private temporary file; nothing here echoes a value.
  if [ ! -f "$HONEYCOMB_ENV" ]; then
    ( umask 077; printf 'HONEYCOMB_API_KEY=%s\n' "${HONEYCOMB_API_KEY:-${HONEYCOMB_INGEST_KEY:-}}" > "$HONEYCOMB_ENV.dad-box" )
    mv "$HONEYCOMB_ENV.dad-box" "$HONEYCOMB_ENV"
    echo "  wrote $HONEYCOMB_ENV from the environment"
  fi
  if [ ! -f "$DASH0_ENV" ] && [ -n "${DASH0_ENDPOINT:-}" ] && [ -n "${DASH0_AUTH_TOKEN:-}" ]; then
    ( umask 077; printf 'DASH0_ENDPOINT=%s\nDASH0_AUTH_TOKEN=%s\n' "$DASH0_ENDPOINT" "$DASH0_AUTH_TOKEN" > "$DASH0_ENV.dad-box" )
    mv "$DASH0_ENV.dad-box" "$DASH0_ENV"
    echo "  wrote $DASH0_ENV from the environment"
  fi
  RELAY_ENV_ARGS=(--env-file "$HONEYCOMB_ENV")
  if [ -f "$DASH0_ENV" ]; then
    RELAY_ENV_ARGS+=(--env-file "$DASH0_ENV")
    cp "$HERE/relay/config.yaml" "$RELAY_CONFIG.dad-box"
  else
    # No Dash0 keys: the same config without its Dash0 exporter, the comment
    # above it and its place in each pipeline. The Collector refuses to start
    # with an unset ${env:DASH0_ENDPOINT}, so the exporter cannot stay.
    awk '
      /^  #/ { held = held $0 "\n"; next }
      /^  otlphttp\/dash0:$/ { held = ""; dropping = 1; next }
      dropping && /^    / { next }
      { dropping = 0; printf "%s", held; held = ""; gsub(/, otlphttp\/dash0\]/, "]"); print }
      END { printf "%s", held }
    ' "$HERE/relay/config.yaml" > "$RELAY_CONFIG.dad-box"
    if grep -qi dash0 "$RELAY_CONFIG.dad-box"; then
      rm -f "$RELAY_CONFIG.dad-box"
      echo "bootstrap.sh: could not take Dash0 out of $HERE/relay/config.yaml; the relay is left as it was" >&2
      exit 1
    fi
    echo "  no $DASH0_ENV: running without the Dash0 exporter"
  fi
  chmod 0644 "$RELAY_CONFIG.dad-box"   # read by the Collector's non-root user, whatever the umask
  RELAY_RUN_ARGS=(--name "$RELAY_CONTAINER" --restart unless-stopped --network host
                  -v "/:/hostfs:ro,rslave" -v "$RELAY_CONFIG:/etc/otelcol-contrib/config.yaml:ro")
  # The Collector checks the new config, with the keys it will run with, before
  # it replaces the one the running relay uses.
  # It needs /hostfs as the relay has it: the hostmetrics receiver's
  # validation stats its root_path.
  if ! sudo docker run --rm "${RELAY_ENV_ARGS[@]}" -v "/:/hostfs:ro,rslave" \
      -v "$RELAY_CONFIG.dad-box:/etc/otelcol-contrib/config.yaml:ro" \
      "$RELAY_IMAGE" validate --config /etc/otelcol-contrib/config.yaml; then
    rm -f "$RELAY_CONFIG.dad-box"
    echo "bootstrap.sh: the Collector rejected the relay config (above); the relay is left as it was" >&2
    exit 1
  fi
  # The container carries a fingerprint of what it was started from: image,
  # parameters, config, and each key file's inode, size and modification time
  # to the nanosecond (not its contents). A different one, or none, means
  # recreate it.
  relay_fp="$(
    {
      printf '%s\n' "$RELAY_IMAGE" "${RELAY_RUN_ARGS[@]}" "${RELAY_ENV_ARGS[@]}"
      cat "$RELAY_CONFIG.dad-box"
      for f in "$HONEYCOMB_ENV" "$DASH0_ENV"; do
        if [ -f "$f" ]; then stat -c '%n %i %s %y' "$f"; fi
      done
    } | sha256sum | cut -c1-16
  )"
  # A replacement interrupted after it set the previous relay aside (below)
  # left $RELAY_CONTAINER.prev, and maybe a $RELAY_CONTAINER it had not yet
  # seen stay up. Settle that first, so the backup is never thrown away: a
  # running $RELAY_CONTAINER is the replacement that made it, and the backup
  # is superseded; otherwise the replacement (if any) is dropped and the
  # backup and its config are put back.
  if sudo docker container inspect "$RELAY_CONTAINER.prev" >/dev/null 2>&1; then
    relay_main="$(sudo docker container inspect -f '{{.State.Running}}' "$RELAY_CONTAINER" 2>/dev/null || true)"
    if [ "$relay_main" = true ]; then
      sudo docker rm -f "$RELAY_CONTAINER.prev" >/dev/null
      rm -f "$RELAY_CONFIG.prev"
    else
      if [ -n "$relay_main" ]; then sudo docker rm -f "$RELAY_CONTAINER" >/dev/null; fi
      if [ -f "$RELAY_CONFIG.prev" ]; then mv "$RELAY_CONFIG.prev" "$RELAY_CONFIG"; fi
      sudo docker rename "$RELAY_CONTAINER.prev" "$RELAY_CONTAINER"
      echo "  recovered the relay an interrupted run had set aside"
    fi
  fi
  relay_now="$(sudo docker container inspect -f '{{index .Config.Labels "dad-box.relay"}} {{.State.Running}}' "$RELAY_CONTAINER" 2>/dev/null || true)"
  if [ "$relay_now" = "$relay_fp true" ] || [ "$relay_now" = "$relay_fp false" ]; then
    # Same fingerprint: the container runs this config. Put it back in place
    # should the installed file have been edited since.
    if cmp -s "$RELAY_CONFIG.dad-box" "$RELAY_CONFIG"; then rm -f "$RELAY_CONFIG.dad-box"; else mv "$RELAY_CONFIG.dad-box" "$RELAY_CONFIG"; fi
    if [ "$relay_now" = "$relay_fp true" ]; then
      echo "  running, up to date"
    else
      sudo docker start "$RELAY_CONTAINER" >/dev/null
      sleep 5
      [ "$(sudo docker container inspect -f '{{.State.Running}}' "$RELAY_CONTAINER" 2>/dev/null)" = true ] || {
        sudo docker logs --tail 20 "$RELAY_CONTAINER" >&2 || true
        echo "bootstrap.sh: the relay container does not stay up (its log is above)" >&2
        exit 1
      }
      echo "  started"
    fi
  else
    # Replace it, keeping the previous container (stopped, renamed) and config
    # until the new one has stayed up, and putting them back if it does not.
    # The config is backed up before the container is touched, so a failed
    # copy stops the run with the old relay still running. Once the old
    # container is set aside, every step goes through the restore below.
    rm -f "$RELAY_CONFIG.prev"
    if [ -f "$RELAY_CONFIG" ]; then cp -p "$RELAY_CONFIG" "$RELAY_CONFIG.prev"; fi
    if [ -n "$relay_now" ]; then
      sudo docker stop "$RELAY_CONTAINER" >/dev/null 2>&1 || true
      sudo docker rename "$RELAY_CONTAINER" "$RELAY_CONTAINER.prev"
    fi
    relay_up=0
    if mv "$RELAY_CONFIG.dad-box" "$RELAY_CONFIG" && sudo docker run -d "${RELAY_RUN_ARGS[@]}" "${RELAY_ENV_ARGS[@]}" --label "dad-box.relay=$relay_fp" \
         "$RELAY_IMAGE" --config /etc/otelcol-contrib/config.yaml >/dev/null; then
      sleep 5
      [ "$(sudo docker container inspect -f '{{.State.Running}}' "$RELAY_CONTAINER" 2>/dev/null)" = true ] && relay_up=1
    fi
    if [ "$relay_up" = 1 ]; then
      sudo docker rm -f "$RELAY_CONTAINER.prev" >/dev/null 2>&1 || true
      rm -f "$RELAY_CONFIG.prev" "$RELAY_CONFIG.dad-box"
      echo "  (re)created"
    else
      sudo docker logs --tail 20 "$RELAY_CONTAINER" >&2 || true
      sudo docker rm -f "$RELAY_CONTAINER" >/dev/null 2>&1 || true
      rm -f "$RELAY_CONFIG.dad-box"
      if [ -f "$RELAY_CONFIG.prev" ]; then mv "$RELAY_CONFIG.prev" "$RELAY_CONFIG"; fi
      if [ -n "$relay_now" ]; then
        sudo docker rename "$RELAY_CONTAINER.prev" "$RELAY_CONTAINER"
        sudo docker start "$RELAY_CONTAINER" >/dev/null || true
        echo "bootstrap.sh: the new relay container did not stay up (its log is above); the previous container and config are back" >&2
      else
        echo "bootstrap.sh: the relay container did not stay up (its log is above)" >&2
      fi
      exit 1
    fi
  fi

  touch "$RELAY_MANAGED"

  step "per-run resource collector (dot-agent-deck-collector user service)"
  COLLECTOR_BIN="$HOME/.local/bin/dot-agent-deck-collector"
  COLLECTOR_ENV="$HOME/.config/dot-agent-deck/collector.env"
  COLLECTOR_UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/dot-agent-deck-collector.service"
  collector_changed=0
  # Like the deck above: installed when missing, not upgraded. The release's
  # checksums.txt must vouch for the download.
  if [ ! -x "$COLLECTOR_BIN" ]; then
    [ -n "$BIN_ARCH" ] || { echo "unsupported arch $(uname -m)" >&2; exit 1; }
    asset="dot-agent-deck-collector-linux-$BIN_ARCH"
    base=https://github.com/vfarcic/dot-agent-deck/releases/latest/download
    mkdir -p "$HOME/.local/bin"
    curl -fsSL -o "$COLLECTOR_BIN.part" "$base/$asset"
    want="$(curl -fsSL "$base/checksums.txt" | awk -v a="$asset" '{ n = $2; sub(/^\*/, "", n) } n == a { print $1 }')"
    if [ -z "$want" ] || [ "$want" != "$(sha256sum "$COLLECTOR_BIN.part" | cut -d' ' -f1)" ]; then
      rm -f "$COLLECTOR_BIN.part"
      echo "bootstrap.sh: $asset does not match the release's checksums.txt; not installed" >&2
      exit 1
    fi
    chmod +x "$COLLECTOR_BIN.part"
    mv "$COLLECTOR_BIN.part" "$COLLECTOR_BIN"
    collector_changed=1
  fi
  mkdir -p "$(dirname "$COLLECTOR_ENV")" "$(dirname "$COLLECTOR_UNIT")"
  if [ ! -f "$COLLECTOR_ENV" ]; then
    ( umask 077; printf 'OTEL_EXPORTER_OTLP_ENDPOINT=%s\n' "$RELAY_ENDPOINT" > "$COLLECTOR_ENV" )
    collector_changed=1
  elif ! grep -qxF "OTEL_EXPORTER_OTLP_ENDPOINT=$RELAY_ENDPOINT" "$COLLECTOR_ENV"; then
    echo "  warning: $COLLECTOR_ENV does not set OTEL_EXPORTER_OTLP_ENDPOINT=$RELAY_ENDPOINT; left as it is"
  fi
  if ! cmp -s "$HERE/user/dot-agent-deck-collector.service" "$COLLECTOR_UNIT"; then
    install -m 0644 "$HERE/user/dot-agent-deck-collector.service" "$COLLECTOR_UNIT"
    systemctl --user daemon-reload || true
    collector_changed=1
  fi
  if systemctl --user enable --now dot-agent-deck-collector.service >/dev/null 2>&1; then
    [ "$collector_changed" -eq 0 ] || systemctl --user restart dot-agent-deck-collector.service
    echo "  $(systemctl --user is-active dot-agent-deck-collector.service 2>/dev/null || true)"
  else
    echo "  warning: could not start it in the systemd --user manager; run: systemctl --user enable --now dot-agent-deck-collector"
  fi
fi

# -------- 7. the repo ---------------------------------------------------------

if [ -n "$REPO_URL" ]; then
  step "repo $REPO_URL"
  REPO_DIR="${REPO_DIR:-$HOME/code/$(basename "$REPO_URL" .git)}"
  [ -d "$REPO_DIR/.git" ] || git clone "$REPO_URL" "$REPO_DIR"
  (cd "$REPO_DIR" && devbox install)
  # This repo's own orphan reaper (CLAUDE.md rule 14's neighbour, docs/develop/orphan-reaper.md).
  [ -x "$REPO_DIR/scripts/install-reaper-timer.sh" ] && "$REPO_DIR/scripts/install-reaper-timer.sh"
  # --desktop: the desktop app's browser tier runs in Playwright's Chromium and
  # WebKit; `--with-deps` installs their runtime libraries and fonts with apt
  # (docs/develop/desktop-gui.md, "The browser tier").
  if [ "$DESKTOP_FLAG" = 1 ] && [ -f "$REPO_DIR/desktop/package.json" ]; then
    step "Playwright browsers for $REPO_DIR/desktop"
    (cd "$REPO_DIR" && devbox run -- bash -c 'cd desktop && pnpm install --frozen-lockfile && pnpm exec playwright install --with-deps chromium webkit')
  fi
elif [ "$DESKTOP_FLAG" = 1 ]; then
  echo; echo "Note: --desktop without --repo installs no Playwright browsers; run \`pnpm exec playwright install --with-deps chromium webkit\` in the repo's desktop/ directory."
fi

# -------- 8. what is left for the operator -----------------------------------

step "versions"
for c in dot-agent-deck claude opencode codex pi devin sem rwx docker node devbox git gh; do
  v=MISSING
  if have "$c"; then
    case "$c" in sem|devbox) v="$("$c" version 2>/dev/null | head -n1)" ;; *) v="$("$c" --version 2>/dev/null | head -n1)" ;; esac
  fi
  printf '  %-15s %s\n' "$c" "${v:-installed}"
done

step "log these in (run each on the box)"
todo=0
check() { if ! eval "$2" >/dev/null 2>&1; then printf '  %-10s %s\n' "$1" "$3"; todo=1; fi; }
# shellcheck disable=SC2016  # each test is eval'd inside check()
check claude   '[ -s ~/.claude/.credentials.json ] || [ -n "${ANTHROPIC_API_KEY:-}" ]' 'claude   (then /login)'
check codex    '[ -s ~/.codex/auth.json ]'                    'codex login'
check opencode '[ -s ~/.local/share/opencode/auth.json ]'     'opencode auth login'
check pi       '[ -s ~/.pi/agent/auth.json ]'                 'pi   (then /login)'
check devin    '[ -s ~/.local/share/devin/credentials.toml ]' 'devin   (follow its login prompt)'
check gh       'gh auth status'                               'gh auth login && gh auth setup-git'
have rwx && check rwx 'rwx whoami'                            'rwx login'
if [ "$USE_VALS_FLAG" = 1 ]; then
  check gcloud '[ -s ~/.config/gcloud/application_default_credentials.json ]' 'gcloud auth login && gcloud auth application-default login'
fi
[ "$todo" -eq 0 ] && echo "  nothing — every agent is logged in"
if [ "$USE_VALS_FLAG" = 1 ] && pgrep -u "$ME" -f 'dot-agent-deck daemon serve' >/dev/null; then
  echo; echo "Note: a deck daemon is already running; its agents get USE_VALS only after it is stopped and started from a fresh login (an in-app restart keeps the old environment)."
fi
if [ "$RELAY_FLAG" = 1 ] && pgrep -u "$ME" -f 'dot-agent-deck daemon serve' >/dev/null; then
  echo; echo "Note: a deck daemon is already running; its agents send through the relay only after it is stopped and started from a fresh login (an in-app restart keeps the old environment)."
fi
if [ "$REMOTE_EXEC_FLAG" = 1 ] && pgrep -u "$ME" -f 'dot-agent-deck daemon serve' >/dev/null; then
  echo; echo "Note: a deck daemon is already running; its agents get DAD_REMOTE_EXEC only after it is stopped and started from a fresh login (an in-app restart keeps the old environment)."
fi
if [ "$REMOTE_EXEC_FLAG" = 1 ] && ! have rwx; then
  echo; echo "Note: DAD_REMOTE_EXEC=rwx is set but there is no rwx CLI on PATH, so compiles stay on this box until one is installed and logged in (rwx login)."
fi
if [ "$RELAY_FLAG" = 1 ] && [ -f "$HOME/.config/dot-agent-deck/otel.env" ]; then
  echo; echo "Note: nothing this script sets up reads ~/.config/dot-agent-deck/otel.env; the RELAY block in ~/.bashrc is the route to the relay now. Remove that file and whatever sources it."
fi
if ! id -nG | tr ' ' '\n' | grep -qx docker; then
  echo; echo "Note: log out and back in (or reconnect) for the docker group to apply."
fi
