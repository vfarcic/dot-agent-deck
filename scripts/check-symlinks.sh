#!/usr/bin/env bash
#
# Verify that every symlink this repository tracks (git mode 120000) actually
# materialised as a link in the working tree.
#
# Issue #552. When git checks out a symlink on a system where `core.symlinks`
# is off — the Windows default unless Developer Mode is on or the clone ran
# elevated — it writes a PLAIN TEXT FILE whose entire content is the link
# target. Nothing errors, and the file is present and readable, so the failure
# is silent:
#
#   * `AGENTS.md` becomes a 9-byte file containing the string "CLAUDE.md".
#     Codex and OpenCode read AGENTS.md for the project's conventions, so they
#     get NO project instructions and behave like a correctly configured agent
#     that simply has nothing to follow.
#   * The `.agents/skills` link becomes a one-line text file, so the
#     whole skills tree resolves to nothing.
#   * `docs/img` stops being the site's image directory.
#
# Usage:
#   scripts/check-symlinks.sh              # check this working tree
#   scripts/check-symlinks.sh <dir>        # check a tree materialised elsewhere
#   scripts/check-symlinks.sh --self-test  # prove the check can actually fail
#
# --self-test writes the index into a temp directory with core.symlinks=false —
# the exact state a Windows clone lands in — and asserts that this script
# rejects that tree, for the materialisation reason specifically. CI runs it
# immediately before the real check so that a green result is never the vacuous
# kind: the same script, on the same machine, is shown failing on a broken tree
# seconds before it passes on the real one.
#
# Portable across Linux, macOS and Git Bash on Windows: it uses git plus
# coreutils only (cmp/wc/cat/dirname/mktemp, all present in Git for Windows'
# bundled /usr/bin), and never depends on `test -L`, whose answer for a native
# Windows symlink is a property of the shell rather than of the checkout.

set -euo pipefail

usage() {
    cat <<'EOF'
Verify that every symlink this repository tracks materialised as a link.

  scripts/check-symlinks.sh              check this working tree
  scripts/check-symlinks.sh <dir>        check a tree materialised elsewhere
  scripts/check-symlinks.sh --self-test  prove the check can actually fail

See the comment at the top of this script, and docs/develop/windows-symlinks.md.
EOF
    exit "${1:-0}"
}

repo_root="$(git rev-parse --show-toplevel)"

# Emits "<path>\t<link target>" for every tracked symlink. `git ls-files -s`
# records are "<mode> <sha> <stage>\t<path>"; -z makes them NUL-terminated so a
# path containing whitespace stays intact.
tracked_symlinks() {
    local record meta path mode sha stage
    while IFS= read -r -d '' record; do
        meta="${record%%$'\t'*}"
        path="${record#*$'\t'}"
        read -r mode sha stage <<<"$meta"
        [ "$mode" = "120000" ] || continue
        # The blob of a symlink IS the link target, so this reads what the link
        # is supposed to point at without touching the working tree.
        printf '%s\t%s\n' "$path" "$(git -C "$repo_root" cat-file blob "$sha")"
    done < <(git -C "$repo_root" ls-files -s -z)
}

# The directory a path ends up at with every symlink in it resolved, or nothing
# if it cannot be entered. `cd` + `pwd -P` rather than `realpath` (not on macOS)
# or `readlink -f` (not on older macOS): both are shell builtins, and both sides
# of a comparison go through the same transformation, so equality means the two
# spellings name one directory.
resolved_dir() {
    (cd "$1" 2>/dev/null && pwd -P)
}

# Why the on-disk entry is wrong, in the words the reader needs.
diagnose() {
    local link="$1" target="$2" expected="$3" where
    if [ ! -e "$link" ]; then
        printf 'is missing from the working tree entirely'
    elif [ -f "$link" ] && [ "$(cat "$link")" = "$target" ]; then
        printf 'did not materialise: it is a %s-byte plain file whose content is the literal text "%s"' \
            "$(wc -c <"$link" | tr -d '[:space:]')" "$target"
    elif [ -d "$expected" ] && [ -d "$link" ]; then
        where="$(resolved_dir "$link")"
        printf 'resolves to %s, not to %s' "${where:-nowhere readable}" "$(resolved_dir "$expected")"
    else
        printf 'does not resolve to its target'
    fi
}

# 0 = every tracked symlink resolves to its target in $1; 1 = at least one does
# not; 2 = nothing was checked (see the vacuity guard below). Prints one line
# per offender, so a broken tree names every one rather than stopping at the first.
check_tree() {
    local tree="$1"
    local path target link expected here checked=0 broken=0

    # Without this, a mistyped directory reports every link as dangling — a
    # true statement that points at the wrong problem.
    if [ ! -d "$tree" ]; then
        echo "no such directory: $tree" >&2
        return 2
    fi

    while IFS=$'\t' read -r path target; do
        checked=$((checked + 1))
        link="$tree/$path"
        expected="$tree/$(dirname "$path")/$target"

        if [ ! -e "$expected" ]; then
            printf '  %s -> %s\n      the target does not exist; the committed symlink is dangling\n' \
                "$path" "$target"
            broken=$((broken + 1))
            continue
        fi

        # Resolve through the link and compare against the target itself, which
        # is the property that matters (an agent must read CLAUDE.md's content,
        # not a file named after it) and which holds however the platform
        # represents the link.
        #
        # Both branches assert IDENTITY, not merely the right shape. Checking
        # that a directory link is a directory would pass a link redirected at
        # some other existing directory — the whole skills tree could point
        # somewhere else and the check would still be green (Greptile, #553).
        # For a file, equal content is the property being protected, so `cmp`
        # is the right test and a byte-identical file elsewhere is not a defect.
        if [ -d "$expected" ]; then
            here="$(resolved_dir "$link")"
            if [ -n "$here" ] && [ "$here" = "$(resolved_dir "$expected")" ]; then
                continue
            fi
        else
            if [ -f "$link" ] && cmp -s "$link" "$expected"; then
                continue
            fi
        fi

        printf '  %s -> %s\n      %s\n' "$path" "$target" "$(diagnose "$link" "$target" "$expected")"
        broken=$((broken + 1))
    done < <(tracked_symlinks)

    # Refuse to report success on an empty set. Zero would otherwise be the
    # answer both when the repo genuinely tracks no symlinks and when git told
    # us nothing (wrong directory, unreadable index) — and the second reading
    # turns this whole check into a green light that means nothing.
    if [ "$checked" -eq 0 ]; then
        echo "no tracked symlinks found at all — refusing to report success vacuously" >&2
        return 2
    fi

    if [ "$broken" -gt 0 ]; then
        printf '%s of %s tracked symlinks are wrong in %s\n' "$broken" "$checked" "$tree"
        return 1
    fi

    printf 'all %s tracked symlinks materialised in %s\n' "$checked" "$tree"
}

remedy() {
    cat <<'EOF'

This is what a clone looks like when git could not create symlinks — the
Windows default when neither Developer Mode nor an elevated shell grants the
privilege, and `core.symlinks` is therefore off.

To fix a Windows clone:

  1. Turn on Developer Mode (Settings > System > For developers), or run git
     from an elevated shell.
  2. git config --global core.symlinks true
  3. Re-materialise this clone:  git checkout -- .

Step 3 is enough on its own once step 2 is in place: with core.symlinks on,
git sees a regular file where the index says symlink, treats it as modified,
and rewrites it as a link. Verified on windows-latest.

See docs/develop/windows-symlinks.md.
EOF
}

# The remedy is for the materialisation failure specifically, so it is printed
# only for that (exit 1) and not for the vacuity guard (exit 2), which is a
# problem with how the script was invoked rather than with the tree.
run_check() {
    local rc=0
    check_tree "$1" || rc=$?
    if [ "$rc" -eq 1 ]; then
        remedy
    fi
    exit "$rc"
}

self_test() {
    local tmp tmp_git out
    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064  # expand $tmp now, not at trap time
    trap "rm -rf '$tmp'" EXIT

    # Git for Windows is a native Windows binary and does not understand the
    # MSYS `/tmp/...` path that mktemp hands back, so give it the Windows
    # spelling. cygpath exists only there, so this is a no-op elsewhere.
    tmp_git="$tmp"
    if command -v cygpath >/dev/null 2>&1; then
        tmp_git="$(cygpath -m "$tmp")"
    fi

    # The whole index (~16 MB), not just the symlinks: the check compares each
    # link against its target, so the targets have to be there too — otherwise
    # this would fail for the dangling-target reason and prove nothing about
    # materialisation.
    git -C "$repo_root" -c core.symlinks=false checkout-index --all --force --prefix="$tmp_git/"

    if out="$(check_tree "$tmp" 2>&1)"; then
        printf '%s\n' "$out"
        echo "self-test FAILED: the check passed on a tree where no symlink materialised" >&2
        exit 1
    fi

    case "$out" in
    *"did not materialise"*) ;;
    *)
        printf '%s\n' "$out"
        echo "self-test FAILED: the broken tree was rejected, but not for the materialisation reason" >&2
        exit 1
        ;;
    esac

    echo "self-test ok: a tree checked out with core.symlinks=false is rejected"
}

case "${1:---check}" in
--self-test)
    self_test
    ;;
-h | --help)
    usage
    ;;
--check)
    run_check "$repo_root"
    ;;
-*)
    echo "unknown option: $1" >&2
    usage 1 >&2
    ;;
*)
    run_check "$1"
    ;;
esac
