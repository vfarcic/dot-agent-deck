#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:?Usage: assemble-changelog.sh <version>}"
CHANGELOG_DIR="changelog.d"
CHANGELOG_FILE="CHANGELOG.md"
DATE=$(date +%Y-%m-%d)

# Collect entries by type
declare -A TYPE_HEADERS=(
  [added]="Added"
  [changed]="Changed"
  [fixed]="Fixed"
  [removed]="Removed"
)

section=""

for type in added changed fixed removed; do
  fragments=()
  while IFS= read -r -d '' f; do
    fragments+=("$f")
  done < <(find "$CHANGELOG_DIR" -name "*.$type.md" -print0 2>/dev/null | sort -z)

  if [ ${#fragments[@]} -gt 0 ]; then
    section+="### ${TYPE_HEADERS[$type]}"$'\n\n'
    for f in "${fragments[@]}"; do
      while IFS= read -r line; do
        section+="- $line"$'\n'
      done < "$f"
    done
    section+=$'\n'
  fi
done

if [ -z "$section" ]; then
  echo "No changelog fragments found in $CHANGELOG_DIR/" >&2
  exit 0
fi

# Build the new release section
release_section="## [$VERSION] - $DATE"$'\n\n'"$section"

# Output to stdout (used by release workflow for release notes)
echo "$release_section"

# Prepend to CHANGELOG.md
if [ -f "$CHANGELOG_FILE" ]; then
  existing=$(cat "$CHANGELOG_FILE")
  printf '%s\n\n%s\n' "$release_section" "$existing" > "$CHANGELOG_FILE"
else
  printf '# Changelog\n\n%s\n' "$release_section" > "$CHANGELOG_FILE"
fi

# Remove processed fragments (keep .gitkeep)
find "$CHANGELOG_DIR" -name "*.md" ! -name ".gitkeep" -delete
