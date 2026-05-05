#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:?Usage: assemble-changelog.sh <version>}"
CHANGELOG_DIR="changelog.d"
CHANGELOG_FILE="CHANGELOG.md"
DATE=$(date +%Y-%m-%d)

# Collect entries by type.
# Supports both Keep-a-Changelog names (added, changed, fixed, removed)
# and semantic fragment names (feature, breaking, bugfix, doc, misc).
declare -A TYPE_HEADERS=(
  [added]="Added"
  [feature]="Added"
  [changed]="Changed"
  [breaking]="Changed"
  [fixed]="Fixed"
  [bugfix]="Fixed"
  [removed]="Removed"
  [doc]="Documentation"
  [misc]="Miscellaneous"
)

# Ordered list of types to scan — earlier entries appear first in the changelog.
TYPES=(breaking added feature changed fixed bugfix removed doc misc)

# Fail loudly if changelog.d/ contains fragments with unrecognized suffixes,
# rather than silently skipping them. (v0.24.3 shipped with `*.fix.md` fragments
# that were ignored because only `*.bugfix.md`/`*.fixed.md` are recognized,
# leaving the GitHub release body and CHANGELOG.md empty for that version.)
if [ -d "$CHANGELOG_DIR" ]; then
  unknown_fragments=()
  while IFS= read -r -d '' f; do
    name="$(basename "$f")"
    [[ "$name" == ".gitkeep" ]] && continue
    matched=false
    for type in "${TYPES[@]}"; do
      [[ "$name" == *.${type}.md ]] && { matched=true; break; }
    done
    $matched || unknown_fragments+=("$name")
  done < <(find "$CHANGELOG_DIR" -maxdepth 1 -name '*.md' -print0 2>/dev/null)

  if [ ${#unknown_fragments[@]} -gt 0 ]; then
    echo "ERROR: changelog.d/ contains fragments with unrecognized type suffix:" >&2
    printf '  %s\n' "${unknown_fragments[@]}" >&2
    echo >&2
    echo "Recognized types: ${TYPES[*]}" >&2
    echo "Rename each fragment so its suffix matches one of the recognized types (e.g. '.bugfix.md', '.feature.md')." >&2
    exit 1
  fi
fi

section=""
processed_files=()
seen_headers=()

for type in "${TYPES[@]}"; do
  fragments=()
  while IFS= read -r -d '' f; do
    fragments+=("$f")
  done < <(find "$CHANGELOG_DIR" -name "*.$type.md" -print0 2>/dev/null | sort -z)

  if [ ${#fragments[@]} -gt 0 ]; then
    header="${TYPE_HEADERS[$type]}"
    # Deduplicate headers (e.g. added & feature both map to "Added")
    if [[ ! " ${seen_headers[*]:-} " =~ " ${header} " ]]; then
      section+="### ${header}"$'\n\n'
      seen_headers+=("$header")
    fi
    for f in "${fragments[@]}"; do
      processed_files+=("$f")
      while IFS= read -r line; do
        # Skip blank lines
        [[ -z "$line" ]] && continue
        # Convert markdown headings to bold list items
        if [[ "$line" =~ ^##\ (.+) ]]; then
          section+="- **${BASH_REMATCH[1]}**"$'\n'
        else
          section+="  $line"$'\n'
        fi
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

# Prepend to CHANGELOG.md, preserving the header if present
if [ -f "$CHANGELOG_FILE" ]; then
  first_line=$(head -n 1 "$CHANGELOG_FILE")
  if [[ "$first_line" =~ ^#\ Changelog ]]; then
    rest=$(tail -n +2 "$CHANGELOG_FILE" | sed '/./,$!d')
    printf '%s\n\n%s\n\n%s\n' "$first_line" "$release_section" "$rest" > "$CHANGELOG_FILE"
  else
    existing=$(cat "$CHANGELOG_FILE")
    printf '%s\n\n%s\n' "$release_section" "$existing" > "$CHANGELOG_FILE"
  fi
else
  printf '# Changelog\n\n%s\n' "$release_section" > "$CHANGELOG_FILE"
fi

# Remove only processed fragments (keep .gitkeep and unprocessed files)
for f in "${processed_files[@]}"; do
  rm -f "$f"
done
