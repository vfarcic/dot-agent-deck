#!/usr/bin/env bash
#
# The isolated sandbox behind `task run`, `task run-desktop`, `task run-all` and
# `task run-stop` (docs/develop/local-run.md). The Taskfile builds the binary
# first; this script only isolates, launches and tears down.
#
# Usage: sandbox-run.sh MODE CHECKOUT DIR SANDBOX PROFILE [ARG...]
#
#   MODE      tui      the TUI, in the foreground (`task run`)
#             desktop  the desktop GUI, in the foreground (`task run-desktop`)
#             all      the desktop in the background, the TUI in the foreground,
#                      and the desktop torn down when the TUI exits (`task run-all`)
#             stop     tear down a background desktop, then `daemon stop ARG...`
#                      against the sandbox daemon (`task run-stop`)
#   CHECKOUT  the Taskfile's own checkout; the desktop always runs from its desktop/
#   DIR       the config dir the TUI starts in
#   SANDBOX   the isolation state dir; empty means DIR/.dad-sandbox
#   PROFILE   release | debug; empty (stop only) means "the build that started it"
#
# A fifth mode, keep-desktop, is internal: `all` re-invokes this script with it
# to lead the desktop's process group. See keep_desktop.

set -euo pipefail

die() {
  echo "sandbox-run: $*" >&2
  exit 1
}

[ $# -ge 5 ] || die "usage: sandbox-run.sh MODE CHECKOUT DIR SANDBOX PROFILE [ARG...]"
mode=$1
checkout=$2
dir=$3
sandbox=$4
profile=$5
shift 5

[ -n "$sandbox" ] || sandbox="$dir/.dad-sandbox"
case $mode in
  stop)
    if [ ! -d "$sandbox" ]; then
      echo "no sandbox at $sandbox — nothing to stop"
      exit 0
    fi
    ;;
  tui | desktop | all | keep-desktop) mkdir -p "$sandbox" ;;
  *) die "unknown mode '$mode'" ;;
esac
# Absolute, because the desktop refuses a relative settings path outright, and
# a relative socket path would resolve against whatever cwd each child has.
sandbox=$(CDPATH='' cd -- "$sandbox" && pwd)

desktop_dir="$checkout/desktop"
desktop_log="$sandbox/desktop.log"
desktop_record="$sandbox/desktop.pgid"
desktop_exit_record="$sandbox/desktop.exit-status"
binary_record="$sandbox/binary"

# --- which binary ------------------------------------------------------------

if [ -n "$profile" ]; then
  bin="$checkout/target/$profile/dot-agent-deck"
else
  # `stop` only needs something that speaks this checkout's protocol to the
  # sandbox socket. Prefer the build that started the sandbox, so `task
  # run-stop` works after `task run-all` without repeating its PROFILE.
  bin=""
  recorded=""
  [ -f "$binary_record" ] && recorded=$(cat "$binary_record")
  for candidate in "$recorded" "$checkout/target/release/dot-agent-deck" "$checkout/target/debug/dot-agent-deck"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
      bin=$candidate
      break
    fi
  done
  [ -n "$bin" ] || die "no dot-agent-deck build under $checkout/target — nothing can talk to the sandbox daemon"
fi
[ -x "$bin" ] || die "no dot-agent-deck binary at $bin"

# --- isolation ---------------------------------------------------------------
#
# Every process below inherits these, and `stop` sets them before its `daemon
# stop` — which is why they live in one place: a stop path missing the attach
# socket would stop YOUR daemon, not the sandbox's.

# Without these four, a branch build's build-version handshake (PRD #103/#161)
# SIGTERMs the daemon your installed build is using — silently when no agents
# are live — and switching back bounces it again. The documented production
# overrides (src/config.rs, src/platform/paths.rs).
export DOT_AGENT_DECK_ATTACH_SOCKET="$sandbox/attach.sock"
export DOT_AGENT_DECK_SOCKET="$sandbox/hook.sock"
export DOT_AGENT_DECK_STATE_DIR="$sandbox/state"
export DOT_AGENT_DECK_SESSION="$sandbox/session.toml"

# The daemon loads the GLOBAL schedules.toml at startup and fires every enabled
# entry (src/daemon.rs), so without this a sandbox daemon runs your real
# scheduled tasks a second time — and a registered schedule also keeps it from
# ever idling out. Absent file = no schedules.
export DOT_AGENT_DECK_SCHEDULES="$sandbox/schedules.toml"

# The TUI and the daemon both append to this when it is set, and it is often
# set in a shell profile — which sent the sandbox's lines into your real
# deck.log (CLAUDE.md rule 12). Set unconditionally, so the sandbox always has
# a log of its own.
export DOT_AGENT_DECK_LOG="$sandbox/deck.log"

# The desktop's own settings document (desktop/src-tauri/src/settings.rs). Its
# default is the real per-installation desktop.toml, whose deck selection and
# remote rows would have the sandbox desktop connect to your real decks, and
# whose Deck selector, zoom and settings sheet would write back to it. Absent
# file = defaults = the local deck only, which is the sandbox's.
export DOT_AGENT_DECK_DESKTOP_CONFIG="$sandbox/desktop.toml"

# Read only by the desktop, for Start deck / Replace deck
# (desktop/src-tauri/src/daemon_bridge.rs). Pinned so a daemon it starts is this
# build, not a sibling binary or whatever is first on PATH.
export DOT_AGENT_DECK_BINARY="$bin"

# Panes inherit this PATH, so an agent inside the sandbox that types a bare
# `dot-agent-deck` must reach the branch build — not whatever is installed.
# Without this the release binary on PATH wins and the miss is SILENT for every
# verb that exists in both builds, so a pane can "test the branch" while
# actually exercising the installed release.
PATH="$(dirname "$bin"):$PATH"
export PATH

# ...and the PATH prepend above is NOT enough on its own. Agents here are
# launched through `devbox run <script>` (see .dot-agent-deck.toml's role
# commands), and a NESTED devbox re-derives PATH from its own environment: the
# prepend above is DISCARDED, and devbox.json's init_hook then puts
# $HOME/.local/bin first — so a bare `dot-agent-deck` in a pane reaches the
# INSTALLED RELEASE. Measured: level-1 PATH starts with the build dir, level-2
# (the agent) starts with $HOME/.local/bin and has no build dir at all.
# Ordinary env vars DO survive that nesting, so hand the dir over as one and let
# init_hook re-prepend it inside every devbox layer.
DAD_DEV_BIN="$(dirname "$bin")"
export DAD_DEV_BIN

# --- the background desktop's process group ----------------------------------
#
# `tauri dev` is a tree — pnpm, the tauri CLI, vite, cargo and rustc while it
# builds, then the app and its WebKit processes — so killing its top process is
# not enough. It runs in a process group of its own, LED by a keeper: this
# script re-invoked as `keep-desktop` with the absolute sandbox path in its
# argv. The keeper stays alive until the group is killed, so the group id cannot
# be recycled while anything in it still runs, and `ps -o args=` on the leader
# is a portable identity check before any signal is sent to the group.

# Is $1 the live leader of a desktop group for THIS sandbox?
desktop_group_is_ours() {
  local pgid=$1 leader_pgid args
  case $pgid in '' | *[!0-9]*) return 1 ;; esac
  leader_pgid=$(ps -o pgid= -p "$pgid" 2>/dev/null | tr -d ' ') || return 1
  [ "$leader_pgid" = "$pgid" ] || return 1
  args=$(ps -ww -o args= -p "$pgid" 2>/dev/null) || return 1
  case $args in
    *"sandbox-run.sh keep-desktop "*" $sandbox "*) return 0 ;;
    *) return 1 ;;
  esac
}

# Does process group $1 still have a live (non-zombie) member?
group_has_members() {
  ps -A -o pgid= -o stat= | awk -v g="$1" '$1 == g && $2 !~ /^Z/ { found = 1 } END { exit !found }'
}

recorded_desktop_pgid() {
  [ -f "$desktop_record" ] || return 1
  local pgid
  pgid=$(cat "$desktop_record")
  desktop_group_is_ours "$pgid" || return 1
  echo "$pgid"
}

# SIGTERM the whole group, give it five seconds, then SIGKILL what is left.
stop_desktop_group() {
  local pgid=$1 i=0
  echo "stopping the sandbox desktop (process group $pgid)"
  kill -TERM -- "-$pgid" 2>/dev/null || true
  while [ "$i" -lt 50 ] && group_has_members "$pgid"; do
    sleep 0.1
    i=$((i + 1))
  done
  if group_has_members "$pgid"; then
    echo "the desktop did not exit on SIGTERM within 5s — sending SIGKILL" >&2
    kill -KILL -- "-$pgid" 2>/dev/null || true
  fi
}

stop_recorded_desktop() {
  local pgid
  if pgid=$(recorded_desktop_pgid); then
    stop_desktop_group "$pgid"
  fi
  rm -f "$desktop_record"
}

# --- desktop preflight -------------------------------------------------------

desktop_preflight() {
  command -v pnpm >/dev/null 2>&1 ||
    die "pnpm is not on PATH — run this from a \`devbox shell\` (docs/develop/desktop-gui.md#prerequisites)"
  [ -d "$desktop_dir/node_modules" ] ||
    die "the desktop's JavaScript dependencies are not installed — run \`pnpm install\` in $desktop_dir once"
  local pgid
  if pgid=$(recorded_desktop_pgid); then
    die "a desktop from an earlier run is still running against this sandbox (process group $pgid) — \`task run-stop\` stops it"
  fi
  # vite binds 1420 with strictPort (desktop/vite.config.ts), so a second `pnpm
  # dev` or `tauri dev` anywhere on this machine would fail the desktop — and
  # under `all` that failure would land in a log file behind the TUI.
  if (exec 3<>/dev/tcp/127.0.0.1/1420) 2>/dev/null || (exec 3<>/dev/tcp/::1/1420) 2>/dev/null; then
    die "port 1420 is in use, probably by another \`pnpm dev\` or \`tauri dev\` — the desktop's vite server needs it"
  fi
  if [ "$(uname -s)" = Linux ] && [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
    echo "note: neither DISPLAY nor WAYLAND_DISPLAY is set, so the desktop window may fail to open" >&2
  fi
}

config_note() {
  if [ ! -f "$dir/.dot-agent-deck.toml" ]; then
    echo "note: no .dot-agent-deck.toml in $dir — the deck will start with no" >&2
    echo "      modes/orchestrations configured. Pass DIR=<dir> to point elsewhere." >&2
  fi
}

# --- modes -------------------------------------------------------------------

run_tui() {
  config_note
  echo "$bin" >"$binary_record"
  echo "deck:    $bin"
  echo "config:  $dir"
  echo "sandbox: $sandbox   (task run-stop to shut its daemon down)"
  cd "$dir"
  exec "$bin"
}

run_desktop() {
  desktop_preflight
  echo "$bin" >"$binary_record"
  echo "desktop: $desktop_dir   (pnpm tauri dev)"
  echo "deck:    $bin   (Start deck launches it inside the sandbox)"
  echo "sandbox: $sandbox   (task run-stop to shut its daemon down)"
  cd "$desktop_dir"
  exec pnpm tauri dev
}

run_all() {
  desktop_preflight
  config_note
  echo "$bin" >"$binary_record"
  rm -f "$desktop_exit_record"
  : >"$desktop_log"

  # `set -m` for this one command only: it is what puts the keeper in a process
  # group of its own. The TUI below stays in this shell's group and keeps the
  # terminal.
  set -m
  bash "$checkout/scripts/sandbox-run.sh" keep-desktop "$checkout" "$dir" "$sandbox" "$profile" "$$" \
    </dev/null >>"$desktop_log" 2>&1 &
  local desktop_pgid=$!
  set +m
  echo "$desktop_pgid" >"$desktop_record"

  # Expanded now: the trap can fire after this function's locals are gone.
  # shellcheck disable=SC2064
  trap "finish_all $desktop_pgid" EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM

  echo "deck:    $bin"
  echo "config:  $dir"
  echo "desktop: $desktop_dir   (log: $desktop_log)"
  echo "sandbox: $sandbox   (task run-stop to shut its daemon down)"

  cd "$dir"
  "$bin"
}

finish_all() {
  local desktop_pgid=$1 exited_on_its_own=""
  # Teardown first, output last, and no errexit: when the terminal has hung up
  # every write to it fails, and under `set -e` the first failing echo would
  # abort this trap before the desktop was stopped.
  set +e
  [ -f "$desktop_exit_record" ] && exited_on_its_own=$(cat "$desktop_exit_record")
  if desktop_group_is_ours "$desktop_pgid"; then
    stop_desktop_group "$desktop_pgid"
  fi
  rm -f "$desktop_record"
  if [ -n "$exited_on_its_own" ]; then
    echo "the desktop exited on its own (status $exited_on_its_own) before the TUI did — last lines of $desktop_log:" >&2
    tail -n 20 "$desktop_log" >&2
  fi
  echo "desktop log: $desktop_log"
  echo "the sandbox daemon outlives the TUI, as with \`task run\` — \`task run-stop\` shuts it down"
}

# The controlling terminal of process $1, or nothing when it has none (`?` on
# Linux, `??` on macOS).
controlling_tty() {
  local tty
  tty=$(ps -o tty= -p "$1" 2>/dev/null | tr -d ' ')
  case $tty in '' | '?' | '??') ;; *) echo "$tty" ;; esac
}

keep_desktop() {
  local parent=$1 child status

  # The TUI lazy-spawns the sandbox daemon, and the desktop makes ONE
  # connect-only probe at launch. So wait until the daemon answers before
  # starting it, or a warm desktop can beat the daemon up and open on an error
  # that only a manual Reconnect clears. Bounded: after 30s it starts anyway.
  local i=0
  until "$bin" daemon status >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -ge 150 ] && break
    sleep 0.2
  done

  (cd "$desktop_dir" && exec pnpm tauri dev) &
  child=$!

  # Hold the group until `all` (or `task run-stop`) kills it, and take it down
  # ourselves if nothing is left to do that:
  #
  # - the shell that started us is gone without having done so — SIGKILL skips
  #   its EXIT trap;
  # - the terminal it ran in has hung up. The kernel signals only the SESSION
  #   LEADER on a hangup, so when that is `task` itself rather than an
  #   interactive shell that forwards SIGHUP to its jobs (`ssh -t host task
  #   run-all`, a tmux pane whose command is `task`), neither the TUI nor that
  #   shell hears about it and the TUI keeps running with no terminal. What the
  #   hangup does do is detach the whole session from the tty, so we watch that.
  local started_on_tty
  started_on_tty=$(controlling_tty "$$")
  local reason=""
  while [ -z "$reason" ]; do
    if [ "$(ps -o ppid= -p "$$" 2>/dev/null | tr -d ' ')" != "$parent" ]; then
      reason="the shell that started this desktop is gone"
    elif [ -n "$started_on_tty" ] && [ -z "$(controlling_tty "$$")" ]; then
      reason="the terminal this desktop was started from has hung up"
    else
      if [ -n "$child" ] && ! kill -0 "$child" 2>/dev/null; then
        status=0
        wait "$child" || status=$?
        echo "$status" >"$desktop_exit_record"
        echo "pnpm tauri dev exited with status $status"
        child=""
      fi
      sleep 1
    fi
  done
  echo "$reason — stopping the desktop"
  kill -TERM -- "-$$"
}

case $mode in
  tui) run_tui ;;
  desktop) run_desktop ;;
  all) run_all ;;
  keep-desktop) keep_desktop "${1:?keep-desktop needs the parent pid}" ;;
  stop)
    stop_recorded_desktop
    exec "$bin" daemon stop "$@"
    ;;
esac
