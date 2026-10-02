#!/usr/bin/env bash
# Bring an Ubuntu box up to the dot-agent-deck dev box's capabilities — PRD #1279.
#
# Knows nothing about any cloud: scripts/box/inmotion.sh ships this directory
# to the box and runs it, but any Ubuntu 26.04 host with passwordless sudo
# works. Re-running it brings the box up to date; every step checks what is
# already there before changing anything.
#
#   bash bootstrap.sh                        # the box itself
#   bash bootstrap.sh --repo URL [--dir D]   # also clone URL (default dir ~/code/<name>) and devbox install it
#
# Layers, each assuming the one before:
#   1. apt: docker (Docker's repo), auditd, build-essential, nodejs/npm (runtime for npm-distributed agents)
#   2. system: sysctl tuning, 8 GB swap, linger, the mem-sampler timer
#   3. nix + devbox, then `devbox global` from devbox-global.json
#   4. PATH for non-interactive shells and systemd --user, so the deck daemon finds every agent
#   5. agents (claude, opencode, codex, pi, devin), sem, dot-agent-deck, `hooks install`
#   6. the optional repo
#   7. which agents still need logging in
#
# Nothing secret is installed. Logging the agents in is left to the operator.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_URL=""
REPO_DIR=""

while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO_URL="${2:?}"; shift 2 ;;
    --dir)  REPO_DIR="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "bootstrap.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

step() { echo; echo "==> $*"; }
have() { command -v "$1" >/dev/null 2>&1; }

sudo -n true 2>/dev/null || { echo "bootstrap.sh: needs passwordless sudo" >&2; exit 1; }
export DEBIAN_FRONTEND=noninteractive
ME="$(id -un)"

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
APT_PACKAGES=(build-essential auditd nodejs npm xz-utils ca-certificates curl git
              docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin)
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
PATH_BLOCK="$(cat <<'EOF'
# >>> dad-box PATH (scripts/box/bootstrap.sh) >>>
for d in /nix/var/nix/profiles/default/bin "$HOME/.local/share/devbox/global/default/.devbox/nix/profile/default/bin" "$HOME/.opencode/bin" "$HOME/.local/bin"; do
  case ":$PATH:" in *":$d:"*) ;; *) [ -d "$d" ] && PATH="$d:$PATH" ;; esac
done
export PATH
# <<< dad-box PATH <<<
EOF
)"
touch "$HOME/.bashrc"
if ! grep -qF '# >>> dad-box PATH' "$HOME/.bashrc"; then
  { printf '%s\n\n' "$PATH_BLOCK"; cat "$HOME/.bashrc"; } > "$HOME/.bashrc.dad-box" && mv "$HOME/.bashrc.dad-box" "$HOME/.bashrc"
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
# The user manager reads environment.d only when it starts, and linger started
# it before this file existed: apply the same PATH to the running manager too.
systemctl --user set-environment "PATH=$HOME/.local/bin:$HOME/.opencode/bin:$HOME/.local/share/devbox/global/default/.devbox/nix/profile/default/bin:/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" || echo "  warning: could not update the running systemd --user manager (applies at next login)"
# shellcheck source=/dev/null
eval "$(sed -n '/# >>> dad-box PATH/,/# <<< dad-box PATH/p' "$HOME/.bashrc")"

# -------- 5. agents and the deck --------------------------------------------

step "agents"
[ -x "$HOME/.local/bin/claude" ]    || curl -fsSL https://claude.ai/install.sh | bash
[ -x "$HOME/.opencode/bin/opencode" ] || curl -fsSL https://opencode.ai/install | bash
npm_missing=()
have codex || npm_missing+=(@openai/codex)
have pi    || npm_missing+=(@earendil-works/pi-coding-agent)
[ ${#npm_missing[@]} -eq 0 ] || sudo npm install -g --no-fund --no-audit "${npm_missing[@]}"
# An installed Codex older than the repo's minimum (scripts/require-codex-version.sh)
# is upgraded, not only a missing one: `codex-big` needs it for its model.
codex_check="$HERE/../require-codex-version.sh"
if have codex && [ -x "$codex_check" ] && ! "$codex_check" 2>/dev/null; then
  sudo npm install -g --no-fund --no-audit @openai/codex@latest
fi
# Devin's installer ends by running `devin setup`, an interactive login, which
# fails without a terminal. The install itself is complete by then, so feed it
# no input and judge by whether the binary landed.
if ! have devin; then
  bash -c "$(curl -fsSL https://cli.devin.ai/install.sh)" </dev/null || [ -x "$HOME/.local/bin/devin" ]
fi
have sem   || curl -fsSL https://storage.googleapis.com/sem-cli-releases/get.sh | bash
hash -r

step "dot-agent-deck"
if [ ! -x "$HOME/.local/bin/dot-agent-deck" ]; then
  case "$(uname -m)" in x86_64) arch=amd64 ;; aarch64) arch=arm64 ;; *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;; esac
  mkdir -p "$HOME/.local/bin"
  curl -fsSL -o "$HOME/.local/bin/.dot-agent-deck.part" \
    "https://github.com/vfarcic/dot-agent-deck/releases/latest/download/dot-agent-deck-linux-$arch"
  chmod +x "$HOME/.local/bin/.dot-agent-deck.part"
  mv "$HOME/.local/bin/.dot-agent-deck.part" "$HOME/.local/bin/dot-agent-deck"
fi
# `hooks install` defaults to Claude Code alone (so does `remote add`); install
# for every agent the deck ingests hooks from. Pi needs no step: the deck
# materializes its extension each time it spawns a Pi pane.
for agent in claude-code opencode codex devin; do
  "$HOME/.local/bin/dot-agent-deck" hooks install --agent "$agent" | sed "s/^/  [$agent] /"
done

# -------- 6. the repo ---------------------------------------------------------

if [ -n "$REPO_URL" ]; then
  step "repo $REPO_URL"
  REPO_DIR="${REPO_DIR:-$HOME/code/$(basename "$REPO_URL" .git)}"
  [ -d "$REPO_DIR/.git" ] || git clone "$REPO_URL" "$REPO_DIR"
  (cd "$REPO_DIR" && devbox install)
  # This repo's own orphan reaper (CLAUDE.md rule 14's neighbour, docs/develop/orphan-reaper.md).
  [ -x "$REPO_DIR/scripts/install-reaper-timer.sh" ] && "$REPO_DIR/scripts/install-reaper-timer.sh"
fi

# -------- 7. what is left for the operator -----------------------------------

step "versions"
for c in dot-agent-deck claude opencode codex pi devin sem docker node devbox git gh; do
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
[ "$todo" -eq 0 ] && echo "  nothing — every agent is logged in"
if ! id -nG | tr ' ' '\n' | grep -qx docker; then
  echo; echo "Note: log out and back in (or reconnect) for the docker group to apply."
fi
