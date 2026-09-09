#!/usr/bin/env bash
#
# Issue #648 — the Rust toolchain and cargo-nextest versions are pinned TWICE,
# once in `devbox.json` (what a `devbox shell` gets) and once in
# `.github/workflows/` (what CI gets). `devbox.json`'s `packages` has two
# spellings and this reads both — see `devbox_pins` below, and issue #791 for
# the day devbox rewrote the file from one to the other. Nothing checked that the two agree, and
# on 2026-08-11 they stopped agreeing: `devbox.json` moved to cargo-nextest
# 0.9.143 via an automerged Renovate PR (#510) while `ci.yml` sat on 0.9.140
# for eleven days. `renovate.json`'s own rule had said the quiet part out loud —
# the lockstep "is the whole point of pinning them, and nothing enforces it
# automatically".
#
# This is that enforcement. It compares the two sides and exits non-zero on any
# disagreement, so `cargo test-fast` locally and CI both say so instead of
# nobody saying anything.
#
# WHERE IT RUNS
#
#   * `cargo test-fast` — via `xtask/linkage-check/src/pin_lockstep.rs`, which
#     drives this script against the real repository and against synthetic
#     drifted fixtures (so the guard itself is tested rather than assumed).
#     That puts it in the required `build` / `build-macos` / `build-windows`
#     jobs too.
#   * the `devbox` job in `ci.yml`, directly. That job is the ONLY one with no
#     `changes` gate, and a devbox-only Renovate PR skips all four required
#     jobs — GitHub reports a skipped required check as passing — so without
#     this step a drift arriving on the devbox side alone would meet no check
#     at all on the PR that introduced it.
#
# WHAT IT CHECKS, per pin class:
#
#   1. Every workflow site carries a PARSEABLE pin. An unpinned or reformatted
#      site (`toolchain: "1.98.0"`, `tool: cargo-nextest`) is an error, not a
#      silent non-match. That matters because Renovate finds these pins with
#      the same shape of regex: a site this script cannot read is a site
#      Renovate cannot bump, which is the "silent rot" failure mode PR #641
#      named, and it must be loud rather than absent. Symmetrically, a site
#      Renovate CAN read must be read here too, in either YAML spelling: block
#      (`toolchain: 1.98.0`) and flow (`with: { toolchain: 1.98.0 }`) are one
#      mapping, and reading only the first is how issue #710 let a tracked pin
#      drift under a check reporting `ok`.
#   2. At least one site exists on each side, so a rename cannot make the whole
#      check pass vacuously.
#   3. All sites within a side agree with each other (every `toolchain:` line
#      across the workflows; devbox's rustc/cargo/clippy/rustfmt, which are one
#      toolchain). The site count is derived, never written down here — it was
#      "seven" in this comment while the files carried nine.
#   4. The two sides agree with each other. This is the lockstep itself.
#
# It deliberately hardcodes NO version. Every value is read off the files, so
# this script never needs touching when a pin moves — only when a new pin class
# starts being duplicated across the two.
#
# Usage: scripts/check-pin-lockstep.sh [REPO_ROOT]
# Exit:  0 = the pins agree, 1 = they do not (details on stderr).

set -euo pipefail

case "${1:-}" in
  -h | --help)
    sed -n '2,/^# Exit:/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
esac

root="${1:-}"
if [ -z "$root" ]; then
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi

if [ ! -f "$root/devbox.json" ]; then
  printf 'check-pin-lockstep: no devbox.json under %s\n' "$root" >&2
  exit 1
fi

fail=0
err() {
  printf 'DRIFT: %s\n' "$*" >&2
  fail=1
}

# The scanners below run inside `$(...)`, i.e. in SUBSHELLS, so a bare `err`
# call from one of them would set `fail=1` in a process that is about to exit
# and the script would print DRIFT and then `exit 0`. They therefore emit their
# findings as `!ERR <message>` lines on stdout, and `compare` — which runs in
# the main shell — turns those into real `err` calls. `note_scan_errors` and
# `sites_only` are the two halves of that split.
SCAN_ERR='!ERR '

note_scan_errors() {
  local line
  while IFS= read -r line; do
    case "$line" in
      "$SCAN_ERR"*) err "${line#"$SCAN_ERR"}" ;;
    esac
  done < <(printf '%s\n' "$1")
}

sites_only() {
  printf '%s\n' "$1" | grep -v "^$SCAN_ERR" | sed '/^[[:space:]]*$/d' || true
}

SEMVER='^[0-9]+\.[0-9]+\.[0-9]+$'

# The workflow file set is deliberately the same one renovate.json's
# customManagers match (`/^\.github/workflows/[^/]+\.ya?ml$/`): top level only,
# `.yml` or `.yaml`. A pin this script reads but Renovate does not — or the
# reverse — would be a lockstep between the wrong two things.
workflow_files() {
  local f
  for f in "$root"/.github/workflows/*.yml "$root"/.github/workflows/*.yaml; do
    [ -e "$f" ] && printf '%s\n' "$f"
  done
  return 0
}

# The value a pin key carries, given everything on the line AFTER the key —
# e.g. `" 1.97.1 # note"` from block style, or `" 1.98.0 }"` from flow style.
#
# It takes the first TOKEN rather than the rest of the line: leading whitespace
# is dropped and the value ends at the next whitespace, `,` or `}`. That is what
# makes flow style readable, and reading it is the point. YAML spells one
# mapping two ways, and renovate.json's `toolchain:\s*(\d+\.\d+\.\d+)` reads
# both, so `with: { toolchain: 1.98.0 }` is a pin Renovate tracks and bumps.
# Rest-of-line trimming yields `1.98.0 }`, which fails the SEMVER test below —
# so the guard would have called an actively-tracked pin unreadable. A
# flow-style pin is therefore ACCEPTED and compared, not reported: it is
# tracked, so it can drift, and comparing pins that can drift is the whole job
# (issue #710).
#
# Ending at whitespace also subsumes the trailing-comment strip this function
# used to do, because YAML requires whitespace before a `#` comment; `1.97.1#x`
# is one scalar to a YAML parser and is not a version, so failing it is right.
#
# Quotes are deliberately NOT stripped: renovate.json matches a BARE X.Y.Z, so
# `toolchain: "1.97.1"` is a pin Renovate cannot read even though YAML gives it
# the same value. Normalising the quotes away here would hand back a
# valid-looking version and let that site pass — which is the exact silent-rot
# case check 1 above exists to catch, so the quotes have to survive into the
# SEMVER test and fail it. Flow style does not weaken that: `{ toolchain:
# "1.97.1" }` ends at the `}` with its quotes intact and fails just the same.
pin_value() {
  local s="$1"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%%[[:space:]]*}"
  s="${s%%,*}"
  s="${s%%\}*}"
  printf '%s' "$s"
}

# The candidate lines for one pin token: `<lineno>:<text>` for every NON-COMMENT
# line of `$2` that contains `$1` anywhere.
#
# Anywhere is the load-bearing word, and it is not a stylistic choice — it is
# what renovate.json does. Its customManagers are unanchored, so they match
# their token wherever it appears on a line. `scan_workflow_toolchain` used to
# anchor at `^[[:space:]]*toolchain:`, i.e. block style only, so a flow-style
# site (`with: { toolchain: 1.98.0 }`) was a real, Renovate-bumped pin that this
# guard could not see: it would drift while the check reported `ok` (issue
# #710). Both scanners share this helper so the two cannot diverge again.
#
# The FULL-LINE COMMENT exclusion is the one deliberate divergence from
# Renovate, and it is why this was not a one-line regex swap. ci.yml's own
# header reads "match on `toolchain:` and `tool: cargo-nextest@` anywhere under
# .github/workflows"; un-anchoring without it makes that line a site, `pin_value`
# yields a backtick, and the guard reports the repository's documentation OF
# THIS CHECK as an unreadable pin — a false positive on the very file the check
# exists for. Renovate ignores that line too, for its own reason (no bare X.Y.Z
# follows the token there), so the exclusion costs nothing as the files stand.
# What it would cost, if someone wrote a literal version into a full-line
# comment, is a pin Renovate bumps and this guard never compares — the harmless
# direction (an upgrade PR nobody asked for, not a drift nobody sees), and the
# same ci.yml header already says in prose not to write versions there.
pin_lines() {
  grep -nE "^[[:space:]]*([^#[:space:]].*)?$1" "$2" || true
}

# Emits "<file>:<line> <version>" per site. `dtolnay/rust-toolchain`'s input.
scan_workflow_toolchain() {
  local f hit lineno value
  while IFS= read -r f; do
    while IFS= read -r hit; do
      lineno="${hit%%:*}"
      value="$(pin_value "${hit#*toolchain:}")"
      # A shell expansion is not a pin. `windows-cross-check` echoes a resolved
      # rustup directory as `toolchain: $WINDOWS_CROSS_CHECK_TOOLCHAIN`, which
      # is diagnostics, not a version — and Renovate's regex skips it for the
      # same reason.
      case "$value" in
        *'$'*) continue ;;
      esac
      if ! printf '%s' "$value" | grep -qE "$SEMVER"; then
        printf '%s%s:%s has an unreadable Rust toolchain pin: %s. The customManager in renovate.json matches a bare X.Y.Z here; anything else silently stops being tracked.\n' \
          "$SCAN_ERR" "${f#"$root"/}" "$lineno" "'$value'"
        continue
      fi
      printf '%s:%s %s\n' "${f#"$root"/}" "$lineno" "$value"
    done < <(pin_lines 'toolchain:' "$f")
  done < <(workflow_files)
}

# Emits "<file>:<line> <version>" per site. `taiki-e/install-action`'s input.
scan_workflow_nextest() {
  local f hit lineno rest token value
  while IFS= read -r f; do
    while IFS= read -r hit; do
      lineno="${hit%%:*}"
      rest="${hit#*:}"
      while IFS= read -r token; do
        value="${token#cargo-nextest}"
        value="${value#@}"
        if [ -z "$value" ]; then
          printf '%s%s:%s names cargo-nextest with no version. Every workflow pin is explicit — never @latest / @nextest.\n' \
            "$SCAN_ERR" "${f#"$root"/}" "$lineno"
          continue
        fi
        if ! printf '%s' "$value" | grep -qE "$SEMVER"; then
          printf '%s%s:%s has an unreadable cargo-nextest pin: %s.\n' \
            "$SCAN_ERR" "${f#"$root"/}" "$lineno" "'$value'"
          continue
        fi
        # The value is a good version, but Renovate only finds it where the
        # tool name follows `tool:` DIRECTLY: its regex is
        # `tool:\s*cargo-nextest@(\d+\.\d+\.\d+)`. `tool: "cargo-nextest@X.Y.Z"`
        # means the same thing to YAML and matches nothing, so the pin stops
        # being tracked with nothing going red — the same silent-rot class as a
        # quoted `toolchain:`, and reported for the same reason.
        if ! printf '%s' "$rest" | grep -qE "tool:[[:space:]]*cargo-nextest@$value"; then
          printf '%s%s:%s has a cargo-nextest pin renovate.json cannot read. Its regex wants `tool:` followed directly by a bare cargo-nextest@X.Y.Z; quoting it silently stops the pin being tracked.\n' \
            "$SCAN_ERR" "${f#"$root"/}" "$lineno"
          continue
        fi
        printf '%s:%s %s\n' "${f#"$root"/}" "$lineno" "$value"
      # `}` is in the excluded set for the same reason `pin_value` stops there:
      # a flow-style `with: {tool: cargo-nextest@0.9.143}` is a pin Renovate
      # reads, and without it the token would come back as `0.9.143}` and be
      # reported unreadable (issue #710). Comment lines are excluded by
      # `pin_lines`, so prose that mentions `tool: cargo-nextest@` — ci.yml's
      # own header does — is not a site.
      done < <(printf '%s' "$rest" | grep -oE "cargo-nextest(@[^[:space:],}\"']*)?" || true)
    done < <(pin_lines 'cargo-nextest' "$f")
  done < <(workflow_files)
}

# Every `<name><TAB><version>` pair in devbox.json's `packages`, in file order.
#
# `packages` HAS TWO SPELLINGS and devbox writes both, which is why this is a
# parser rather than the one-line `grep -oE "\"$name@[^\"]*\""` it used to be.
# The array form (`"rustc@1.97.1"`) is what a file only ever touched by plain
# `devbox add <pkg>` keeps. The object form (`"rustc": "1.97.1"`, or
# `"rustc": {"version": "1.97.1", …}` once a package carries a per-package
# option) is what devbox REWRITES THE WHOLE FILE to the moment any one package
# takes an option — `--disable-plugin` on nodejs (issue #791) converted all 22
# entries in one go. Reading only the array form therefore did not fail on the
# one changed entry; it lost every Rust pin at once and said "devbox.json pins
# no rustc at all" four times over. That is the loud direction, and the "or its
# spelling changed (fix the script)" clause in that message is exactly this fix.
#
# Reading both is also what keeps the lockstep between the right two things.
# Renovate's devbox manager parses `packages` as a union of the array form and a
# record whose values are either a version string or an object carrying one
# (`lib/modules/manager/devbox/schema.ts`), so BOTH spellings are pins Renovate
# tracks and bumps — and a guard that reads only one of them is blind to drift
# in the other, the same shape of hole as reading only block-style YAML (#710).
#
# Scoped to the `packages` block by a brace/bracket walk rather than grepped
# over the whole file, so a `shell.scripts` entry that happens to be named after
# a package (`"cargo": "cargo build"`) cannot be read as that package's pin. The
# walk tracks string state, so a brace inside a value is not a nesting change.
devbox_pins() {
  awk '
    { all = all $0 "\n" }

    # Index of the character one past the value that starts at `start` (the
    # opening `{` or `[`), honouring nesting and quoted strings.
    function close_at(s, start,   i, c, depth, instr) {
      depth = 0
      instr = 0
      for (i = start; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (instr) {
          if (c == "\\") i++
          else if (c == "\"") instr = 0
          continue
        }
        if (c == "\"") { instr = 1; continue }
        if (c == "{" || c == "[") depth++
        else if (c == "}" || c == "]") {
          depth--
          if (depth == 0) return i + 1
        }
      }
      return length(s) + 1
    }

    # The contents of the first quoted token in `s` — the key of an object
    # entry, or the whole `name@version` of an array entry.
    function unquote(s) {
      if (!match(s, /"[^"]*"/)) return ""
      return substr(s, RSTART + 1, RLENGTH - 2)
    }

    # The contents of the LAST quoted token in a matched `… "key": "value"`
    # head. Written as "drop everything through the opening quote of the value"
    # rather than "take everything after the first colon", because a devbox
    # package name may itself contain one — `path:gcloud#google-cloud-sdk` is
    # two of the entries in this repository.
    function tailvalue(s,   t) {
      t = s
      sub(/^[^"]*"[^"]*"[[:space:]]*:[[:space:]]*"/, "", t)
      sub(/"$/, "", t)
      return t
    }

    # The value of a `"version": "X.Y.Z"` key anywhere in `s`, or "".
    function versionof(s,   t) {
      if (!match(s, /"version"[[:space:]]*:[[:space:]]*"[^"]*"/)) return ""
      t = substr(s, RSTART, RLENGTH)
      sub(/^"version"[[:space:]]*:[[:space:]]*"/, "", t)
      sub(/"$/, "", t)
      return t
    }

    function emit_array_entry(tok,   at) {
      at = index(tok, "@")
      # A version-less entry (a `path:` flake ref) is emitted with an EMPTY
      # version rather than dropped: `scan_devbox` only looks up the names it
      # was asked for, and for one of those an empty value has to reach the
      # "not an exact X.Y.Z version" complaint instead of reading as absent.
      if (at > 0) print substr(tok, 1, at - 1) "\t" substr(tok, at + 1)
      else print tok "\t"
    }

    END {
      p = index(all, "\"packages\"")
      if (p == 0) exit
      s = substr(all, p)
      if (!match(s, /^"packages"[[:space:]]*:[[:space:]]*[\[{]/)) exit
      # The match ends ON the opening bracket, so RLENGTH indexes it.
      open_at = RLENGTH
      body = substr(s, open_at, close_at(s, open_at) - open_at)

      # Every branch below is anchored at `^`, so RSTART is 1 and RLENGTH alone
      # says how far to advance. Both are SAVED before anything else runs:
      # `match` is global state in awk, and calling `unquote` between reading
      # RLENGTH and using it silently reinterprets the rest of the file.
      while (length(body) > 0) {
        # `"name": { … }` — a package carrying per-package options.
        if (match(body, /^[][{}[:space:],]*"[^"]*"[[:space:]]*:[[:space:]]*\{/)) {
          head = substr(body, 1, RLENGTH)
          obj_at = RLENGTH
          obj_end = close_at(body, obj_at)
          v = versionof(substr(body, obj_at, obj_end - obj_at))
          # An object with no `version` key pins nothing, and is reported as
          # absent by `scan_devbox` — loud, and the only honest answer.
          if (v != "") print unquote(head) "\t" v
          body = substr(body, obj_end)
          continue
        }
        # `"name": "version"` — the ordinary object entry.
        if (match(body, /^[][{}[:space:],]*"[^"]*"[[:space:]]*:[[:space:]]*"[^"]*"/)) {
          head = substr(body, 1, RLENGTH)
          body = substr(body, RLENGTH + 1)
          print unquote(head) "\t" tailvalue(head)
          continue
        }
        # `"name@version"` — the array entry.
        if (match(body, /^[][{}[:space:],]*"[^"]*"/)) {
          head = substr(body, 1, RLENGTH)
          body = substr(body, RLENGTH + 1)
          emit_array_entry(unquote(head))
          continue
        }
        # Anything else (a bare `true`, a number, an unrecognised shape): step
        # over one character and keep going. Nothing is invented from it, so an
        # unreadable spelling loses its pins loudly rather than inventing one.
        body = substr(body, 2)
      }
    }
  ' "$root/devbox.json"
}

# Emits "devbox.json(<name>) <version>" per named package in devbox.json.
scan_devbox() {
  local name value found pins
  pins="$(devbox_pins)"
  for name in "$@"; do
    found=0
    while IFS= read -r value; do
      found=1
      if ! printf '%s' "$value" | grep -qE "$SEMVER"; then
        printf '%s%s\n' "$SCAN_ERR" "devbox.json pins $name at '$value', which is not an exact X.Y.Z version."
        continue
      fi
      printf 'devbox.json(%s) %s\n' "$name" "$value"
    done < <(printf '%s\n' "$pins" | awk -F'\t' -v n="$name" '$1 == n { print $2 }')
    # A component that VANISHED is not a component that agrees. `compare` only
    # ever sees the versions that were found, so without this a devbox.json
    # which dropped or renamed `clippy` would leave the other three agreeing
    # with the workflows and the whole class passing. Check 2 catches that only
    # when EVERY name in the class is gone; this is its per-name half, and the
    # four Rust components are exactly where it matters, since they are one
    # toolchain spelled four ways.
    if [ "$found" -eq 0 ]; then
      printf '%s%s\n' "$SCAN_ERR" "devbox.json pins no $name at all. Each package named here is half of a version duplicated into .github/workflows/, so an absent one cannot be in lockstep with anything. If it was removed or renamed deliberately, update this script's caller to match."
    fi
  done
}

# Both sides of one pin class. `$1` is the human name, `$2`/`$3` the collected
# "<site> <version>" lines.
compare() {
  local class="$1" devbox_raw="$2" workflow_raw="$3"
  local devbox_sites workflow_sites devbox_versions workflow_versions

  note_scan_errors "$devbox_raw"
  note_scan_errors "$workflow_raw"
  devbox_sites="$(sites_only "$devbox_raw")"
  workflow_sites="$(sites_only "$workflow_raw")"

  if [ -z "$devbox_sites" ]; then
    err "$class: devbox.json pins nothing for this class. Either the package was removed (drop it from this script) or its spelling changed (fix the script) — passing vacuously is not an option."
    return
  fi
  if [ -z "$workflow_sites" ]; then
    err "$class: no workflow pins this class. .github/workflows/ must pin the same version devbox.json does; an absent pin means CI floats."
    return
  fi

  devbox_versions="$(printf '%s\n' "$devbox_sites" | awk '{print $2}' | sort -u)"
  workflow_versions="$(printf '%s\n' "$workflow_sites" | awk '{print $2}' | sort -u)"

  if [ "$(printf '%s\n' "$devbox_versions" | wc -l)" -ne 1 ]; then
    err "$class: devbox.json is internally inconsistent ($(printf '%s' "$devbox_versions" | tr '\n' ' ')):"
    printf '%s\n' "$devbox_sites" | sed 's/^/         /' >&2
    return
  fi
  if [ "$(printf '%s\n' "$workflow_versions" | wc -l)" -ne 1 ]; then
    err "$class: .github/workflows/ is internally inconsistent ($(printf '%s' "$workflow_versions" | tr '\n' ' ')):"
    printf '%s\n' "$workflow_sites" | sed 's/^/         /' >&2
    return
  fi

  if [ "$devbox_versions" != "$workflow_versions" ]; then
    err "$class: devbox.json pins $devbox_versions, .github/workflows/ pins $workflow_versions. The local gate and CI would run different builds."
    printf '%s\n%s\n' "$devbox_sites" "$workflow_sites" | sed 's/^/         /' >&2
    return
  fi

  printf 'ok: %s pinned at %s on both sides (%s workflow site(s))\n' \
    "$class" "$devbox_versions" "$(printf '%s\n' "$workflow_sites" | wc -l | tr -d ' ')"
}

# `rustc`/`cargo`/`clippy`/`rustfmt` are four devbox packages carrying ONE
# toolchain version, and the workflows express the same thing as a single
# `toolchain:` input, so they are one class with four devbox sites.
compare "Rust toolchain" \
  "$(scan_devbox rustc cargo clippy rustfmt)" \
  "$(scan_workflow_toolchain)"

compare "cargo-nextest" \
  "$(scan_devbox cargo-nextest)" \
  "$(scan_workflow_nextest)"

if [ "$fail" -ne 0 ]; then
  cat >&2 <<'EOF'

The pins in devbox.json and .github/workflows/ must match exactly: `cargo
test-fast` / `cargo test-e2e` in a devbox shell and `cargo nextest run` in CI
are supposed to be the same claim, and they are not while these disagree.

Both sides move together in ONE pull request. renovate.json holds the
toolchain-class devbox packages for a human precisely so the two halves can be
landed as one change; see the "Devbox toolchain packages" rule there.
EOF
  exit 1
fi

exit 0
