#!/usr/bin/env bash
set -euo pipefail

# Analyze changelog fragments and propose a semantic version bump.
# This script is read-only — it never creates commits, tags, or pushes.
#
# TWO CALLERS, and both matter (issue #1089). A maintainer runs it from
# `/tag-release` Step 1 to see the proposed version before dispatching; and
# `.github/workflows/tag-release.yml` runs it again at release time to re-derive
# that version and refuse when it disagrees with the one the operator supplied.
# Keeping it as ONE script is the point — the 0.x recalibration below is what
# `docs/develop/versioning.md` cites as the implementation of its bump table, and
# a second copy inlined in the workflow is exactly how the two would drift.
#
# The workflow runs this in a job holding the RELEASE_TOKEN admin PAT. Treat a
# change here as a change to release-path code, not to a helper script.

CHANGELOG_DIR="changelog.d"

# --- Check for pending fragments ---

if [ ! -d "$CHANGELOG_DIR" ]; then
  echo "NO_FRAGMENTS=true"
  echo "MESSAGE=No changelog.d/ directory found. Nothing to release."
  exit 0
fi

fragments=()
while IFS= read -r -d '' f; do
  fragments+=("$(basename "$f")")
done < <(find "$CHANGELOG_DIR" -maxdepth 1 -name '*.md' -not -name '.gitkeep' -print0 | sort -z)

if [ ${#fragments[@]} -eq 0 ]; then
  echo "NO_FRAGMENTS=true"
  echo "MESSAGE=No changelog fragments found. Nothing to release."
  exit 0
fi

# --- Get current version ---

current_version=$(git tag --list 'v*' --sort=-v:refname 2>/dev/null | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1)
if [ -z "$current_version" ]; then
  current_version="v0.0.0"
fi

# Validate and parse semver with regex
version="${current_version#v}"
if [[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  major="${BASH_REMATCH[1]}"
  minor="${BASH_REMATCH[2]}"
  patch="${BASH_REMATCH[3]}"
else
  echo "ERROR=true"
  echo "MESSAGE=Current tag '${current_version}' is not valid semver. Cannot determine version."
  exit 1
fi

# --- Analyze fragment types ---

has_breaking=false
has_feature=false
has_bugfix=false
unknown_fragments=()

for frag in "${fragments[@]}"; do
  case "$frag" in
    *.breaking.md) has_breaking=true ;;
    *.feature.md)  has_feature=true ;;
    *.bugfix.md)   has_bugfix=true ;;
    *.doc.md|*.misc.md) ;; # known types that don't affect bump
    *) unknown_fragments+=("$frag") ;;
  esac
done

if [ ${#unknown_fragments[@]} -gt 0 ]; then
  echo "ERROR=true"
  echo "MESSAGE=Unknown fragment type(s): ${unknown_fragments[*]}"
  exit 1
fi

# --- Calculate next version ---
#
# The fragment-type -> bump mapping depends on whether we are pre-1.0:
#
#   While major is 0, the MINOR digit is the compatibility boundary
#   (caret-rule semantics: 0.a.* and 0.b.* are incompatible for a != b).
#   So a breaking/wire-incompatible change bumps the minor, while features
#   and bugfixes ship as patch releases. The minor digit means "broke
#   compatibility", not "has new features".
#
#   From 1.0 onward, use standard semver: breaking -> major,
#   feature -> minor, bugfix -> patch.

if [ "$major" -eq 0 ]; then
  if $has_breaking; then
    bump_type="minor"
    proposed_version="v0.$(( minor + 1 )).0"
  else
    # feature, bugfix, or doc/misc-only: patch
    bump_type="patch"
    proposed_version="v0.${minor}.$(( patch + 1 ))"
  fi
else
  if $has_breaking; then
    bump_type="major"
    proposed_version="v$(( major + 1 )).0.0"
  elif $has_feature; then
    bump_type="minor"
    proposed_version="v${major}.$(( minor + 1 )).0"
  elif $has_bugfix; then
    bump_type="patch"
    proposed_version="v${major}.${minor}.$(( patch + 1 ))"
  else
    bump_type="patch"
    proposed_version="v${major}.${minor}.$(( patch + 1 ))"
  fi
fi

# --- Check HEAD for skip-ci ---
#
# ADVISORY ONLY since issue #1089. Nothing acts on this value any more: the
# release workflow makes its own check against GitHub's five documented markers
# immediately before tagging, which is the only moment at which the answer is
# binding. It is still printed because it tells a maintainer, up front, whether
# the workflow is going to insert an empty preparation commit — the usual cause
# being `docs-publish.yml`, which ends every release by committing
# `chore: update docs chart to <tag> [skip ci]` to `main`.
head_message=$(git log -1 --format="%B" HEAD 2>/dev/null || echo "")
skip_ci=false
if echo "$head_message" | grep -qiE '\[(skip ci|ci skip|no ci|skip actions|actions skip)\]'; then
  skip_ci=true
fi

# --- Output structured summary ---

echo "CURRENT_VERSION=${current_version}"
echo "PROPOSED_VERSION=${proposed_version}"
echo "BUMP_TYPE=${bump_type}"
echo "SKIP_CI=${skip_ci}"
echo "FRAGMENTS:"
for frag in "${fragments[@]}"; do
  echo "  ${frag}"
done
