#!/usr/bin/env bash
#
# Resolve and VALIDATE the version being released, then print it to stdout.
#
# Port of the "Extract version" step in .github/workflows/release.yml. It lives
# in a script rather than inline in the pipeline YAML for two reasons: the
# SemVer gate is ~40 lines of assembled ERE that would be unreadable as a YAML
# block scalar, and — unlike GHA, which computes the version once in `prepare`
# and passes it to every consumer as a job output — Semaphore blocks share no
# outputs, so several blocks each resolve it independently. One implementation,
# called from each, is the only way those stay in agreement.
#
# The GHA `workflow_dispatch` input has no counterpart here: Semaphore has no
# dispatch trigger, so the tag is the only source. See
# docs/develop/semaphore-ci.md.
set -euo pipefail

# Pin the collation for the whole script: the SemVer gate below matches with
# bash's `=~`, whose bracket expressions are resolved against the active locale
# rather than against ASCII. Under en_US.UTF-8 the GHA version of this gate
# accepted `1.2.3-é` and full-width `１.２.３`; under C it rejects them. Pinned
# here so nothing added later can reintroduce a locale-dependent comparison.
export LC_ALL=C

if [ -z "${SEMAPHORE_GIT_TAG_NAME:-}" ]; then
  echo "ERROR: SEMAPHORE_GIT_TAG_NAME is empty. This pipeline only runs for tag-triggered workflows." >&2
  exit 1
fi
VERSION="${SEMAPHORE_GIT_TAG_NAME#v}"

# SemVer 2.0.0 transcribed into POSIX ERE (bash's `=~` has no non-capturing
# groups), assembled from named pieces because the character classes have to be
# spelled out and the one-line form would be unreadable.
#
# WHY THE CLASSES ARE ENUMERATED AND NOT RANGES: a bracket *range* in an ERE is
# interpreted through the active locale's collation, so `[0-9]` and `[a-zA-Z]`
# are not ASCII-only. Belt and braces with the LC_ALL=C above — these
# enumerations mean the same thing in any locale, so neither defence alone is
# load-bearing and a reader does not have to guess which one is.
d='[0123456789]'
nz='[123456789]'
# `-` last inside a bracket expression is a literal, never a range end.
alpha='[abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-]'
alnum='[0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-]'
# The {0,18} bound on the three core fields keeps this gate at least as strict
# as build.rs, whose `semver::Version::parse` stores each field in a u64: an
# unbounded digit run would accept a 20-digit core that the build script then
# rejects, silently falling DAD_VERSION back to git so the artifact misreports
# its own version (issue #250).
core="(0|${nz}${d}{0,18})"
ident="(0|${nz}${d}*|${d}*${alpha}${alnum}*)"
# `[.]`/`[+]` rather than `\.`/`\+`: a single-character bracket expression
# carries no range semantics and no backslash to survive quoting.
semver_re="^${core}[.]${core}[.]${core}(-${ident}([.]${ident})*)?([+]${alnum}+([.]${alnum}+)*)?"'$'

if [[ ! "$VERSION" =~ $semver_re ]]; then
  # %q escapes the rejected value so it cannot span multiple lines.
  printf 'Rejected version (escaped): %q\n' "$VERSION" >&2
  echo "ERROR: Refusing to release: version is not valid SemVer. Expected X.Y.Z with no leading 'v', optionally -prerelease and +build." >&2
  exit 1
fi

printf '%s' "$VERSION"
