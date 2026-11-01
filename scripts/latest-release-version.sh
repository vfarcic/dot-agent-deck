#!/usr/bin/env bash
# The version a CI or sandbox build reports: the newest `vX.Y.Z` release tag at
# a remote, printed without its `v` (issue #326, issue #2052).
#
#   scripts/latest-release-version.sh <remote>
#
# `<remote>` is anything `git ls-remote` takes: a remote name (`origin`) or a
# URL. Prints the version on stdout and exits 0, or prints why on stderr and
# exits 1 when the remote cannot be read or holds no release tag.
#
# Why it exists: CI's checkouts and the RWX sandbox's are shallow clones with no
# tags, so `build.rs`'s `git describe` finds none and a build reports the
# `0.1.0` placeholder as "version unknown" (`0.1.0+unknown`), which `remote
# upgrade` without `--version` refuses. Each caller exports what this prints as
# `DAD_VERSION`, which is what `git describe --tags --abbrev=0` reports in a
# developer's checkout of `main`. `git ls-remote` reads the tags without the
# history a deep fetch would download. Pre-release tags (`v0.25.0-alpha.0`) are
# skipped, as a developer's `main` is never on one.
#
# The callers, which must not grow their own copy of the lookup:
#   .github/workflows/ci.yml  the `changes` job's `version` step
#   .rwx/ci.yml               the `backend` task
#   .rwx/sandbox.yml          the `version` task
# xtask/linkage-check/src/release_version_lookup.rs tests this script and that
# each caller runs it.
set -euo pipefail

if [ "$#" -ne 1 ] || [ -z "$1" ]; then
  echo "usage: $0 <remote>" >&2
  exit 1
fi
remote=$1

if ! tags=$(git ls-remote --tags --refs "$remote" 'v*'); then
  echo "could not list the tags at $remote" >&2
  exit 1
fi

version=$(printf '%s\n' "$tags" \
  | sed -n 's#.*refs/tags/v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$#\1#p' | sort -V | tail -n 1)
if [ -z "$version" ]; then
  echo "no vX.Y.Z release tag found at $remote" >&2
  exit 1
fi
printf '%s\n' "$version"
