#!/usr/bin/env bash
# Smoke-test the dot-agent-deck binary against an isolated sandbox.
#
# This script redirects every user-visible state dir to a tempdir so the
# smoke run does NOT attach to the developer's real daemon, render their
# real Claude sessions, or contend on their lock files. Without these
# overrides the TUI you launch picks up live sessions via the global
# hook socket — what you'd see is the user's actual workflow, not a
# clean smoke.
#
# Subcommands:
#   cli   — non-interactive CLI surface (version, help, init, validate)
#   tui   — launch the dashboard under tmux, capture-pane → screenshot,
#           drive the Ctrl+C quit dialog, verify clean exit.
#   all   — both, in order. Default.
#
# Env overrides:
#   DAD_BIN         path to the dot-agent-deck binary
#                   (default: $PWD/target/release/dot-agent-deck)
#   DAD_SCREENSHOT  where to write the TUI capture-pane snapshot
#                   (default: /tmp/dad-screenshot.txt)
set -euo pipefail

BIN="${DAD_BIN:-$PWD/target/release/dot-agent-deck}"
if [[ ! -x "$BIN" ]]; then
  echo "binary not found at $BIN" >&2
  echo "build first: cargo build --release" >&2
  exit 1
fi

SANDBOX="$(mktemp -d -t dad-driver-XXXXXXXX)"
SESSION="dad-driver-$$"
SCREENSHOT="${DAD_SCREENSHOT:-/tmp/dad-screenshot.txt}"

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

# Every knob the binary reads to find sockets / locks / state / config.
# Source of truth: src/config.rs + src/daemon.rs env-var lookups.
export DOT_AGENT_DECK_SOCKET="$SANDBOX/hook.sock"
export DOT_AGENT_DECK_ATTACH_SOCKET="$SANDBOX/attach.sock"
export DOT_AGENT_DECK_LOCK_DIR="$SANDBOX/locks"
export DOT_AGENT_DECK_STATE_DIR="$SANDBOX/state"
export DOT_AGENT_DECK_CONFIG="$SANDBOX/config"
export DOT_AGENT_DECK_SESSION="$SANDBOX/session.json"
export XDG_RUNTIME_DIR="$SANDBOX/xdg-runtime"
export XDG_STATE_HOME="$SANDBOX/xdg-state"
export HOME="$SANDBOX/home"
mkdir -p "$DOT_AGENT_DECK_LOCK_DIR" "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$HOME"

cli_smoke() {
  echo "== CLI smoke =="
  "$BIN" --version
  "$BIN" --help > /dev/null
  workdir="$SANDBOX/project"
  mkdir -p "$workdir"
  ( cd "$workdir" && "$BIN" init --path . )
  ( cd "$workdir" && "$BIN" validate --path . )
  echo "   init + validate OK in sandbox project"
}

tui_smoke() {
  echo "== TUI smoke =="
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  # 120×40 is wide enough for the default 3-column dashboard layout.
  tmux new-session -d -s "$SESSION" -x 120 -y 40 "$BIN"

  # Wait up to 8 s for the dashboard to render. The marker is the
  # bottom-row hotkey hint, which only appears once the TUI has drawn.
  for i in 1 2 3 4 5 6 7 8; do
    if tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -q "Ctrl+c: quit"; then
      break
    fi
    sleep 1
  done

  if ! tmux capture-pane -t "$SESSION" -p 2>/dev/null | grep -q "Ctrl+c: quit"; then
    echo "TUI did not render within 8s" >&2
    tmux capture-pane -t "$SESSION" -p >&2 || true
    return 1
  fi

  tmux capture-pane -t "$SESSION" -p > "$SCREENSHOT"
  echo "   screenshot saved to $SCREENSHOT ($(wc -l < "$SCREENSHOT") lines)"

  # Quit: Ctrl+C opens the Stop/Cancel dialog (Stop is default-selected),
  # Enter confirms shutdown. Two send-keys calls because the binary
  # reads the dialog state synchronously between key events.
  tmux send-keys -t "$SESSION" C-c
  sleep 0.5
  tmux send-keys -t "$SESSION" Enter

  for i in 1 2 3 4 5 6 7 8; do
    if ! tmux has-session -t "$SESSION" 2>/dev/null; then
      echo "   TUI exited cleanly"
      return 0
    fi
    sleep 1
  done

  echo "TUI did not exit after Ctrl+C + Enter" >&2
  tmux capture-pane -t "$SESSION" -p >&2 || true
  return 1
}

cmd="${1:-all}"
case "$cmd" in
  cli) cli_smoke ;;
  tui) tui_smoke ;;
  all) cli_smoke; tui_smoke ;;
  *) echo "usage: $0 [cli|tui|all]" >&2; exit 1 ;;
esac
echo "OK"
