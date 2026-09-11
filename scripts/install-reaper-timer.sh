#!/usr/bin/env bash
# Install (or remove) the orphan-reaper systemd USER timer — no root required.
#
# The units are templates because a systemd unit needs an absolute path and this
# repo can live anywhere; this script substitutes the real checkout path.
#
#   scripts/install-reaper-timer.sh           # install and start
#   scripts/install-reaper-timer.sh --status  # show timer + last run
#   scripts/install-reaper-timer.sh --remove  # stop, disable, delete units

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
NAME=dot-agent-deck-reap-orphans

case "${1:-install}" in
  --status)
    systemctl --user status "$NAME.timer" --no-pager || true
    echo; systemctl --user list-timers "$NAME.timer" --no-pager || true
    echo; journalctl --user -u "$NAME.service" -n 20 --no-pager || true
    exit 0 ;;
  --remove)
    systemctl --user disable --now "$NAME.timer" 2>/dev/null || true
    rm -f "$UNIT_DIR/$NAME.timer" "$UNIT_DIR/$NAME.service"
    systemctl --user daemon-reload
    echo "Removed $NAME.timer and $NAME.service."
    exit 0 ;;
  install) ;;
  *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

command -v systemctl >/dev/null 2>&1 || { echo "systemd not available on this host" >&2; exit 1; }

mkdir -p "$UNIT_DIR"
sed "s|__REPO__|$REPO|g" "$REPO/scripts/systemd/$NAME.service.in" >"$UNIT_DIR/$NAME.service"
sed "s|__REPO__|$REPO|g" "$REPO/scripts/systemd/$NAME.timer.in"   >"$UNIT_DIR/$NAME.timer"

systemctl --user daemon-reload
systemctl --user enable --now "$NAME.timer"

echo "Installed and started $NAME.timer (reaping from $REPO)."
echo
systemctl --user list-timers "$NAME.timer" --no-pager || true
echo
echo "Inspect with: scripts/install-reaper-timer.sh --status"
echo "Remove with:  scripts/install-reaper-timer.sh --remove"
