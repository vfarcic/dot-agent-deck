#!/usr/bin/env bash
set -euo pipefail

# Detect what a working session leaves behind on this box, so the tag-release
# skill (or anyone asking to "clean up the box") can show it and prune what the
# user confirms:
#
# - worktrees and local/remote branches whose work has already been merged;
# - other registered worktrees at a detached HEAD (`/verify-pr`'s `-pr-<n>-base`);
# - tool caches beside the main checkout (`<repo>-land`, `<repo>-xver-*`);
# - stray sibling directories (`<repo>-*`, `dad-*`) that are neither a
#   registered worktree nor a known tool cache;
# - processes owned by this user whose cwd was deleted or lies inside one of the
#   directories above.
#
# This script is detection-only: it never removes a worktree, deletes a branch or
# directory, or signals a process. It does run `git fetch --prune` to refresh
# remote-tracking refs, which only updates local bookkeeping and never modifies
# the remote. The e2e harness's temp roots are not scanned here: the skill runs
# `cargo xtask clean-e2e-tmp`, which already vets those by owning PID and age.

# --- Determine the default branch ---
default_branch="main"
if ref=$(git symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null); then
  default_branch="${ref#refs/remotes/origin/}"
fi

# --- Refresh remote-tracking refs (drops refs for branches deleted upstream) ---
git fetch --prune --quiet origin 2>/dev/null || true

current_branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
current_worktree=$(git rev-parse --show-toplevel 2>/dev/null || echo "")

# --- Gather PR state ---
#
# Merge detection is per-COMMIT, not per-branch-name. A name alone proves
# nothing: Renovate reuses one branch name across many PRs, so a merged
# `renovate/foo` PR leaves that name looking "merged" long after the branch has
# been recreated at a new, unmerged tip for the next open PR. Judging by name
# there offers a live PR's branch up for deletion.
#
# Held as newline-delimited strings rather than `declare -A` associative arrays,
# which are bash 4. macOS ships /bin/bash 3.2.57, where `declare -A` does not go
# quietly unsupported: the assignment degrades to an ordinary INDEXED one, the
# subscript is evaluated arithmetically, and `set -u` kills the script on that
# line with a message naming a variable that appears nowhere in it. The same
# defect was fixed in `scripts/assemble-changelog.sh`, `verify-pr/scan.sh` and
# `demo-reel-adapter/build.sh` (issues #521, #582); this file was the one
# instance knowingly left behind, because it was a `dot-ai-` synced mirror and a
# fix would have been reverted by the next sync. Issue #1089 forked it, so the
# reason expired and the fix lands here.
#
# A newline is the one delimiter that is safe: git refuses a ref name containing
# one, so no branch name can forge a record boundary.
merged_pairs=$'\n'   # lines of "<branch name> <head sha>" for merged PRs
open_names=$'\n'     # lines of "<branch name>" with an open PR, never candidates

if command -v gh >/dev/null 2>&1; then
  # Head SHAs of merged same-repo PRs. Covers squash & rebase merges, where the
  # branch's commits never land verbatim on the default branch and so are
  # invisible to an ancestry test. Cross-repo (fork) PRs are excluded: their
  # head names describe branches in the fork, not ours, so a merged fork PR for
  # `fix/thing` says nothing about our own `fix/thing`.
  while IFS=$'\t' read -r name sha; do
    if [ -z "$name" ] || [ -z "$sha" ]; then continue; fi
    merged_pairs="${merged_pairs}${name} ${sha}"$'\n'
  done < <(gh pr list --state merged --limit 200 --json headRefName,headRefOid,isCrossRepository \
             --jq '.[] | select(.isCrossRepository | not) | [.headRefName, .headRefOid] | @tsv' 2>/dev/null || true)

  # Any open PR protects its head branch. Deleting the branch closes the PR, so
  # this is the guard that matters most. Fork PRs are deliberately INCLUDED
  # here: matching a fork's head name against ours can only over-protect (we
  # keep a branch we might have pruned), which is the safe direction to err.
  # The limit is higher than the merged query's on purpose: truncating merged
  # PRs just leaves a branch unoffered, but truncating OPEN ones would offer a
  # live PR's branch for deletion.
  while IFS= read -r b; do
    if [ -z "$b" ]; then continue; fi
    open_names="${open_names}${b}"$'\n'
  done < <(gh pr list --state open --limit 1000 --json headRefName --jq '.[].headRefName' 2>/dev/null || true)
fi

# is_merged <branch-name> <ref-to-its-tip>
# Resolves the ref's OWN tip, so a local branch and its same-named remote are
# judged independently -- `origin/foo` may carry unmerged commits that local
# `foo` does not. On success the vetted tip is left in `vetted_tip`, which the
# caller records beside the branch so the delete step can confirm the branch has
# not moved since (see the skill's cleanup step).
vetted_tip=""
is_merged() {
  local name="$1" ref="$2" tip mname msha
  vetted_tip=""
  case "$open_names" in
    *$'\n'"$name"$'\n'*) return 1 ;;
  esac
  tip=$(git rev-parse --verify --quiet "$ref") || return 1
  if [ -z "$tip" ]; then return 1; fi
  # Real merge or fast-forward: the tip is already reachable from the default.
  if git merge-base --is-ancestor "$tip" "refs/remotes/origin/${default_branch}" 2>/dev/null; then
    vetted_tip="$tip"
    return 0
  fi
  # Squash/rebase merge: the tip must still be exactly what the merged PR
  # carried. A recreated or advanced branch has moved on and is not merged.
  while IFS=' ' read -r mname msha; do
    if [ "$mname" = "$name" ] && [ "$msha" = "$tip" ]; then
      vetted_tip="$tip"
      return 0
    fi
  done <<< "$merged_pairs"
  return 1
}


# --- Processes owned by this user, and their working directories ---
#
# Read from `/proc/<pid>/cwd`, which exists on Linux only. Where it cannot be
# read the script says `PROC_CHECK=unavailable` and labels every directory
# `processes: could not check`, rather than printing `none` for a check that
# never ran. `CLEANUP_PROC_ROOT` points the scan elsewhere; the tests use it to
# take that degraded path on a Linux host.
proc_root="${CLEANUP_PROC_ROOT:-/proc}"
proc_check="unavailable"
procs=$'\n'   # lines of "<pid><TAB><cwd>", cwd as the kernel reports it
if readlink "${proc_root}/$$/cwd" >/dev/null 2>&1; then
  proc_check="ok"
  for p in "${proc_root}"/[0-9]*; do
    # `-O`: owned by our effective uid. Another user's cwd is unreadable anyway,
    # and none of their processes is ours to offer.
    [ -O "$p" ] || continue
    pid="${p##*/}"
    [ "$pid" = "$$" ] && continue
    cwd=$(readlink "${p}/cwd" 2>/dev/null) || continue
    procs="${procs}${pid}"$'\t'"${cwd}"$'\n'
  done
fi

# proc_cmd <pid> — the command line, space-joined and cut short for display.
proc_cmd() {
  tr '\0' ' ' 2>/dev/null < "${proc_root}/$1/cmdline" | cut -c1-120 || true
}

# is_daemon <pid> — a `dot-agent-deck … daemon …` process. Never offered for a
# kill, whatever its cwd: stopping a daemon stops every agent it manages
# (CLAUDE.md rule 12's teardown step and rule 15). Matching any argv element
# rather than argv[0] alone also catches a daemon started through an
# interpreter or wrapper.
is_daemon() {
  local a saw=false
  while IFS= read -r -d '' a; do
    if $saw && [ "$a" = "daemon" ]; then return 0; fi
    case "${a##*/}" in dot-agent-deck*) saw=true ;; esac
  done 2>/dev/null < "${proc_root}/$1/cmdline" || true
  return 1
}

# holders <dir> — pids whose cwd is <dir> or below it, one per line. A cwd the
# kernel marks ` (deleted)` is not inside any directory that still exists.
holders() {
  local d="$1" pid cwd
  while IFS=$'\t' read -r pid cwd; do
    [ -z "$pid" ] && continue
    case "$cwd" in *" (deleted)") continue ;; esac
    case "$cwd" in "$d"|"$d"/*) echo "$pid" ;; esac
  done <<< "$procs"
}

# --- Directory facts: size, newest modification, git state ---

# `du` exits non-zero when one file below is unreadable but still prints the
# total, so its status is ignored and only an empty answer reads as unknown.
dir_size() {
  local s
  s=$(du -sh "$1" 2>/dev/null | cut -f1 || true)
  echo "${s:-?}"
}

# The newest modification anywhere below <dir>, which says far more about
# whether something still uses it than the directory's own mtime does. GNU
# `find -printf` where it exists; otherwise the directory's own mtime, labelled.
dir_modified() {
  local t
  t=$(find "$1" -xdev -printf '%TY-%Tm-%Td %TH:%TM\n' 2>/dev/null | LC_ALL=C sort | tail -1 || true)
  if [ -n "$t" ]; then echo "$t"; return; fi
  t=$(date -r "$1" '+%Y-%m-%d %H:%M' 2>/dev/null || echo "?")
  echo "${t} (directory mtime)"
}

# git in another checkout, with the ambient location variables cleared so a
# GIT_DIR inherited from a hook or `rebase --exec` cannot redirect it.
dgit() {
  local d="$1"; shift
  (
    unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
          GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE
    git -C "$d" "$@"
  )
}

# unpushed <dir> <sha> — empty when <sha> is on a remote or is a merged PR's
# head; otherwise a label that NAMES the commits removal would lose. Asked of
# this repository first (a worktree or a clone of it usually shares the object),
# then of <dir>'s own repository. When neither can answer, it says so instead of
# guessing in either direction.
unpushed() {
  local d="$1" sha="$2" n list where
  if git cat-file -e "${sha}^{commit}" 2>/dev/null; then
    [ -n "$(git for-each-ref --count=1 --contains "$sha" refs/remotes 2>/dev/null)" ] && return 0
    case "$merged_pairs" in *" ${sha}"$'\n'*) return 0 ;; esac
    n=$(git rev-list --count "$sha" --not --remotes 2>/dev/null || echo "?")
    list=$(git log --format='%h %s' -n 3 "$sha" --not --remotes 2>/dev/null | paste -s -d ';' - || true)
    where="this repository"
  elif [ -n "$(dgit "$d" for-each-ref --count=1 refs/remotes 2>/dev/null)" ]; then
    n=$(dgit "$d" rev-list --count "$sha" --not --remotes 2>/dev/null || echo "?")
    [ "$n" = "0" ] && return 0
    list=$(dgit "$d" log --format='%h %s' -n 3 "$sha" --not --remotes 2>/dev/null | paste -s -d ';' - || true)
    where="its own repository"
  else
    echo "COULD NOT VERIFY ${sha:0:12}: unknown to this repository and no remote-tracking refs in the directory"
    return 0
  fi
  echo "UNPUSHED ${n} commit(s) per ${where}: ${list}"
}

# git_label <dir> — what removing <dir> would lose from git's point of view.
# A linked worktree's branches live in the shared repository and survive the
# directory, so only a detached HEAD is checked there; a standalone clone keeps
# its branches inside the directory, so every one of them is checked.
git_label() {
  local d="$1" head br tips="" t u out="" dirty=""
  if [ ! -e "$d/.git" ]; then echo "git: not a checkout"; return; fi
  if ! head=$(dgit "$d" rev-parse --verify --quiet HEAD 2>/dev/null); then
    echo "git: no commits"; return
  fi
  if [ -n "$(dgit "$d" status --porcelain 2>/dev/null | head -1)" ]; then
    dirty="; UNCOMMITTED CHANGES"
  fi
  if [ -f "$d/.git" ]; then
    if br=$(dgit "$d" symbolic-ref --quiet --short HEAD 2>/dev/null); then
      echo "git: on branch ${br}, which outlives the directory${dirty}"; return
    fi
    tips="$head"
  else
    tips="$head"$'\n'"$(dgit "$d" for-each-ref --format='%(objectname)' refs/heads 2>/dev/null || true)"
  fi
  while IFS= read -r t; do
    [ -z "$t" ] && continue
    u=$(unpushed "$d" "$t")
    if [ -n "$u" ]; then
      case "$out" in *"$u"*) ;; *) out="${out:+${out}; }${u}" ;; esac
    fi
  done <<< "$(printf '%s\n' "$tips" | sort -u)"
  echo "git: ${out:-every commit is on a remote or in a merged PR}${dirty}"
}

# --- Candidate directories, each vetted for live processes ---
#
# A directory that a live process has its cwd inside is NEVER offered. It goes
# to HELD_DIRS naming the holders, and its non-daemon holders go to PROCESSES.
# Killing a holder does not make the directory removable on this run's word:
# re-run the script, and remove only what a fresh run offers.
held_out=()
proc_pids=$'\n'   # pids already queued for PROCESSES, to print each once

queue_proc() {
  case "$proc_pids" in *$'\n'"$1"$'\n'*) return ;; esac
  proc_pids="${proc_pids}$1"$'\n'
}

# vet <dir> <kind> — 0 when <dir> may be offered; otherwise records it as held.
# Sets `proc_label` for the caller's output line.
proc_label=""
vet() {
  local d="$1" kind="$2" c pids pid who=""
  if [ "$proc_check" != "ok" ]; then proc_label="processes: could not check"; return 0; fi
  # The kernel reports a canonical cwd; git may have recorded the worktree
  # through a symlink.
  c=$(canon "$d" || true)
  pids=$(holders "${c:-$d}")
  if [ -z "$pids" ]; then proc_label="processes: none"; return 0; fi
  while IFS= read -r pid; do
    [ -z "$pid" ] && continue
    if is_daemon "$pid"; then
      who="${who:+${who}, }pid ${pid} (dot-agent-deck daemon, never offered for a kill)"
    else
      who="${who:+${who}, }pid ${pid} ($(proc_cmd "$pid"))"
      queue_proc "$pid"
    fi
  done <<< "$pids"
  held_out+=("${d}|${kind}|held by ${who}")
  return 1
}

canon() { (cd "$1" 2>/dev/null && pwd -P); }

# --- Registered worktrees ---
#
# Merged-branch worktrees keep the original rule (never the current worktree,
# never the default branch). The default-branch exclusion is NOT redundant with
# the local-branch loop below, which has always had its own. Without it this
# loop offers the MAIN CHECKOUT for removal whenever the script is run from a
# linked worktree: the main tree sits on `main`, `main`'s tip is by definition an
# ancestor of `origin/main`, and the only other guard here is "not the worktree I
# am standing in". Measured on this repo while forking the skill (issue #1089) —
# a run from a dispatch worktree printed
# `WORKTREES: /home/vfarcic/code/dot-agent-deck|main`. `git worktree remove`
# refuses a main working tree, so the blast radius was a confusing failure rather
# than a deletion; but the skill presents this list for confirmation, and a list
# that routinely contains the main checkout teaches the operator to confirm
# without reading. It also made the skill's own "cleanup.sh already excludes the
# default branch" a false claim.
#
# The default-branch test alone still let the main checkout through when it sat
# on a merged FEATURE branch — the shape this box was in on 2026-10-02, its main
# checkout on `chore/gpt-6.1-sol`. The first porcelain record is always the main
# working tree, so it is now excluded by position as well as by branch.
worktrees_out=()
detached_out=()
caches_out=()
registered=$'\n'   # canonical paths of every registered worktree
main_worktree=""
wt_path=""
wt_head=""
while IFS= read -r line; do
  case "$line" in
    "worktree "*)
      wt_path="${line#worktree }"
      [ -z "$main_worktree" ] && main_worktree="$wt_path"
      c=$(canon "$wt_path" || true)
      [ -n "$c" ] && registered="${registered}${c}"$'\n'
      ;;
    "HEAD "*) wt_head="${line#HEAD }" ;;
    "branch refs/heads/"*)
      br="${line#branch refs/heads/}"
      if [ "$wt_path" != "$current_worktree" ] && [ "$wt_path" != "$main_worktree" ] \
         && [ "$br" != "$default_branch" ] && is_merged "$br" "refs/heads/${br}"; then
        if vet "$wt_path" "worktree"; then worktrees_out+=("${wt_path}|${br}"); fi
      fi
      ;;
    "detached")
      if [ "$wt_path" != "$current_worktree" ] && [ "$wt_path" != "$main_worktree" ] \
         && [ -d "$wt_path" ]; then
        case "${wt_path##*/}" in
          *-land)
            # `/land-prs` keeps this detached worktree for resolving conflicts.
            if vet "$wt_path" "tool-cache land-worktree"; then
              caches_out+=("${wt_path}|land-worktree|size=$(dir_size "$wt_path")|${proc_label}|$(git_label "$wt_path")")
            fi
            ;;
          *)
            if vet "$wt_path" "detached-worktree"; then
              detached_out+=("${wt_path}|HEAD ${wt_head:0:12}|size=$(dir_size "$wt_path")|${proc_label}|$(git_label "$wt_path")")
            fi
            ;;
        esac
      fi
      ;;
    "") wt_path=""; wt_head="" ;;
  esac
done < <(git worktree list --porcelain 2>/dev/null || true)

# --- Local branches that are merged (never current / default) ---
# A branch checked out in another worktree is still listed here; it is only
# deletable once its worktree is removed, hence the worktree-first ordering in
# the skill's cleanup step.
#
# Every branch offered here or below has a tip that is on a remote or is a merged
# PR's head — those are `is_merged`'s only two ways to succeed — so none carries
# the UNPUSHED label the directory lists can.
local_out=()
while IFS= read -r b; do
  [ -z "$b" ] && continue
  [ "$b" = "$default_branch" ] && continue
  [ "$b" = "$current_branch" ] && continue
  if is_merged "$b" "refs/heads/${b}"; then local_out+=("${b} ${vetted_tip}"); fi
done < <(git branch --format='%(refname:short)' 2>/dev/null || true)

# --- Remote branches that are merged (never default) ---
remote_out=()
while IFS= read -r b; do
  b="${b#origin/}"
  [ -z "$b" ] && continue
  [ "$b" = "HEAD" ] && continue
  [ "$b" = "$default_branch" ] && continue
  if is_merged "$b" "refs/remotes/origin/${b}"; then remote_out+=("${b} ${vetted_tip}"); fi
done < <(git branch -r --format='%(refname:short)' 2>/dev/null | grep '^origin/' || true)

# --- Sibling directories: tool caches and strays ---
#
# Scanned beside the MAIN checkout, where rule 14's `../<repo>-<suffix>` scheme
# puts worktrees and where the tools below put their caches, matching only
# `<repo>-*` and `dad-*` so an unrelated project next door is never listed.
# Registered worktrees are left to the loop above. Symlinks are skipped: a
# removal would act on the link or, worse, on what it points at.
#
# Known caches are recreated on demand, at the cost of a cold build:
# - `<repo>-xver-*` — `cargo xver`'s build clone, target dirs, cargo home,
#   release downloads and run sandboxes (docs/develop/cross-version-harness.md).
#   Plain directories, not worktrees: `-xver-src` is a standalone clone.
# - `<repo>-land` — `/land-prs`'s worktree, handled above because it is a
#   registered (detached) worktree. One that is no longer registered is a stray.
strays_out=()
sibling_root=""
repo_name=""
if [ -n "$main_worktree" ] && sibling_root=$(canon "${main_worktree}/.."); then
  repo_name="${main_worktree##*/}"
  main_canon=$(canon "$main_worktree" || true)
  current_canon=$(canon "$current_worktree" || true)
  for d in "${sibling_root}/${repo_name}"-* "${sibling_root}"/dad-*; do
    [ -d "$d" ] || continue
    [ -L "$d" ] && continue
    d=$(canon "$d") || continue
    [ "$d" = "$main_canon" ] && continue
    [ "$d" = "$current_canon" ] && continue
    case "$registered" in *$'\n'"$d"$'\n'*) continue ;; esac
    case "${d##*/}" in
      "${repo_name}"-xver-*)
        if vet "$d" "tool-cache xver"; then
          caches_out+=("${d}|xver|size=$(dir_size "$d")|${proc_label}|$(git_label "$d")")
        fi
        ;;
      *)
        if vet "$d" "stray"; then
          strays_out+=("${d}|size=$(dir_size "$d")|modified=$(dir_modified "$d")|${proc_label}|$(git_label "$d")")
        fi
        ;;
    esac
  done
fi

# --- Processes whose working directory was deleted ---
#
# A finished unit's `vite preview` outliving the worktree it ran in is the case
# this exists for: nothing else names it, and its cwd no longer resolves.
if [ "$proc_check" = "ok" ]; then
  while IFS=$'\t' read -r pid cwd; do
    [ -z "$pid" ] && continue
    case "$cwd" in *" (deleted)") ;; *) continue ;; esac
    [ -d "${proc_root}/${pid}" ] || continue
    is_daemon "$pid" && continue
    queue_proc "$pid"
  done <<< "$procs"
fi

processes_out=()
while IFS= read -r pid; do
  [ -z "$pid" ] && continue
  [ -d "${proc_root}/${pid}" ] || continue
  cwd=$(readlink "${proc_root}/${pid}/cwd" 2>/dev/null || echo "?")
  processes_out+=("${pid}|${cwd}|$(proc_cmd "$pid")")
done <<< "$proc_pids"

# --- Output structured summary ---
#
# Each branch is printed as `<name> <sha>`, where the SHA is the tip this script
# actually vetted. That is not decoration: it is what lets the delete step verify
# a squash-merged branch, which `git branch -d` cannot (it tests ancestry, and a
# squash merge never puts the branch's commits on the default branch). The skill
# compares the branch's current tip against this value before deleting it.
#
# Directory and process lines are `|`-separated, with free text (git labels,
# command lines) in the LAST field so a `|` inside it cannot shift the others.
echo "DEFAULT_BRANCH=${default_branch}"
echo "PROC_CHECK=${proc_check}"
[ -n "$sibling_root" ] && echo "SIBLING_ROOT=${sibling_root}"

total=$(( ${#worktrees_out[@]} + ${#local_out[@]} + ${#remote_out[@]} + ${#detached_out[@]} \
        + ${#caches_out[@]} + ${#strays_out[@]} + ${#held_out[@]} + ${#processes_out[@]} ))
if [ "$total" -eq 0 ]; then
  echo "NOTHING_TO_CLEAN=true"
  exit 0
fi
echo "NOTHING_TO_CLEAN=false"

print_list() { # <header> <items...>
  local h="$1" x; shift
  echo "${h}:"
  # `if`, not `[ -n ] &&`: a function whose last command is a false `&&` test
  # returns 1, and `set -e` then kills the script on an empty final list.
  for x in "$@"; do
    if [ -n "$x" ]; then echo "  ${x}"; fi
  done
}

print_list WORKTREES "${worktrees_out[@]:-}"
print_list LOCAL_BRANCHES "${local_out[@]:-}"
print_list REMOTE_BRANCHES "${remote_out[@]:-}"
print_list DETACHED_WORKTREES "${detached_out[@]:-}"
print_list TOOL_CACHES "${caches_out[@]:-}"
print_list STRAY_DIRS "${strays_out[@]:-}"
print_list HELD_DIRS "${held_out[@]:-}"
print_list PROCESSES "${processes_out[@]:-}"

exit 0
