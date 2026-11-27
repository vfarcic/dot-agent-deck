#!/usr/bin/env bash
# Host screenshots for a pull request's description (issue #2438).
#
#   .claude/skills/pr-create/pr-screenshots.sh <pr-number> <image>[=<alt text>]...
#
# Pass the images of every client that has the feature, each with the alt text
# the PR body should carry: what the frame shows, not its file name. An image
# given without `=<alt text>` prints with an `ALT TEXT NEEDED` placeholder to
# replace.
#
# Every screenshot comes in a light and a dark version (PR #2491), named
# `<scene>-<client>-light.png` and `<scene>-<client>-dark.png` as
# `cargo docs-screenshots` writes them. After the one image line per file, the
# script prints each pair it was given as a two-column `| Light | Dark |` table,
# ready to go under the pair's caption in the body; an image passed without its
# other half is printed as a line only, and a missing half is named on stderr.
#
# GitHub has no API for the image uploads its web editor does, so the images go
# to a branch of their own, `pr-screenshots`, which shares no history with
# `main` and is never merged: each file lands at `<pr-number>/<file name>` in a
# new commit on top of it, built with git plumbing so the working tree, the
# index and the current branch are left alone. The script prints one Markdown
# image line per file, with a URL pinned to that commit, ready to paste into the
# PR body. Uploading a file of the same name again replaces it on the branch
# and prints a new URL; a body that still carries the old URL keeps showing the
# old image.
#
# Another agent may push to the branch between this script's fetch and its
# push; the push is then rejected as a non-fast-forward, and the script fetches
# again and rebuilds its commit on the new tip, up to five times.
#
# The printed URLs name the GitHub repository the images were pushed to, read
# from the remote's URL (https, with or without credentials, `git@github.com:`, or
# `ssh://` with or without a port), so a contributor whose `origin` is a fork gets links
# into the fork, which a pull request on the upstream repository displays as
# long as the fork is public. PR_SCREENSHOTS_REMOTE overrides the remote
# (default `origin`); a remote that is not a GitHub URL, such as the path of a
# scratch bare repository the script is tried against, needs
# PR_SCREENSHOTS_REPO=<owner>/<repo> to name the repository its URLs point at.
#
# Every input must be a regular file, not a symlink, whose first bytes are a
# PNG, JPEG, GIF or WebP signature: the branch is public, and a file that only
# carries an image's extension could be anything readable on this machine.
#
# What goes into the printed Markdown is checked before anything is pushed
# (PR #2491 audit K4): alt text may not hold brackets, '|', a line break (CR or
# LF), a tab or any other control character, and the repository, whether from
# PR_SCREENSHOTS_REPO or the remote's URL, must be exactly one GitHub
# <owner>/<name>. `xtask/linkage-check/src/pr_screenshots.rs` drives these
# refusals against a local bare repository.

set -euo pipefail

branch=pr-screenshots
remote=${PR_SCREENSHOTS_REMOTE:-origin}

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <pr-number> <image>[=<alt text>]..." >&2
  exit 2
fi
pr=$1
shift
case "$pr" in
  '' | *[!0-9]*) echo "ERROR: the PR number must be digits, got: $pr" >&2; exit 2 ;;
esac

# The bytes an image of each accepted kind starts with (hex), checked against
# the file's content rather than its name.
image_signature() {
  local head
  head=$(od -An -tx1 -N12 -- "$1" | tr -d ' \n')
  case "$head" in
    89504e470d0a1a0a*) return 0 ;;                 # PNG
    ffd8ff*) return 0 ;;                           # JPEG
    474946383761* | 474946383961*) return 0 ;;     # GIF87a, GIF89a
    52494646????????57454250) return 0 ;;          # RIFF....WEBP
  esac
  return 1
}

# Whether a string holds a control character: CR, LF, a tab, an escape, DEL or
# any other byte below 0x20. CommonMark ends a line at CR as well as LF, so
# either would let alt text end the image line and start a heading or a link
# of its own (PR #2491 audit K4).
has_control() {
  local LC_ALL=C
  [[ $1 =~ [[:cntrl:]] ]]
}

declare -a files=() names=() alts=()
for arg in "$@"; do
  f=${arg%%=*}
  if [ "$f" = "$arg" ]; then alt=""; else alt=${arg#*=}; fi
  if [ -L "$f" ]; then echo "ERROR: a symlink, not an image file: $f" >&2; exit 2; fi
  [ -f "$f" ] || { echo "ERROR: no such file: $f" >&2; exit 2; }
  name=$(basename -- "$f")
  case "$name" in
    *[!A-Za-z0-9._-]*) echo "ERROR: rename it to letters, digits, '.', '_' and '-' only: $name" >&2; exit 2 ;;
  esac
  case "$name" in
    *.png | *.jpg | *.jpeg | *.gif | *.webp) ;;
    *) echo "ERROR: not a .png, .jpg, .jpeg, .gif or .webp image: $name" >&2; exit 2 ;;
  esac
  image_signature "$f" || { echo "ERROR: its content is not a PNG, JPEG, GIF or WebP image: $f" >&2; exit 2; }
  if has_control "$alt"; then
    echo "ERROR: alt text may not contain a line break, a tab or another control character: $(printf '%q' "$alt")" >&2
    exit 2
  fi
  case "$alt" in
    *[\]\[\|]*) echo "ERROR: alt text may not contain brackets or '|': $alt" >&2; exit 2 ;;
  esac
  for seen in "${names[@]+"${names[@]}"}"; do
    [ "$seen" != "$name" ] || { echo "ERROR: two files named $name" >&2; exit 2; }
  done
  files+=("$f")
  names+=("$name")
  alts+=("${alt:-ALT TEXT NEEDED: say what $name shows}")
done

# The repository the URLs name is the one the push goes to.
if [ -n "${PR_SCREENSHOTS_REPO:-}" ]; then
  repo=$PR_SCREENSHOTS_REPO
else
  url=$(git remote get-url "$remote" 2>/dev/null || printf '%s' "$remote")
  repo=$(printf '%s\n' "$url" | sed -nE 's#^(https://([^@/]+@)?github\.com/|ssh://git@github\.com(:[0-9]+)?/|git@github\.com:)([^/]+/[^/]+)$#\4#p' | sed -E 's#\.git$##; s#/$##')
  if [ -z "$repo" ]; then
    echo "ERROR: $remote ($url) is not a GitHub repository; set PR_SCREENSHOTS_REPO=<owner>/<repo> to the repository the images are served from" >&2
    exit 2
  fi
fi
# Both sources, the override and the remote's URL, must name exactly one
# GitHub repository: an owner of letters, digits and single inner hyphens (at
# most 39 characters), and a name of letters, digits, '.', '_' and '-' that is
# not '.' or '..'. Anything else could carry text out of the URLs below and into
# the PR body (PR #2491 audit K4).
valid_repo() {
  local LC_ALL=C owner name
  [[ $1 =~ ^([A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?)/([A-Za-z0-9._-]{1,100})$ ]] || return 1
  owner=${BASH_REMATCH[1]}
  name=${BASH_REMATCH[3]}
  [[ $owner != *--* && $name != "." && $name != ".." ]]
}
if ! valid_repo "$repo"; then
  echo "ERROR: not an <owner>/<name> GitHub repository: $(printf '%q' "$repo")" >&2
  exit 2
fi
tmp=$(mktemp -d)
tip_ref=refs/pr-screenshots-upload/tip-$$
cleanup() {
  rm -rf "$tmp"
  git update-ref -d "$tip_ref" 2>/dev/null || true
}
trap cleanup EXIT
export GIT_INDEX_FILE="$tmp/index"

commit=""
for attempt in 1 2 3 4 5; do
  # Exit status 2 from ls-remote means the branch does not exist yet; any other
  # failure (network, auth) is not that, and stops the script.
  parent=""
  if git ls-remote --exit-code --heads "$remote" "$branch" >/dev/null; then
    git fetch -q "$remote" "+refs/heads/$branch:$tip_ref"
    parent=$(git rev-parse "$tip_ref")
    git read-tree "$parent"
  else
    status=$?
    [ "$status" -eq 2 ] || { echo "ERROR: could not read $remote (git ls-remote exited $status)" >&2; exit 1; }
    git read-tree --empty
  fi
  i=0
  for f in "${files[@]}"; do
    blob=$(git hash-object -w --no-filters -- "$f")
    git update-index --add --cacheinfo "100644,$blob,$pr/${names[$i]}"
    i=$((i + 1))
  done
  tree=$(git write-tree)
  if [ -n "$parent" ]; then
    commit=$(git commit-tree "$tree" -p "$parent" -m "Screenshots for PR #$pr")
  else
    commit=$(git commit-tree "$tree" -m "Screenshots for pull request descriptions (see .claude/skills/pr-create/pr-screenshots.sh)")
  fi
  if git push -q "$remote" "$commit:refs/heads/$branch" 2>"$tmp/push.err"; then
    break
  fi
  if [ "$attempt" -eq 5 ]; then
    cat "$tmp/push.err" >&2
    echo "ERROR: the push to $branch was rejected five times" >&2
    exit 1
  fi
  echo "push to $branch rejected (attempt $attempt), fetching its new tip and retrying" >&2
  rm -f "$GIT_INDEX_FILE"
  sleep $((attempt * 2))
done

declare -a lines=()
i=0
for name in "${names[@]}"; do
  lines+=("![${alts[$i]}](https://raw.githubusercontent.com/$repo/$commit/$pr/$name)")
  echo "${lines[$i]}"
  i=$((i + 1))
done

# Each light image with its dark half, as the table the PR body shows.
i=0
for name in "${names[@]}"; do
  case "$name" in
    *-light.*)
      want="${name%-light.*}-dark.${name##*.}"
      found=""
      j=0
      for other in "${names[@]}"; do
        [ "$other" != "$want" ] || found=$j
        j=$((j + 1))
      done
      if [ -n "$found" ]; then
        printf '\n| Light | Dark |\n| --- | --- |\n| %s | %s |\n' "${lines[$i]}" "${lines[$found]}"
      else
        echo "NOTE: $name has no $want beside it; every screenshot comes in both themes" >&2
      fi
      ;;
    *-dark.*)
      want="${name%-dark.*}-light.${name##*.}"
      found=""
      for other in "${names[@]}"; do [ "$other" != "$want" ] || found=1; done
      [ -n "$found" ] || echo "NOTE: $name has no $want beside it; every screenshot comes in both themes" >&2
      ;;
  esac
  i=$((i + 1))
done
