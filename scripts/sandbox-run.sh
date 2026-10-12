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
#   SANDBOX   the isolation state dir; empty means the checkout's branch's own,
#             DIR/.dad-sandbox/branch-<slug>, or commit-<sha> on a detached
#             HEAD (see "which sandbox" below)
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

case $mode in
  tui | desktop | all | stop | keep-desktop) ;;
  *) die "unknown mode '$mode'" ;;
esac

# --- which sandbox -----------------------------------------------------------
#
# Every branch built from a checkout gets a sandbox, and so a daemon, of its
# own: DIR/.dad-sandbox/branch-<slug>, or DIR/.dad-sandbox/commit-<sha> on a
# detached HEAD. A run of another branch of this checkout therefore does not
# meet this one's daemon, nor the build-mismatch prompt that offers to restart
# it — with whatever agents are running under it. Two checkouts on branches of
# one name still share a sandbox under one DIR. A new commit on the SAME
# branch keeps the same sandbox on purpose: that is the build-stamp case
# (CLAUDE.md rule 15), whose documented recovery is `task run-stop` and run
# again against this same sandbox. An explicit SANDBOX is used as given.
#
# Socket paths are bounded. A Unix socket's path lives in sockaddr_un's
# sun_path, a fixed array of 104 bytes on macOS and the BSDs and 108 on Linux,
# one of which is the terminating NUL, so a path is at most 103 or 107 bytes;
# a longer one fails only when the daemon binds it, with an error that does not
# say why. The longest path the sandbox exports is SANDBOX/attach.sock, 12
# bytes past SANDBOX ("/attach.sock"). With ROOT = DIR/.dad-sandbox the default
# sandbox is ROOT/<slug>, so
#
#   len(ROOT) + 1 + len(slug) + 12 <= max   →   len(slug) <= max - len(ROOT) - 13
#
# e.g. ROOT = /Users/vfarcic/code/dot-agent-deck/.dad-sandbox is 47 bytes,
# which leaves 103 - 47 - 13 = 43 bytes of slug on macOS. A slug that does not
# fit is cut and given a hash of the full branch name (below), and when even
# the shortest slug cannot fit this refuses to start rather than let the daemon
# fail to bind.
case $(uname -s) in
  Darwin | *BSD | DragonFly) socket_path_max=103 ;;
  *) socket_path_max=107 ;;
esac
socket_path_tail="/attach.sock"

# The length of $1 in BYTES, which is what sun_path bounds. `${#var}` counts
# characters under a UTF-8 locale, so a DIR holding multibyte characters would
# pass a bound it does not fit. `wc -c` counts bytes on Linux and macOS alike,
# and LC_ALL=C stays on it rather than reaching the TUI.
byte_len() {
  printf '%s' "$1" | LC_ALL=C wc -c | tr -d ' '
}

# The slug for the branch CHECKOUT is on, at most $1 bytes long, or a refusal
# when even its shortest form is longer. Every slug starts with `branch-` or
# `commit-`, which no file of the sandbox layout from before per-branch
# sandboxes did (attach.sock, state, deck.log, …), so a branch can never be
# named onto one of that layout's files in the same ROOT.
sandbox_slug() {
  local budget=$1 kind raw clean slug hash keep
  # The full ref, stripped by hand: `--short` abbreviates to `heads/main` when
  # a tag `main` also exists, which would move the sandbox.
  if raw=$(git -C "$checkout" symbolic-ref --quiet HEAD 2>/dev/null); then
    raw=${raw#refs/heads/}
    kind=branch
  elif raw=$(git -C "$checkout" rev-parse --short=12 HEAD 2>/dev/null); then
    kind=commit
  else
    die "cannot tell which branch $checkout is on (is it a git checkout?) — pass SANDBOX=<dir> to name a sandbox"
  fi
  # A slug comes in two forms: KIND-NAME, the name as it is, and
  # KIND-PREFIX-HASH, a prefix of the name with anything outside
  # [A-Za-z0-9._-] turned into `-`, then `-` and eight hex digits of a CRC of
  # the full name (POSIX cksum, the same on Linux and macOS).
  #
  # The plain form is kept to names it cannot confuse, and the rest are hashed:
  #
  # - a name with a byte outside [A-Za-z0-9._-]: `a/b` and `a-b` would read
  #   the same once `/` is replaced;
  # - a name with an uppercase letter: macOS's default APFS is
  #   case-insensitive, so `Main` and `main` would be one directory;
  # - a name that already ENDS the way a hashed slug does, in `-` and eight
  #   lowercase hex digits: the plain form of `a-b-<hash of a/b>` would be the
  #   hashed form of `a/b` (PR #1672 review);
  # - a name whose plain form is longer than the budget.
  #
  # So a plain slug never ends in `-<8 hex>` and a hashed one always does: the
  # two forms cannot meet. Two plain slugs are equal only for one name, and two
  # hashed ones only when the names share both the kept prefix and the CRC.
  # `clean` is ASCII, so its length in characters is its length in bytes.
  clean=$(printf '%s' "$raw" | LC_ALL=C tr -c 'A-Za-z0-9._-' '-')
  slug="$kind-$clean"
  if [ "$clean" != "$raw" ] ||
    [ "$(printf '%s' "$clean" | LC_ALL=C tr -d 'A-Z')" != "$clean" ] ||
    printf '%s' "$clean" | LC_ALL=C grep -Eq -- '-[0-9a-f]{8}$' ||
    [ "${#slug}" -gt "$budget" ]; then
    # The shortest hashed slug keeps one byte of the name: KIND, `-`, one
    # byte, `-` and the eight-digit hash.
    [ "$budget" -ge $((${#kind} + 11)) ] ||
      die "$sandbox_root is too long to hold this ${kind}'s sandbox ($kind-$clean needs at least $((${#kind} + 11)) bytes of its name, and $budget fit) with socket paths inside the $socket_path_max bytes a Unix socket path can hold here — pass SANDBOX=<a shorter dir>"
    hash=$(printf '%s' "$raw" | cksum)
    # bash's printf, not awk's: some awks clamp %x at 2^31 - 1.
    hash=$(printf '%08x' "${hash%% *}")
    keep=$((budget - ${#kind} - 1 - 9))
    [ "$keep" -le "${#clean}" ] || keep=${#clean}
    slug="$kind-${clean:0:keep}-$hash"
  fi
  echo "$slug"
}

check_socket_path_fits() {
  local longest="$1$socket_path_tail" bytes
  bytes=$(byte_len "$longest")
  [ "$bytes" -le "$socket_path_max" ] ||
    die "the sandbox's socket path $longest is $bytes bytes, past the $socket_path_max a Unix socket path can hold here — pass SANDBOX=<a shorter dir>"
}

dir_abs=$(CDPATH='' cd -- "$dir" 2>/dev/null && pwd) || die "DIR $dir is not a directory"
sandbox_root="$dir_abs/.dad-sandbox"
if [ -z "$sandbox" ]; then
  # The refusal, when even this checkout's shortest slug cannot fit, is
  # sandbox_slug's: it depends on the name, so a short branch still runs from a
  # DIR too deep for a long one. The assignment carries its exit status, so
  # `set -e` stops here on it.
  slug=$(sandbox_slug $((socket_path_max - $(byte_len "$sandbox_root") - 13)))
  sandbox="$sandbox_root/$slug"
fi

# Before per-branch sandboxes every branch shared ROOT itself as its sandbox,
# and a daemon started that way may still be running there with agents under
# it. Nothing here touches it unless SANDBOX names it; say how to reach it.
old_sandbox_note() {
  if [ "$mode" != keep-desktop ] && [ "$sandbox" != "$sandbox_root" ] && [ -S "$sandbox_root/attach.sock" ]; then
    echo "note: $sandbox_root holds a sandbox from before each branch got its own;" >&2
    echo "      its daemon may still be running. \`task run-stop SANDBOX=$sandbox_root\` stops it." >&2
  fi
}

# An explicit SANDBOX is checked before anything creates it, against its path
# as written made absolute; the check after it is resolved below also catches a
# symlink in it that leads somewhere longer.
case $sandbox in
  /*) check_socket_path_fits "$sandbox" ;;
  *) check_socket_path_fits "$PWD/$sandbox" ;;
esac

case $mode in
  stop)
    if [ ! -d "$sandbox" ]; then
      old_sandbox_note
      echo "no sandbox at $sandbox — nothing to stop"
      exit 0
    fi
    ;;
  *) mkdir -p "$sandbox" ;;
esac
# Absolute, because the desktop refuses a relative settings path outright, and
# a relative socket path would resolve against whatever cwd each child has.
sandbox=$(CDPATH='' cd -- "$sandbox" && pwd)
check_socket_path_fits "$sandbox"
old_sandbox_note

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
# Absolute, because DOT_AGENT_DECK_BIN (below) is honoured only as an absolute
# path; a relative value would be ignored and agents would report through the
# installed release.
case $bin in
  /*) ;;
  *) bin="$PWD/$bin" ;;
esac

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

# Every agent's hook command, the OpenCode plugin and the Pi extension honour
# this in place of the binary they name (PRD #1497). Without it a sandbox
# agent's hooks run the absolute installed path the agent's own config holds —
# the release — so status still reaches this sandbox's daemon (the socket above
# is inherited) but anything the release predates, such as the turn's reply for
# voice reading, is dropped. Panes inherit it, nested devbox layers included.
# It takes effect only once the hook entries carry the override — `hooks
# install` from this build writes them (docs/develop/local-run.md).
export DOT_AGENT_DECK_BIN="$bin"

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
#
# The keeper also leads a SESSION of its own, with no controlling terminal, so
# nothing in the desktop's tree can make another process group the foreground
# of the terminal the TUI owns: tcsetpgrp works only on the caller's own
# controlling terminal, and opening /dev/tty there fails. While the desktop
# shared the TUI's session, a process in its tree that took the foreground
# would leave the TUI in a background group, and the TUI's next read of a key
# would stop its whole group — `task` included — with SIGTTIN, which zsh
# reports as `suspended (tty input)`: the report this guards against, from
# macOS. It did not reproduce on Linux, so which process did it there is not
# known; this rules out every one in the desktop's tree
# (docs/develop/local-run.md, "Why the desktop runs in a session of its own").
# The keeper's session id is also its pid, so `desktop_pgid` still names its
# group.
#
# The session is made by perl's POSIX::setsid, because macOS ships perl and
# has no setsid(1), so Linux runs the same path macOS does. perl runs in place
# and then execs the keeper — the backgrounded child that calls setsid is not a
# process-group leader, so setsid needs no fork — which is what makes `$!` the
# keeper's pid. Until perl has called setsid that pid is not a group leader,
# which is why finish_all waits for it to become one.
keeper_session_preflight() {
  command -v perl >/dev/null 2>&1 ||
    die "\`task run-all\` needs perl to start the desktop in a session of its own, and perl is not on PATH"
}

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

# Is $1 a live process that is about to be this sandbox's keeper — perl, or
# whatever runs it, with the keeper's command line in its argv?
desktop_keeper_starting() {
  local pid=$1 args
  case $pid in '' | *[!0-9]*) return 1 ;; esac
  args=$(ps -ww -o args= -p "$pid" 2>/dev/null) || return 1
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
  # DAD_SANDBOX_RUN_VITE_PORT moves only this probe, not vite, and exists for
  # xtask/linkage-check/src/sandbox_run.rs, whose stand-in desktop binds
  # nothing: 1420 is in use whenever a `tauri dev` runs on the machine.
  local port=${DAD_SANDBOX_RUN_VITE_PORT:-1420}
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null || (exec 3<>"/dev/tcp/::1/$port") 2>/dev/null; then
    die "port $port is in use, probably by another \`pnpm dev\` or \`tauri dev\` — the desktop's vite server needs it"
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
  keeper_session_preflight
  echo "$bin" >"$binary_record"
  rm -f "$desktop_exit_record"
  : >"$desktop_log"

  # The keeper in a session, and so a process group, of its own (see "the
  # background desktop's process group" above). This shell never turns job
  # control on, so the TUI below stays in this shell's process group — the one
  # the terminal's shell made its foreground — as it does under `task run`.
  perl -MPOSIX -e '
    POSIX::setsid() > 0 or die "setsid: $!\n";
    exec { $ARGV[0] } @ARGV or die "exec $ARGV[0]: $!\n";
  ' bash "$checkout/scripts/sandbox-run.sh" keep-desktop "$checkout" "$dir" "$sandbox" "$profile" "$$" \
    </dev/null >>"$desktop_log" 2>&1 &
  local desktop_pgid=$!
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
  # The TUI can exit before perl has made the keeper's session, while the pid
  # still sits in THIS shell's process group, so it is not yet the leader
  # desktop_group_is_ours looks for. Give it two seconds to become one rather
  # than leave it to start the desktop after we are gone. Until then it is
  # signalled only by its own pid: its group is still ours and the TUI's.
  local i=0
  while [ "$i" -lt 20 ] && ! desktop_group_is_ours "$desktop_pgid" &&
    desktop_keeper_starting "$desktop_pgid"; do
    sleep 0.1
    i=$((i + 1))
  done
  if desktop_group_is_ours "$desktop_pgid"; then
    stop_desktop_group "$desktop_pgid"
  elif desktop_keeper_starting "$desktop_pgid"; then
    kill -TERM "$desktop_pgid" 2>/dev/null
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

# Has this keeper lost the shell that started it ($1)?
parent_is_gone() {
  [ "$(ps -o ppid= -p "$$" 2>/dev/null | tr -d ' ')" != "$1" ]
}

keep_desktop() {
  local parent=$1 child status

  # The TUI lazy-spawns the sandbox daemon, and the desktop makes ONE
  # connect-only probe at launch. So wait until the daemon answers before
  # starting it, or a warm desktop can beat the daemon up and open on an error
  # that only a manual Reconnect clears. Bounded: after 30s it starts anyway.
  # And never start it once the shell that started us is gone: its teardown
  # may already have run and found nothing to stop.
  local i=0
  until "$bin" daemon status >/dev/null 2>&1; do
    parent_is_gone "$parent" && exit 0
    i=$((i + 1))
    [ "$i" -ge 150 ] && break
    sleep 0.2
  done
  parent_is_gone "$parent" && exit 0

  # Through perl, which puts SIGINT and SIGQUIT back to their defaults: this
  # keeper is a shell without job control, which starts a `&` command with both
  # ignored, an ignored signal stays ignored across exec, and the desktop's tree
  # had them at their defaults when `set -m` started it.
  (cd "$desktop_dir" && exec perl -e '
    $SIG{INT} = $SIG{QUIT} = "DEFAULT";
    exec { $ARGV[0] } @ARGV or die "exec $ARGV[0]: $!\n";
  ' pnpm tauri dev) &
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
  #   hangup does do is detach that shell's session from the tty, so we watch
  #   the shell's terminal — this keeper's own session never has one.
  local started_on_tty
  started_on_tty=$(controlling_tty "$parent")
  local reason=""
  while [ -z "$reason" ]; do
    if parent_is_gone "$parent"; then
      reason="the shell that started this desktop is gone"
    elif [ -n "$started_on_tty" ] && [ -z "$(controlling_tty "$parent")" ]; then
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
