#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:?Usage: assemble-changelog.sh <version>}"
CHANGELOG_DIR="changelog.d"
CHANGELOG_FILE="CHANGELOG.md"
DATE=$(date +%Y-%m-%d)

# Map a fragment type to the changelog heading it is filed under.
# Supports both Keep-a-Changelog names (added, changed, fixed, removed)
# and semantic fragment names (feature, breaking, bugfix, doc, misc).
#
# A `case` rather than the `declare -A TYPE_HEADERS` this used to be. `declare
# -A` is bash 4, and macOS ships /bin/bash 3.2.57, where it is not merely
# ignored: the assignment degrades to an ordinary *indexed* array assignment,
# so `[added]` is evaluated as an ARITHMETIC subscript, `added` is read as an
# unset variable, and `set -u` killed the script on that line before it did
# anything at all (issue #593). The release job runs on ubuntu-latest, so this
# never bit a release — it bit the first maintainer to run the assembler
# locally on a Mac, and it made `tests/assemble_changelog.rs` unrunnable on the
# `build-macos` job. `.claude/skills/verify-pr/scan.sh` was rewritten away from
# `declare -A` for the same reason under issue #521; this is the same fix.
#
# LOCKSTEP with TYPES below: every type listed there must have an arm here.
# The `*)` arm makes a mismatch a loud, named error rather than a silent empty
# heading — the old associative array failed loudly too, via `set -u`, and that
# property is deliberately preserved.
type_header() {
  case "$1" in
    added|feature)    echo "Added" ;;
    changed|breaking) echo "Changed" ;;
    fixed|bugfix)     echo "Fixed" ;;
    removed)          echo "Removed" ;;
    doc)              echo "Documentation" ;;
    misc)             echo "Miscellaneous" ;;
    *)
      echo "ERROR: no changelog heading is mapped for fragment type '$1'." >&2
      echo "Add a matching arm to type_header() for its entry in TYPES." >&2
      return 1
      ;;
  esac
}

# Ordered list of types to scan — earlier entries appear first in the changelog.
# LOCKSTEP with type_header() above.
TYPES=(breaking added feature changed fixed bugfix removed doc misc)

# Fail loudly if changelog.d/ contains fragments with unrecognized suffixes,
# rather than silently skipping them. (v0.24.3 shipped with `*.fix.md` fragments
# that were ignored because only `*.bugfix.md`/`*.fixed.md` are recognized,
# leaving the GitHub release body and CHANGELOG.md empty for that version.)
#
# LOCKSTEP: this validation `find` and the collection `find` further down MUST
# scan the same tree. Both are recursive — no `-maxdepth` on either — so every
# `*.md` under changelog.d/ is suffix-checked before the collection loop can
# reach it, and nothing is deleted without having passed this gate. They
# disagreed once: validation was `-maxdepth 1` while collection was unbounded,
# so a fragment in a subdirectory was invisible to the guard and visible to the
# `rm -f` at the end, and an unrecognized one sat there while its siblings were
# consumed around it (issue #582) — the same silent-drop shape this guard was
# written to prevent, reached by the one path it did not cover. Change the depth
# of one and you must change the other.
if [ -d "$CHANGELOG_DIR" ]; then
  unknown_fragments=()
  while IFS= read -r -d '' f; do
    name="$(basename "$f")"
    [[ "$name" == ".gitkeep" ]] && continue
    matched=false
    for type in "${TYPES[@]}"; do
      [[ "$name" == *.${type}.md ]] && { matched=true; break; }
    done
    # Report the path relative to changelog.d/, so a fragment in a
    # subdirectory names the directory to look in; for the flat layout the
    # repo actually uses this is still just the filename.
    $matched || unknown_fragments+=("${f#"$CHANGELOG_DIR"/}")
  done < <(find "$CHANGELOG_DIR" -name '*.md' -print0 2>/dev/null)

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
  # LOCKSTEP with the validation `find` above — see the note there. Recursive,
  # and every path it can yield has already been suffix-checked by the time this
  # loop runs, which is what makes the `rm -f` at the end safe.
  while IFS= read -r -d '' f; do
    fragments+=("$f")
  done < <(find "$CHANGELOG_DIR" -name "*.$type.md" -print0 2>/dev/null | sort -z)

  if [ ${#fragments[@]} -gt 0 ]; then
    header="$(type_header "$type")"
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
