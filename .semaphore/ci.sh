#!/usr/bin/env bash
# =============================================================================
# PRD #376 — Semaphore-side bootstrap and cache plumbing for the devbox spike.
# =============================================================================
#
# `.semaphore/semaphore.yml` calls this script; everything Semaphore-specific
# that is longer than one line lives here rather than inline in the YAML,
# because quoting a 30-line bash conditional inside a YAML command list is how
# pipelines acquire bugs nobody can see. Read this file together with
# `.semaphore/semaphore.yml` and `.github/workflows/ci.yml`.
#
# WHY ANY OF THIS EXISTS
#
# On GitHub Actions, three marketplace actions do this work:
#   * `jetify-com/devbox-install-action` — installs nix + devbox, caches /nix
#   * `dtolnay/rust-toolchain`          — installs a floating `stable` toolchain
#   * `Swatinem/rust-cache`            — caches ~/.cargo + target/
# None of them exist on Semaphore, so the toolchain has to be hand-provisioned
# there. That is the whole reason PRD #376 does the Semaphore side FIRST: the
# devbox layer is not extra work on this provider, it is the only option.
#
# WHAT IS PROVIDER-COUPLED AND WHY THAT IS EXPECTED
#
# The PRD says up front that caching is the part that does not abstract. The
# `cache store` / `cache restore` / `cache has_key` / `checksum` commands below
# are Semaphore toolbox commands with no GHA equivalent. The *build* steps, by
# contrast, are provider-agnostic: they are `task ci-*` entrypoints from
# `Taskfile.yml`, identical to what a contributor runs locally.
#
# THE ONE SEMAPHORE CACHE BEHAVIOUR EVERYTHING HERE IS SHAPED BY
#
# EVERY path this script hands to `cache store` is CHECKOUT-RELATIVE, and that is
# also why `CARGO_HOME` is moved inside the checkout (`cargo-home` subcommand)
# instead of being left at `$HOME/.cargo`. Please do not "helpfully" undo it.
#
# THE REASON, CORRECTED 2026-08-04 against the cache CLI's source, because the
# reason written here originally was wrong in its mechanism. This file used to
# claim that `cache store` strips a leading `/` so an absolute path "does not
# round-trip". Semaphore's docs do say tar "automatically removes any leading /
# from a given path value" — but the implementation
# (`cache-cli/pkg/archive/shell_out_archiver.go`) branches on whether the path is
# absolute and uses tar's `-P` when it is:
#
#     Compress:   filepath.IsAbs(src) ? `tar czPf dst src`  : `tar czf dst src`
#     Decompress: filepath.IsAbs(dst) ? `tar xzPf tmp -C .` : `tar xzf tmp -C .`
#
# `-P` PRESERVES the leading `/`, and the decompress side picks its branch from
# the FIRST MEMBER of the archive. So an absolute path does round-trip — back to
# its absolute location — and the original justification does not hold.
#
# Checkout-relative paths are still the right choice, for the reason that
# survives: a relative archive is extracted WITHOUT `-P`, i.e. tar strips leading
# `/` and refuses `..` members, and everything lands under the current directory.
# That is what makes `restore_member`'s scratch directory meaningful at all (see
# the comment above `adopt_restored_member`, which now records the verified
# behaviour of the `-P` branch and what it costs).
#
# VALIDATION STATUS — THIS SCRIPT HAS RUN, ON macOS ONLY, AND IT WORKED THERE
#
# A Semaphore project IS connected to this repository and the pipeline ran twice on
# 2026-08-04. `build-macos` PASSED both times (5m45s, cold bootstrap, no cache);
# both LINUX blocks failed without ever starting — `start_time: 0`, no agent
# assigned, not one command executed, because `f1-standard-4` is not available on
# this organization's plan. So nothing below was implicated in those failures, and
# nothing below has ever executed on Linux either.
#
# WHAT THE macOS RUN OBSERVED (via `diagnose-nix-env`, so measured rather than
# inferred): the pinned v3.21.9 nix installer runs and creates the APFS `/nix`
# volume; `$NIX_PROFILE_SCRIPT` exists at exactly the path semaphore.yml sources
# unguarded; the devbox install lands in `$HOME/.local/bin`; `devbox run` realises
# the aarch64-darwin closure; and `is_trusted_ref` correctly reported `no` on a
# non-default-branch push, so the trust gate declined to store. That is the whole
# bootstrap plus the cache trust decision, working on one platform.
#
# What HAS been verified against Semaphore's docs and the toolbox's own source
# rather than against a log: the `cache` CLI's exit codes (a miss is 0, an error is
# 0 unless `CACHE_FAIL_ON_ERROR=true`, `has_key` is nonzero when absent), its tar
# path handling including the `-P` branch, and the
# `SEMAPHORE_GIT_REF_TYPE`/`SEMAPHORE_GIT_BRANCH` values `is_trusted_ref` depends
# on. Each is cited at its use site, and two comments here were WITHDRAWN as wrong
# in the process — look for "corrected" and "WITHDRAWN".
#
# STILL NOT OBSERVED: the whole Linux path, and the real `cache` CLI's STORE side
# on either platform (only a non-trusted-ref restore has run, which stores
# nothing). Expect the first Linux run to still need fixes.
#
# Which is why the `diagnose-nix-env` subcommand is still here — a TEMPORARY step
# the prologue runs between the bootstrap and the unguarded `nix-daemon.sh`
# source. It has already paid for itself on macOS; it stays until a LINUX run has
# been green, because that is the platform whose failure it has yet to explain.
# Read the comment above `cmd_diagnose_nix_env` before either extending it or
# deleting it.
# =============================================================================

set -euo pipefail

# Where the nix binaries land in a multi-user (daemon) install, on both Linux
# and macOS.
NIX_BIN="/nix/var/nix/profiles/default/bin"

# The profile script a multi-user install leaves behind, which is what points a
# non-root client at the daemon. ONE constant because two things read it —
# `load_nix_env` sources it, `cmd_diagnose_nix_env` reports on it — and a
# diagnostic that reports on a *different* path than the code uses is worse than
# no diagnostic. semaphore.yml necessarily spells the same path out a third time,
# inline in the prologue; that copy is the thing being diagnosed.
NIX_PROFILE_SCRIPT="/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh"

# ---------------------------------------------------------------------------
# Pinned installers
# ---------------------------------------------------------------------------
#
# Both installers used to be `curl … | sh` against a floating URL. HTTPS is
# transport security, not artifact integrity: it proves you talked to the right
# host, not that the bytes are the bytes you reviewed. One of these performs a
# privileged multi-user nix install, so a mutated response is root code
# execution on the agent. Both are therefore PINNED to an exact version,
# downloaded to a file, digest-checked against a value recorded in this
# repository, and only then executed.
#
# HOW STRONG EACH CHECK ACTUALLY IS — stated because the two differ:
#
#   * devbox: the digests below are the vendor's own, copied from the
#     `checksums.txt` asset published in the `jetify-com/devbox` GitHub release
#     (and independently re-computed on 2026-08-04 while writing this).
#   * nix-installer: Determinate publishes NO checksum for these binaries — the
#     v3.21.9 release carries only the three platform binaries and the
#     `nix-installer.sh` shim, with no `.sha256` sidecar — and no GitHub build
#     provenance attestation either (`gh attestation verify` returns 404 for
#     both projects). So the digests below are SELF-RECORDED: measured here on
#     2026-08-04 against the pinned release assets, and re-measured by
#     `scripts/refresh-installer-digests.sh` whenever the pin moves — so UPDATE
#     THAT DATE when a refresh changes one of them. That defends against a
#     pinned artifact being mutated later, which is the realistic risk; it does
#     not defend against the artifact having been wrong when it was recorded.
#     If Determinate ever starts publishing checksums or attestations, switch to
#     verifying those instead of trusting this line.
#
# RENOVATE MANAGES THE TWO VERSION STRINGS BELOW AND NOT THE DIGESTS ABOVE.
#
# `renovate.json` has a custom manager for this file — grouped as `Semaphore CI
# installers`, which is the string to grep for — that matches the two
# `# renovate:`-annotated pins below and bumps them like any other dependency.
# It cannot compute the sha256 of a GitHub release asset, so its PR moves a
# version and leaves the digest table above describing the PREVIOUS release — at
# which point `verify_sha256` fails closed on the first step of every job. That
# is why the matching rule in `renovate.json` sets `automerge: false` for these
# two deps specifically, and it is the one thing here not to tidy up: an
# automerged version-only bump would put a pipeline on the default branch that
# hard-fails every run, on a provider nobody is watching yet.
#
# The second half of such a bump is mechanical, and the PR is not mergeable
# until it has been run and the result committed onto the branch:
#
#     task refresh-installer-digests          # or, equivalently:
#     scripts/refresh-installer-digests.sh    # --check to verify without writing
#
# It reads the versions pinned below, takes devbox's digests from the vendor's
# `checksums.txt` and self-hashes the nix-installer binaries — preserving the
# distinction drawn above — and rewrites the table above in place, refusing to
# write anything at all if a download or a lookup fails.
#
# Each annotation below must stay on the line IMMEDIATELY above its assignment —
# that adjacency is what the regex matches, and a blank line between them turns
# the pin back into something no bot is watching.

# renovate: datasource=github-releases depName=DeterminateSystems/nix-installer
NIX_INSTALLER_VERSION="v3.21.9"
# renovate: datasource=github-releases depName=jetify-com/devbox
DEVBOX_VERSION="0.17.5"

# `<asset-name> <sha256>` per platform. Unlisted platform => hard failure, not
# a silent unverified download.
nix_installer_asset() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
      printf 'nix-installer-x86_64-linux 58cf15422853e95187405d66b0cdb306e66f602218ee0032386c46b1b776a6d1' ;;
    Linux-aarch64 | Linux-arm64)
      printf 'nix-installer-aarch64-linux 3e4f83cc87025c2890293cd2a8b6889ad2a0f7c5394f87ba8ad4fc958cf2aaea' ;;
    Darwin-arm64)
      printf 'nix-installer-aarch64-darwin f6a266434f08606a023fd5bd33a77b868016256265ba5668ad0748d71d1625b0' ;;
    *) return 1 ;;
  esac
}

devbox_asset() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
      printf 'devbox_%s_linux_amd64.tar.gz eb2d8fb34266ba3befc294d7d6f56e2cd4da2cacb7a0cf52db5b8092575544f8' "$DEVBOX_VERSION" ;;
    Linux-aarch64 | Linux-arm64)
      printf 'devbox_%s_linux_arm64.tar.gz 880901fff1ce7bf48086c12d84535bc14c257b56cb0d05e93e037f2cb1b1d529' "$DEVBOX_VERSION" ;;
    Darwin-arm64)
      printf 'devbox_%s_darwin_arm64.tar.gz 0684fecd68bf2009a2ad57be1ba1ea2bbd735a02017fff355cea0f1b15a7e00f' "$DEVBOX_VERSION" ;;
    *) return 1 ;;
  esac
}

# devbox goes somewhere the agent user already owns, so nothing here needs
# `sudo`. The old code let the Jetify installer `sudo`-place a binary in
# /usr/local/bin; a pinned tarball extracted into $HOME needs no privilege at
# all. semaphore.yml's prologue puts this on PATH for the job shell.
DEVBOX_BIN_DIR="$HOME/.local/bin"

# ---------------------------------------------------------------------------
# Cache layout
# ---------------------------------------------------------------------------

# `CARGO_HOME` moved inside the checkout — see "THE ONE SEMAPHORE CACHE
# BEHAVIOUR" above. `cargo-home` prints the absolute path so semaphore.yml can
# export the identical value into the job shell from ONE source of truth; a
# second hardcoded copy in the YAML would drift into a silently-cold cache.
CARGO_HOME_REL=".cargo-home"
cargo_home_abs() { printf '%s/%s' "$PWD" "$CARGO_HOME_REL"; }

# Where a restored archive is unpacked BEFORE anything is adopted from it. Never
# restore straight over the checkout: `cache store`/`cache restore` will happily
# round-trip an archive whose members are `Taskfile.yml` or `devbox.json`, and a
# restore that lands on the checkout hands the archive's producer code execution
# in this job. See the trust-model comment below for who that producer can be.
#
# What this scratch directory is NOT: a sandbox. `cache restore` writes into it
# before any code here runs, so it constrains what gets ADOPTED, not what the
# extractor may already have written elsewhere. The comment above
# `restore_member` states that limit precisely.
CACHE_SCRATCH_DIR=".semaphore-cache-restore"

# Upper bound on the `target/` directory we are willing to push through the
# cache, in MiB. See the long comment in the cargo-cache section for why a cap
# is not optional: a bloated `target/` restore can be slower than a cold build
# (PRD #376 Open Question 2), and Semaphore documents a per-project cache quota
# (9.6 GB at the time of writing) with LRU eviction that a couple of multi-GB
# archives per platform would thrash.
: "${CARGO_TARGET_CACHE_MAX_MB:=4000}"

log() { printf '\n== %s\n' "$*"; }
die() { printf '\nERROR: %s\n' "$*" >&2; exit 1; }

# Everything below reads repository files by relative path, so the script must
# run from the checkout root. Fail loudly rather than deriving a wrong key.
[ -f devbox.json ] || die "run this from the repository root (devbox.json not found in $PWD)"

# Set unconditionally rather than defaulted, so that the value here and the one
# semaphore.yml exports into the job shell (from the `cargo-home` subcommand)
# cannot drift apart. A mismatch would not fail — it would silently cache a
# directory cargo does not use, which is the class of bug that made the previous
# nix-store cache dead code for its whole life.
export CARGO_HOME
CARGO_HOME="$(cargo_home_abs)"

# ---------------------------------------------------------------------------
# Digest verification
# ---------------------------------------------------------------------------

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "no sha256 tool available (sha256sum/shasum) — cannot verify a download"
  fi
}

verify_sha256() {
  local file="$1" want="$2" got
  got="$(sha256_of "$file")"
  [ "$got" = "$want" ] || die "digest mismatch for $file
  expected $want
  got      $got
This is either a bumped pin whose digest was not updated, or a tampered
artifact. Do not 'fix' it by deleting the check."
}

# ---------------------------------------------------------------------------
# Cache trust model — READ BEFORE CHANGING A KEY
# ---------------------------------------------------------------------------
#
# Cache keys here are derived from repository CONTENT (lockfiles), which means
# they do not authenticate the PRODUCER. A branch in this same repository that
# leaves `devbox.json`, `devbox.lock`, `gcloud/*` and `Cargo.lock` untouched
# derives byte-identical keys to the default branch while running its own
# modified `.semaphore/ci.sh`. Semaphore's cache CLI exposes `store`, `delete`
# and `clear` project-wide, so such a branch could delete the expected key and
# store its own payload under it, and a later default-branch job would restore
# it. That is code execution in a trusted run. (Forked PRs are NOT the vector —
# Semaphore denies forked-PR workflows cache access outright. The actor is
# somebody who can push a branch, or an already-compromised trusted job.)
#
# THREE MITIGATIONS, AND ONE HONEST ADMISSION.
#
# 1. There is exactly ONE cache namespace, `trusted`, and only a push to the
#    default branch ever writes it (`is_trusted_ref`). Branch and pull-request
#    runs restore from it read-only and store nothing. That is stricter than
#    giving untrusted refs their own namespace, and deliberately so: Semaphore's
#    per-project cache quota is shared and LRU-evicted, so per-branch multi-GB
#    `target/` archives would evict the trusted archives every run depends on.
#    A branch that wants a warm cache of its own gets one by merging.
# 2. Restores never land on the checkout. Every restore unpacks into
#    `$CACHE_SCRATCH_DIR` and only an ALLOWLISTED member that passes validation
#    is moved into place; anything else in the archive is logged and deleted.
#    That is a limit on what an archive can get ADOPTED INTO PLACE — it is NOT
#    containment of the extractor, which has already run by then. See the
#    comment above `restore_member`, which states exactly how far it goes.
# 3. No prefix-fallback restores. The previous version restored
#    `cargo-target-v1-<platform>-<toolchain>-` and even `cargo-target-v1-` as
#    fallbacks, which widens the surface from "guess one key" to "store under
#    any key with the right prefix". The cost of removing them is real and
#    should be reported by M5: a `Cargo.lock` bump is now a cold build rather
#    than a rebuild-what-moved. That is a deliberate trade of warm-cache hit
#    rate for a smaller poisoning surface, and it is revisitable once the real
#    boundary below exists.
#
# THE ADMISSION: `is_trusted_ref` is DEFENCE IN DEPTH, NOT AN AUTHORIZATION
# BOUNDARY. The untrusted branch controls this very script, so it controls the
# check. The only real boundary is provider-side — separate Semaphore projects
# for trusted and untrusted refs, or cache namespaces an untrusted ref cannot
# reach. `docs/develop/ci-entrypoints.md` lists that as a required step before
# a Semaphore project is connected; nothing in this repository can enforce it.
CACHE_NS="trusted"

# `SEMAPHORE_GIT_BRANCH` is the TARGET branch on a pull-request run, not the
# head — so it alone would classify every PR against `main` as trusted. The
# ref-type check is what makes this correct.
is_trusted_ref() {
  [ "${SEMAPHORE_GIT_REF_TYPE:-}" = "branch" ] && [ "${SEMAPHORE_GIT_BRANCH:-}" = "main" ]
}

require_trusted_writer() {
  if is_trusted_ref; then
    return 0
  fi
  log "NOT storing caches: this run does not look like a push to the default branch"
  log "    (SEMAPHORE_GIT_REF_TYPE='${SEMAPHORE_GIT_REF_TYPE:-}', SEMAPHORE_GIT_BRANCH='${SEMAPHORE_GIT_BRANCH:-}')."
  log "    Only the trusted ref writes the '$CACHE_NS' namespace — see the trust-model comment in this script."
  return 1
}

# ---------------------------------------------------------------------------
# Cache keys
# ---------------------------------------------------------------------------

# `checksum` is a Semaphore toolbox command (md5 of a file). The fallbacks let
# this script be run by hand on a laptop while debugging it, which is the only
# way anybody can exercise it until a Semaphore project exists.
file_checksum() {
  if command -v checksum >/dev/null 2>&1; then
    checksum "$1"
  elif command -v md5sum >/dev/null 2>&1; then
    md5sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "no checksum tool available (checksum/md5sum/shasum)"
  fi
}

# Keys are readable on purpose — they show up in `cache list` output, and a
# human comparing a Semaphore run against a GHA run needs to see at a glance
# which toolchain and which lockfile a hit came from. Keys must not contain a
# comma: `cache restore` uses commas to separate fallback keys.
short() { printf '%.8s' "$1"; }

platform() { printf '%s-%s' "$(uname -s)" "$(uname -m)"; }

# Fingerprint of the LOCAL FLAKE SOURCE, not just its lockfile. `devbox.json`
# has one non-registry entry, `path:gcloud#google-cloud-sdk`, and BOTH files in
# `gcloud/` decide what it resolves to: `flake.lock` pins the nixpkgs revision,
# and `flake.nix` selects the package and its extra components. Hashing only the
# lock (the previous behaviour) meant editing `flake.nix` alone produced the OLD
# key — and because a store is only written when the key is absent, the stale
# archive could never be replaced and warm-bootstrap numbers would be wrong
# indefinitely. Hashing every file in the directory also survives somebody
# adding a third file there.
local_flake_fingerprint() {
  local list
  list="$(mktemp)"
  find gcloud -type f -print | LC_ALL=C sort | while IFS= read -r f; do
    printf '%s  %s\n' "$(file_checksum "$f")" "$f"
  done >"$list"
  file_checksum "$list"
  rm -f "$list"
}

# The toolchain fingerprint — what decides which store paths the devbox
# environment resolves to:
#   * devbox.json  — WHICH packages the shell has
#   * devbox.lock  — WHICH store path each pinned package resolves to
#   * gcloud/*     — the one devbox.json entry devbox.lock does NOT pin
# This is also the "toolchain version" half of the cargo cache key, playing the
# role `Swatinem/rust-cache` fills by hashing `rustc -vV`: a Renovate devbox
# bump moves devbox.lock, which moves every key here. Deriving it from the
# lockfiles instead of from `rustc --version` avoids a chicken-and-egg problem —
# the keys are needed before a toolchain exists to interrogate.
toolchain_fingerprint() {
  printf '%s-%s-%s' \
    "$(short "$(file_checksum devbox.json)")" \
    "$(short "$(file_checksum devbox.lock)")" \
    "$(short "$(local_flake_fingerprint)")"
}

# `v2` rather than `v1`: the previous keys named archives whose paths were
# absolute and whose fingerprint omitted `gcloud/flake.nix`. Nothing should ever
# restore one of those, so the version segment moves.
#
# NOT platform-scoped: the registry holds downloaded `.crate` sources and git
# checkouts, which are identical on Linux and macOS, so the two jobs can share
# one entry. Exactly one job WRITES each of these (`build`) — see the
# one-writer-per-key comment further down.
cargo_registry_key() { printf 'cargo-registry-v2-%s-%s-%s' "$CACHE_NS" "$(toolchain_fingerprint)" "$(short "$(file_checksum Cargo.lock)")"; }
cargo_git_key() { printf 'cargo-git-v2-%s-%s-%s' "$CACHE_NS" "$(toolchain_fingerprint)" "$(short "$(file_checksum Cargo.lock)")"; }

# Platform-scoped: compiled artifacts obviously are not portable.
cargo_target_key() { printf 'cargo-target-v2-%s-%s-%s-%s' "$CACHE_NS" "$(platform)" "$(toolchain_fingerprint)" "$(short "$(file_checksum Cargo.lock)")"; }

# ---------------------------------------------------------------------------
# bootstrap — nix, devbox, and the devbox environment
# ---------------------------------------------------------------------------

install_nix() {
  if [ -x "$NIX_BIN/nix" ]; then
    log "nix already installed at $NIX_BIN"
    return
  fi

  # The Determinate Systems installer rather than `nixos.org/nix/install`, for
  # two reasons, both about macOS:
  #   * macOS has had a read-only root filesystem since Catalina, so /nix
  #     cannot simply be `mkdir`ed — it has to be a separate APFS volume. This
  #     installer creates and mounts it.
  #   * it is unattended by design (`--no-confirm`). The upstream multi-user
  #     script prompts before the volume surgery, which no CI agent can answer.
  # Both platforms therefore get the SAME code path and a MULTI-USER (daemon)
  # install: a root-owned /nix/store, reached through the daemon.
  #
  # The installer binary is fetched from the pinned release tag and digest-
  # checked before it runs — see the pinning comment at the top. It is invoked
  # WITHOUT sudo, exactly as Determinate's own `nix-installer.sh` shim invokes
  # it (the binary escalates itself and says so); installing nix is inherently
  # privileged, which is different from the cache path, where there is no
  # privilege at all.
  local spec asset want dir
  spec="$(nix_installer_asset)" || die "no pinned nix-installer for $(platform) — add its asset name and digest rather than falling back to an unverified download"
  asset="${spec%% *}"
  want="${spec##* }"

  dir="$(mktemp -d)"
  log "installing nix (Determinate Systems nix-installer $NIX_INSTALLER_VERSION, $asset)"
  curl --proto '=https' --tlsv1.2 -sSf -L \
    -o "$dir/nix-installer" \
    "https://github.com/DeterminateSystems/nix-installer/releases/download/${NIX_INSTALLER_VERSION}/${asset}"
  verify_sha256 "$dir/nix-installer" "$want"
  chmod +x "$dir/nix-installer"
  "$dir/nix-installer" install --no-confirm
  rm -rf "$dir"
}

# Put nix on PATH for the rest of THIS script. The job shell gets the same
# treatment separately, in the pipeline's prologue — see semaphore.yml.
#
# `nix-daemon.sh` is what points a non-root client at the daemon (NIX_REMOTE)
# and at the CA bundle. Guarded with `if`, not `&&`: under `set -e` a failing
# `[ -f … ] && …` would end the script rather than skip the source.
load_nix_env() {
  if [ -f "$NIX_PROFILE_SCRIPT" ]; then
    # shellcheck disable=SC1090
    . "$NIX_PROFILE_SCRIPT"
  fi
  export PATH="$NIX_BIN:$PATH"
}

# ---------------------------------------------------------------------------
# diagnose-nix-env — WHY THIS STEP EXISTS AND WHEN TO DELETE IT
# ---------------------------------------------------------------------------
#
# THIS IS TEMPORARY SCAFFOLDING, NOT A FEATURE. It exists because this pipeline
# cannot be observed from a development machine: there is no Semaphore access
# here, every attempt costs a real push, and the first runs failed with logs
# nobody could read at the time. So a run has to explain itself rather than buy
# one more round trip.
#
# IT WORKED, ON macOS. Its output on the green `build-macos` run is the evidence
# behind every empirical claim in semaphore.yml's VALIDATION STATUS block: the
# APFS `/nix` volume, the pinned installer version read off the resolved
# `profile.d` store path, `nix-daemon.sh` present where the unguarded `source`
# expects it, and `is_trusted_ref: no` on a branch push. DO NOT DELETE IT YET
# ANYWAY — the Linux blocks have never executed a command, so on that platform it
# has not yet done the job it was written for.
#
# WHAT IT IS AIMED AT. semaphore.yml's prologue sources `$NIX_PROFILE_SCRIPT`
# UNGUARDED, immediately after the bootstrap. If the installer does not leave a
# file at exactly that path, that line kills every job on the platform right
# after the bootstrap step — which was the single most likely reading of "it
# failed and we cannot see why". This step runs IMMEDIATELY BEFORE that line and
# prints what the installer actually left on disk, so the failure arrives with
# its own explanation attached.
#
# WHAT IT DELIBERATELY DOES NOT DO. It does not guard the source line, does not
# fall back to another profile path, and does not repair anything. Guarding
# alone would only move the failure to `devbox run` with nix off PATH — one
# opaque failure traded for another. Diagnosing and repairing are separate jobs
# and this is the first one.
#
# IT MUST NOT FAIL THE JOB. Everything below is pure observation, so errexit is
# turned OFF for the body and the function returns 0 unconditionally. That is
# the opposite of the rule everywhere else in this file — in the cache path a
# swallowed error is how a corrupt archive becomes indistinguishable from a miss
# — and it is correct only because nothing here decides anything.
#
# TRIM IT ONCE THE BOOTSTRAP IS PROVEN ON BOTH PLATFORMS. A permanent forty-line
# dump at the head of every job is noise, and noise is what stops being read.
# macOS has shown where the profile lives and that the daemon is up; Linux has
# not run at all. When it has, cut this to the two or three lines that turned out
# to matter, or delete the subcommand and its prologue line outright.
diag() { printf '   %s\n' "$*"; }

# Indent a command's combined output under its label, so a multi-line probe
# stays visually attached to the question it answers.
diag_out() { sed 's/^/      /'; }

cmd_diagnose_nix_env() {
  # See the comment block above: observation only, so a probe that fails is a
  # result, not an error. `set -u` stays on, hence `${VAR:-}` throughout.
  set +e

  log "nix-env diagnostics (temporary — see the comment above cmd_diagnose_nix_env)"

  diag "uname -a:"
  uname -a 2>&1 | diag_out
  diag "whoami: $(whoami 2>&1)   id: $(id 2>&1)"
  # Distinguished on purpose: "no sudo binary" and "sudo refuses without a
  # password" are different diagnoses, and the installer escalates itself, so
  # either one is fatal to the bootstrap in a different way.
  if ! command -v sudo >/dev/null 2>&1; then
    diag "sudo: NOT INSTALLED — the nix installer escalates itself and needs it"
  elif sudo -n true 2>/dev/null; then
    diag "sudo -n true: OK (passwordless escalation available)"
  else
    diag "sudo -n true: FAILED — no passwordless escalation for $(whoami 2>&1)"
  fi

  # An explicit allowlist, NOT `env | grep SEMAPHORE_`: Semaphore puts cache and
  # registry credentials in that same namespace (SEMAPHORE_CACHE_USERNAME,
  # SEMAPHORE_REGISTRY_PASSWORD, ...), and this output is meant to be pasted
  # into a chat window.
  diag "SEMAPHORE_AGENT_MACHINE_TYPE='${SEMAPHORE_AGENT_MACHINE_TYPE:-}'"
  diag "SEMAPHORE_AGENT_MACHINE_OS_IMAGE='${SEMAPHORE_AGENT_MACHINE_OS_IMAGE:-}'"
  diag "SEMAPHORE_AGENT_MACHINE_ENVIRONMENT_TYPE='${SEMAPHORE_AGENT_MACHINE_ENVIRONMENT_TYPE:-}'"
  diag "SEMAPHORE_GIT_REF_TYPE='${SEMAPHORE_GIT_REF_TYPE:-}'"
  diag "SEMAPHORE_GIT_BRANCH='${SEMAPHORE_GIT_BRANCH:-}'"
  diag "SEMAPHORE_GIT_REF='${SEMAPHORE_GIT_REF:-}'"
  diag "SEMAPHORE_GIT_PR_NUMBER='${SEMAPHORE_GIT_PR_NUMBER:-}'"
  # `is_trusted_ref` was argued from the docs; this line checks it against
  # reality, and says whether this run would have written the cache at all.
  if is_trusted_ref; then
    diag "is_trusted_ref: YES — this run WOULD store into the '$CACHE_NS' namespace"
  else
    diag "is_trusted_ref: no — this run restores read-only and stores nothing"
  fi

  # The devbox closure is 4.4 GiB unpacked on x86_64-linux. Recording the free
  # space removes "the agent ran out of disk" from the suspect list for good.
  diag "df -h (checkout, /, and /nix if it exists):"
  if [ -e /nix ]; then
    # One invocation so the mounts share a header — and on macOS this is where a
    # separate /nix APFS volume shows up as its own line.
    df -h "$PWD" / /nix 2>&1 | diag_out
  else
    df -h "$PWD" / 2>&1 | diag_out
  fi

  if [ -e /nix ]; then
    diag "/nix exists:"
    ls -ld /nix 2>&1 | diag_out
    # Same substring on both platforms: Linux prints
    # `/dev/sda1 on /nix type ext4 (...)`, macOS `/dev/disk3s7 on /nix (apfs, ...)`.
    # The macOS line is also how the separate APFS volume announces itself.
    local mnt
    mnt="$(mount 2>/dev/null | grep ' on /nix ')"
    if [ -n "$mnt" ]; then
      diag "/nix mount entry:"
      printf '%s\n' "$mnt" | diag_out
    else
      diag "/nix is not a mount point (no 'on /nix' entry in mount output)"
    fi
  else
    diag "/nix DOES NOT EXIST"
  fi

  # The highest-value lines in this whole step: what the installer actually left
  # in the profile.d directory, rather than a yes/no on one guessed filename.
  # Both spellings on purpose — `ls -ld <dir>` names the directory and resolves a
  # symlink to the store, `ls -la <dir>/` (trailing slash) lists what is IN it.
  #
  # Ancestor walk done with `${var%/*}` rather than `dirname`: it terminates by
  # construction and needs no external command, so a stripped PATH cannot turn a
  # diagnostic into a hang.
  local prof_dir probe
  prof_dir="${NIX_PROFILE_SCRIPT%/*}"
  if [ -d "$prof_dir" ]; then
    diag "$prof_dir:"
    ls -ld "$prof_dir" 2>&1 | diag_out
    diag "$prof_dir/ contents:"
    ls -la "$prof_dir/" 2>&1 | diag_out
  else
    diag "$prof_dir DOES NOT EXIST"
    probe="$prof_dir"
    while [ -n "$probe" ] && [ ! -e "$probe" ]; do
      probe="${probe%/*}"
    done
    [ -n "$probe" ] || probe="/"
    diag "deepest existing ancestor: $probe"
    ls -ld "$probe" 2>&1 | diag_out
    if [ -d "$probe" ]; then
      ls -la "$probe/" 2>&1 | diag_out
    fi
  fi

  # The exact path semaphore.yml sources unguarded.
  if [ -f "$NIX_PROFILE_SCRIPT" ]; then
    diag "$NIX_PROFILE_SCRIPT: present"
    ls -l "$NIX_PROFILE_SCRIPT" 2>&1 | diag_out
  else
    diag "$NIX_PROFILE_SCRIPT: ABSENT — the prologue's unguarded 'source' of this"
    diag "   path is the next thing to run, and it will kill this job."
  fi

  local nix_path
  nix_path="$(command -v nix 2>/dev/null)"
  if [ -n "$nix_path" ]; then
    diag "nix on PATH: $nix_path ($(nix --version 2>&1 | head -n 1))"
  else
    diag "nix: NOT on PATH"
  fi
  if [ -x "$NIX_BIN/nix" ]; then
    diag "$NIX_BIN/nix: present ($("$NIX_BIN/nix" --version 2>&1 | head -n 1))"
  else
    diag "$NIX_BIN/nix: absent"
  fi
  diag "PATH=$PATH"
  diag "NIX_REMOTE='${NIX_REMOTE:-}'"

  local sock="/nix/var/nix/daemon-socket/socket"
  if [ -S "$sock" ]; then
    diag "daemon socket: present ($sock)"
  elif [ -e "$sock" ]; then
    diag "daemon socket: $sock exists but is NOT a socket"
  else
    diag "daemon socket: absent ($sock)"
  fi
  # Whichever service manager this platform has; neither being present is a
  # legitimate answer, not a failure.
  if command -v systemctl >/dev/null 2>&1; then
    diag "systemctl is-active nix-daemon.socket:  $(systemctl is-active nix-daemon.socket 2>&1)"
    diag "systemctl is-active nix-daemon.service: $(systemctl is-active nix-daemon.service 2>&1)"
  elif command -v launchctl >/dev/null 2>&1; then
    diag "launchctl print system/org.nixos.nix-daemon:"
    launchctl print system/org.nixos.nix-daemon 2>&1 | head -n 3 | diag_out
  else
    diag "no systemctl and no launchctl — service status not probed"
  fi

  # Determinate's installer does not write a log file; what it does leave is an
  # install receipt, and macOS additionally gets the daemon's launchd stderr.
  # These are CANDIDATES that are reported if present, not paths assumed to exist.
  local artefact found=0
  for artefact in /nix/receipt.json /nix/nix-installer /var/log/nix-daemon.log; do
    if [ -e "$artefact" ]; then
      found=1
      diag "installer artefact:"
      ls -l "$artefact" 2>&1 | diag_out
    fi
  done
  if [ "$found" -eq 0 ]; then
    diag "no installer artefact at any of: /nix/receipt.json /nix/nix-installer /var/log/nix-daemon.log"
  fi

  log "end nix-env diagnostics"

  set -e
  return 0
}

# ---------------------------------------------------------------------------
# THERE IS DELIBERATELY NO NIX-STORE CACHE. READ THIS BEFORE ADDING ONE.
# ---------------------------------------------------------------------------
#
# The first version of this script cached `/nix/store` as a `sudo tar -C / -cf`
# archive plus a `nix-store --dump-db` dump, restored with `sudo tar -C / -xpf -`
# and `sudo nix-store --load-db`. That was removed, for two independent reasons.
#
# 1. IT WAS UNSAFE. The archive came from the project cache with no digest, no
#    signature, no producer identity and no member allowlist, and was extracted
#    at `/` as root with `-p`. An ordinary RELATIVE member such as
#    `etc/ld.so.preload` therefore lands at `/etc/ld.so.preload` — tar's refusal
#    of absolute and `..` members buys nothing when the extraction root is `/` —
#    and `-p` as root preserves modes and ownership, so the archive can install
#    root-owned or setuid content. `--load-db` then registers paths as valid
#    from the dump's own claimed hashes, without recomputing content hashes or
#    checking binary-cache signatures. Net effect: whoever could write that
#    cache key got root code execution in a later trusted job.
# 2. IT WAS ALSO BUILT ON A MECHANISM NOBODY HAD CHECKED. It stored an absolute
#    `$HOME/.cache/...` path, and this comment used to assert that `cache store`
#    strips the leading `/`, so the restore could never hit and every job
#    cold-provisioned. That assertion is WITHDRAWN: the cache CLI archives an
#    absolute path with `tar czPf` and restores it with `tar xzPf`, which
#    preserves the leading `/` (see the corrected header comment). Whether the
#    deleted code hit or missed was therefore never established either way, and
#    it is moot — the code is gone. What is NOT moot is that the same `-P`
#    behaviour makes reason (1) worse, not better: absolute members extracted as
#    root are the escape, not a hypothetical.
#
# WHY IT WAS NOT SIMPLY REPLACED WITH A SIGNED `nix copy`. The safe shape is
# real and is written down here so the next person does not reinvent the tar:
# save with `nix copy --to "file://<checkout-relative-dir>" <paths>`, restore
# with `nix copy --from "file://<dir>" --all`, signature verification left ON,
# no sudo anywhere, and the directory kept checkout-relative so it round-trips.
#
# DO NOT DISABLE SIGNATURE CHECKING. nix has a flag that turns it off and it
# must not appear in this file — that flag is the whole difference between the
# safe shape and the one that was just deleted. If a path refuses to import
# because it is unsigned, that is the mechanism working, not a bug: locally-built
# paths (the gcloud `withExtraComponents` composition, devbox's own profile)
# carry no signature and simply should not travel through a cache.
#
# It was not done now because for THIS workload it plausibly costs more than it
# saves, and nothing has measured it:
#
#   * The paths worth caching are exactly the ones `cache.nixos.org` already
#     serves, signed, from a CDN. Substituting them directly does one download +
#     NAR decompress + hash + register. Going through the project cache does
#     export-to-file-store (a second full copy of the closure) + tar + upload on
#     the save side, then download + untar + NAR re-import on the restore side.
#     The saving is CDN-vs-Semaphore transfer speed, against two extra full
#     copies of a multi-GiB closure on a 4-vCPU agent.
#   * Size against quota: the full `devbox.json` profile closure measures
#     4.4 GiB unpacked / 442 paths on x86_64-linux, and the darwin closure is
#     ~1.8 GiB. Semaphore's documented per-project quota is 9.6 GB with LRU
#     eviction, shared with the `target/` archives — and `target/` is the cache
#     with NO upstream CDN alternative. Spending the quota on paths a CDN
#     already serves would evict the ones it does not.
#   * A signed round-trip additionally needs the saver to copy only paths that
#     CARRY a signature (locally-built paths — the `withExtraComponents` gcloud
#     composition, devbox's own profile — have none, and `nix copy --from`
#     rightly refuses them). Identifying those portably across two OSes and a
#     drifting `nix path-info --json` shape is a third never-executed mechanism
#     in a file where nothing has ever run.
#
# So: every job substitutes from `cache.nixos.org`, which is signed and verified
# by nix itself, and COLD BOOTSTRAP IS THE MEASURED BASELINE for M5. Store
# caching is deferred to a follow-up that can be argued from those numbers. If
# you add it back, add the `nix copy` shape above — not a privileged tar.

install_devbox() {
  local spec asset want dir
  spec="$(devbox_asset)" || die "no pinned devbox release for $(platform) — add its asset name and digest rather than falling back to an unverified installer"
  asset="${spec%% *}"
  want="${spec##* }"

  export PATH="$DEVBOX_BIN_DIR:$PATH"

  # Version-checked, not just presence-checked: a pre-installed devbox of some
  # other version would otherwise satisfy `command -v` and silently un-pin the
  # thing this PRD exists to pin.
  if [ -x "$DEVBOX_BIN_DIR/devbox" ] && [ "$("$DEVBOX_BIN_DIR/devbox" version 2>/dev/null | head -1)" = "$DEVBOX_VERSION" ]; then
    log "devbox $DEVBOX_VERSION already installed at $DEVBOX_BIN_DIR"
    return
  fi

  # Straight from the pinned GitHub release tarball, digest-checked, extracted
  # into a directory the agent user owns. No `curl | bash`, and no `sudo`.
  dir="$(mktemp -d)"
  log "installing devbox $DEVBOX_VERSION ($asset)"
  curl --proto '=https' --tlsv1.2 -sSf -L \
    -o "$dir/$asset" \
    "https://github.com/jetify-com/devbox/releases/download/${DEVBOX_VERSION}/${asset}"
  verify_sha256 "$dir/$asset" "$want"
  tar -xzf "$dir/$asset" -C "$dir" devbox
  mkdir -p "$DEVBOX_BIN_DIR"
  install -m 0755 "$dir/devbox" "$DEVBOX_BIN_DIR/devbox"
  rm -rf "$dir"
  log "devbox installed: $("$DEVBOX_BIN_DIR/devbox" version)"
}

# First `devbox run` is what actually realises the environment — nix substitutes
# the whole profile closure from `cache.nixos.org`. Doing it here, in the
# prologue, rather than implicitly inside the first build step, is what makes
# the bootstrap a separately-timed line in the Semaphore UI. M5 needs that
# number, and with no store cache it is the ONLY bootstrap number this spike
# produces — a cold one, every job, on purpose.
#
# The version echoes are the evidence a GHA-vs-Semaphore diff analysis needs.
# ONE OF THEM LOOKS WRONG AND IS NOT: under the `cargo@1.97.1` devbox pin,
# `rustc --version` reports 1.97.1 while `cargo --version` reports
# `cargo 1.97.0 (c980f4866 2026-06-30)`. That is nixpkgs' 1.97.1 derivation
# reporting a 1.97.0 internal version, not a mis-resolved pin. Do not read it
# as "the pin failed".
provision_devbox_env() {
  log "provisioning the devbox environment (this is the bootstrap cost M5 measures)"
  devbox run -- rustc --version
  devbox run -- cargo --version
  devbox run -- cargo clippy --version
  devbox run -- rustfmt --version
  devbox run -- cargo nextest --version
  devbox run -- cargo audit --version
  devbox run -- task --version
}

cmd_bootstrap() {
  install_nix
  load_nix_env
  install_devbox
  provision_devbox_env
}

# ---------------------------------------------------------------------------
# restore-cargo-cache / save-cargo-cache — the `Swatinem/rust-cache` stand-in
# ---------------------------------------------------------------------------

# WHAT IS AND IS NOT REIMPLEMENTED FROM `Swatinem/rust-cache@v2`
# (PRD #376 Open Question 2, answered honestly rather than optimistically)
#
# Reimplemented here:
#   * key derivation from Cargo.lock plus the toolchain identity — the
#     toolchain half comes from the devbox lockfiles rather than from
#     `rustc -vV`, see `toolchain_fingerprint`
#   * the three cached locations: the cargo registry, the cargo git db, `target/`
#   * incremental compilation disabled (`CARGO_INCREMENTAL=0` in semaphore.yml)
#     and any incremental directory deleted before storing — rust-cache
#     disables incremental by default for exactly this reason
#   * dropping `registry/src` (cargo re-extracts it from the `.crate` files in
#     `registry/cache`) and the `registry/index/*/.cache` blobs
#   * writing once per key, and only for a job that passed
#
# NOT reimplemented, and this is the part that matters:
#   * pruning `target/` down to artifacts the CURRENT dependency graph actually
#     references. rust-cache walks the metadata and deletes `deps/` entries for
#     versions that no longer exist.
#   * per-package pruning of build-script output directories
#   * cache-key invalidation on rustc/cargo environment changes beyond the
#     lockfiles
#   * rust-cache's prefix-fallback restore. That one was implemented and has
#     been REMOVED on purpose — see mitigation 3 in the trust-model comment.
#     The consequence is that a `Cargo.lock` bump is a cold build, and M5 should
#     report the hit rate so the trade can be argued from numbers.
#
# The measured stakes: a long-lived local `target/` in this repo is 8.5 GiB
# (8.0 GiB debug, of which 1.4 GiB is incremental; 528 MiB release). A CI
# `target/` built by exactly `cargo build --release` plus `cargo nextest run`
# is smaller than that — but nobody has measured how much smaller, because
# nobody can run this pipeline yet. So there is a hard cap below, and when it
# trips it says so loudly instead of silently degrading: a restore that takes
# longer than the build it saves is the failure mode the PRD calls out, and it
# is invisible unless something prints it.
#
# ONE WRITER PER KEY. `has_key` is not atomic, so two blocks that both miss and
# both store the same key duplicate GiB of work, contradict "one write per key",
# and confound M5. Ownership is therefore explicit and asymmetric:
#   * `build` (Linux)  — `save-cargo-cache`: registry + git db + its `target/`.
#     Sole writer of the two non-platform keys.
#   * `build-macos`    — `save-cargo-target-cache`: its own platform `target/`
#     ONLY. Restores the registry/git keys, never writes them.
#   * `security`       — no cargo cache at all. `cargo audit` never compiles the
#     workspace, so `target/` stays empty and sharing the key would let it store
#     an empty archive under a key `build` needs.

# ---------------------------------------------------------------------------
# Adopting a restored member — WHAT THIS DOES AND DOES NOT PROTECT AGAINST
# ---------------------------------------------------------------------------
#
# A restore is untrusted input (trust-model comment above). `restore_member`
# unpacks one key into a scratch directory, VALIDATES what came out, and moves
# exactly one expected member into place; everything else is logged and deleted,
# and every failure is a cache miss rather than a partial adoption.
#
# WHAT THAT IS WORTH, stated precisely — an earlier version of this comment
# claimed more than the code can deliver:
#
#   * It limits what an archive can get ADOPTED INTO PLACE. One allowlisted
#     member, only if it is a real directory holding nothing but real files and
#     real directories, reached through no symlinked ancestor on either side of
#     the rename, with no file in it linked from outside it.
#   * It DOES NOT CONFINE THE EXTRACTOR, and that is now CONFIRMED rather than
#     suspected. `cache restore` writes before any of this code sees the scratch
#     directory, and the cache CLI decompresses with
#     `tar xzPf <tmp> -C .` — WITH `-P` — whenever the archive's FIRST MEMBER is
#     an absolute path (`cache-cli/pkg/archive/shell_out_archiver.go`, read
#     2026-08-04). `-P` is precisely the flag that tells tar to honour absolute
#     and `..` members instead of sanitising them, and the producer of the
#     archive chooses that first member. So a crafted archive CAN write outside
#     the scratch directory — outside the checkout entirely — before any check
#     below runs, and a scratch-only cleanup can neither detect nor undo it.
#     (An archive whose first member is relative gets the no-`-P` branch, where
#     tar does sanitise. Ours are all relative; an attacker's need not be.)
#
# So this is one layer of defence in depth against a hazard that is real, not
# hypothetical. It limits ADOPTION and cannot limit EXTRACTION. Sandboxing the
# restore so that only the scratch directory is writable — or keeping untrusted
# refs out of this cache namespace provider-side — is a prerequisite before this
# pipeline is trusted with a cache shared across trust levels. It is an explicit
# item on the activation checklist in `docs/develop/ci-entrypoints.md`.
#
# The scratch directory sits inside the checkout so that adopting a member is a
# rename rather than a multi-GiB copy across filesystems.

# Prints the offending component and returns 0 if any component of a
# checkout-relative path is a symlink.
#
# `-e`/`-d` FOLLOW symlinks, so neither can see a symlinked ANCESTOR: make
# `.semaphore-cache-restore/.cargo-home` a link to any directory on the agent
# and `[ -e .semaphore-cache-restore/.cargo-home/registry ]` is true for a
# directory that is not in the scratch tree at all — which the `mv` would then
# move out of that external location. `-L` per component is the only test that
# sees it. Used on BOTH sides of the rename: a symlinked destination ancestor is
# the same escape in reverse.
path_has_symlink_component() {
  local rest="$1" prefix="" comp
  while [ -n "$rest" ]; do
    comp="${rest%%/*}"
    if [ "$comp" = "$rest" ]; then rest=""; else rest="${rest#*/}"; fi
    if [ -z "$comp" ]; then continue; fi
    prefix="${prefix:+$prefix/}$comp"
    if [ -L "$prefix" ]; then
      printf '%s' "$prefix"
      return 0
    fi
  done
  return 1
}

# Everything in a tree that is not a plain file or a plain directory. Empty
# output means clean.
tree_nonplain_entries() {
  find "$1" \( -type l -o -type b -o -type c -o -type p -o -type s \) -print
}

# Inodes inside a tree whose link count exceeds the number of links to them
# found IN that tree — i.e. regular files that are also linked from outside it.
#
# A blunt `-links +1` rejection is not usable: cargo hardlinks its uplifted
# binaries (`target/debug/foo` <-> `target/debug/deps/foo-<hash>`), which is
# 9,011 of the 33,896 files in a local `target/`, so it would reject every
# genuine archive. Counting links per inode separates "linked to its own
# sibling" from "linked to something on the agent". `find -links` and `ls -ldi`
# are POSIX, unlike GNU `find -printf '%i %n'`, so this works on the macOS agent
# too. Fields of `ls -ldi`: inode, mode, link count.
tree_external_hardlinks() {
  find "$1" -type f -links +1 -exec ls -ldi {} + |
    awk '$1 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ { seen[$1]++; nlink[$1] = $3 + 0 }
         END { for (i in seen) if (seen[i] < nlink[i]) print i }'
}

# Move ONE expected member out of a restored archive into the checkout, or
# refuse and leave the checkout untouched. Returns 0 only if the member was
# adopted. Every refusal is a cache miss: the job cold-builds, which is slow and
# correct. Nothing here ever repairs a suspicious tree.
adopt_restored_member() {
  local scratch="$1" member="$2"
  local src="$scratch/$member" dest_dir offender

  # The member is a constant from `cmd_restore_cargo_cache`, never archive
  # input — assert that anyway, so a future edit cannot turn the walks below
  # into nonsense by passing an absolute or `..`-bearing path.
  case "$member" in
    /* | */../* | ../* | */..) die "internal error: member '$member' must be a relative path without '..'" ;;
  esac

  # `-L` before `-e`, because `-e` is false for a dangling symlink and true for
  # whatever a live one points at; both are cases we want to see, not miss.
  if [ ! -L "$src" ] && [ ! -e "$src" ]; then
    log "  MISS: '$member' not present in the restored archive — this run builds it cold"
    return 1
  fi

  if offender="$(path_has_symlink_component "$src")"; then
    log "  REJECTED '$member': '$offender' is a symlink, so this member is not inside $scratch"
    return 1
  fi

  if [ ! -d "$src" ]; then
    log "  REJECTED '$member': the restored member is not a real directory"
    return 1
  fi

  # RULE: reject ALL symlinks and special files in the adopted tree, not only
  # the ones that resolve outside it. Two reasons. (a) "Resolves outside" has to
  # be computed, and a chain of links can defeat a lexical computation while a
  # real resolution is a second filesystem round trip to get wrong; the blunt
  # rule cannot be argued into being wrong. (b) It costs nothing measurable
  # here: a full local `target/` (33,896 files) and a populated
  # `~/.cargo/{registry,git}` contain zero symlinks — measured 2026-08-04. If a
  # dependency ever legitimately puts one in `target/`, this fails CLOSED (cold
  # build, loudly logged) and the deliberate fix is to relax the rule to
  # "reject links whose resolved target escapes the member", not to delete it.
  if ! offender="$(tree_nonplain_entries "$src")"; then
    log "  REJECTED '$member': could not scan the restored tree"
    return 1
  fi
  if [ -n "$offender" ]; then
    log "  REJECTED '$member': restored tree contains symlinks or special files:"
    printf '%s\n' "$offender" | sed -n '1,20s|^|    |p'
    return 1
  fi

  # A hardlink into an existing file on the agent would let a later write
  # through the adopted tree edit that file in place.
  if ! offender="$(tree_external_hardlinks "$src")"; then
    log "  REJECTED '$member': could not check hardlinks in the restored tree"
    return 1
  fi
  if [ -n "$offender" ]; then
    log "  REJECTED '$member': restored tree hardlinks file(s) outside it (inodes: $(printf '%s' "$offender" | tr '\n' ' '))"
    return 1
  fi

  # Destination side. `mkdir -p` first — `.cargo-home` does not exist on a cold
  # agent — then check, because `mkdir -p` is satisfied by an existing symlink.
  dest_dir="$(dirname "$member")"
  mkdir -p "$dest_dir"
  if offender="$(path_has_symlink_component "$dest_dir")"; then
    log "  REJECTED '$member': destination ancestor '$offender' is a symlink"
    return 1
  fi

  # `rm -rf` on a symlinked destination removes the link, not its target.
  rm -rf "$member"
  mv "$src" "$member"
  return 0
}

restore_member() {
  local key="$1" member="$2" scratch="$CACHE_SCRATCH_DIR"

  rm -rf "$scratch"
  mkdir -p "$scratch"

  # A restore that did not exit 0 is a MISS, never a partial hit.
  #
  # THE EXIT-CODE CONTRACT, now taken from the docs rather than guessed at:
  # a cache MISS is not an error — "`cache restore` … If no archives are restored
  # the command exits with exit status 0" — and `has_key` is the one that "exits
  # with non-zero status if the key is not found". So a miss arrives here as 0
  # with an empty scratch directory, and it is `adopt_restored_member` that
  # reports it (the member simply is not there). This branch is therefore NOT the
  # miss path.
  #
  # What it catches is a genuine cache ERROR, e.g. a failed connection or a
  # corrupt archive that tar bailed out of half-way. By default those ALSO exit 0,
  # which would leave a half-written member to be adopted and hand the job a
  # `target/` that looks warm and is not — so `CACHE_FAIL_ON_ERROR=true` is set
  # for this one invocation, which is what makes an error distinguishable from a
  # miss at all.
  #
  # It is scoped to this subshell ON PURPOSE and must not be exported globally:
  # with it set, a `cache store` that hits a transient cache-server error also
  # exits nonzero, and under `set -e` that would fail the epilogue and turn a
  # green build red over a cache hiccup. Here the nonzero is caught and demoted to
  # a miss, so the worst case is a cold build.
  if ! (cd "$scratch" && CACHE_FAIL_ON_ERROR=true cache restore "$key"); then
    log "  MISS: 'cache restore' did not succeed for $key — nothing adopted, this run builds it cold"
    rm -rf "$scratch"
    return 0
  fi

  if adopt_restored_member "$scratch" "$member"; then
    log "  restored '$member' from $key"
    # Drop the now-empty ancestor directories of the member just adopted, so
    # the leftover report below names only genuinely unexpected members. Only
    # empty directories are removed, so this cannot reach the checkout.
    rmdir -p "$(dirname "$scratch/$member")" 2>/dev/null || true
  fi

  # Whatever is left was not asked for, or was refused. Say so; do not trust it.
  if [ -n "$(ls -A "$scratch" 2>/dev/null)" ]; then
    log "  discarding unexpected or rejected members restored under $key:"
    find "$scratch" -mindepth 1 -maxdepth 2 -print | sed 's/^/    /'
  fi
  rm -rf "$scratch"
}

cmd_restore_cargo_cache() {
  log "restoring cargo caches (exact keys only — no prefix fallbacks, see the trust-model comment)"
  restore_member "$(cargo_registry_key)" "$CARGO_HOME_REL/registry"
  restore_member "$(cargo_git_key)" "$CARGO_HOME_REL/git"
  restore_member "$(cargo_target_key)" "target"
}

prune_before_store() {
  log "pruning before storing (partial rust-cache emulation — see comment above)"
  rm -rf target/debug/incremental target/release/incremental
  rm -rf "$CARGO_HOME_REL/registry/src"
  if [ -d "$CARGO_HOME_REL/registry/index" ]; then
    find "$CARGO_HOME_REL/registry/index" -name .cache -type d -prune -exec rm -rf {} +
  fi
}

store_target() {
  local tgt_key size_mb
  tgt_key="$(cargo_target_key)"

  if [ ! -d target ]; then
    log "no target/ to store"
    return
  fi

  size_mb="$(du -sm target | cut -f1)"
  if [ "$size_mb" -gt "$CARGO_TARGET_CACHE_MAX_MB" ]; then
    log "NOT storing target/: ${size_mb} MiB exceeds the ${CARGO_TARGET_CACHE_MAX_MB} MiB budget."
    log "    This is the un-reimplemented half of rust-cache showing up. Either add"
    log "    real pruning or raise CARGO_TARGET_CACHE_MAX_MB deliberately — but read"
    log "    PRD #376 Open Question 2 first, because a restore this large can cost"
    log "    more wall clock than the build it replaces."
    return
  fi

  log "storing target/ (${size_mb} MiB) under $tgt_key"
  cache has_key "$tgt_key" || cache store "$tgt_key" target
}

# Sole writer of the non-platform registry/git keys, plus this platform's
# `target/`. Invoked only from the Linux `build` block.
cmd_save_cargo_cache() {
  require_trusted_writer || return 0
  prune_before_store

  local reg_key git_key
  reg_key="$(cargo_registry_key)"
  git_key="$(cargo_git_key)"

  if [ -d "$CARGO_HOME_REL/registry" ]; then
    cache has_key "$reg_key" || cache store "$reg_key" "$CARGO_HOME_REL/registry"
  fi
  if [ -d "$CARGO_HOME_REL/git" ]; then
    cache has_key "$git_key" || cache store "$git_key" "$CARGO_HOME_REL/git"
  fi

  store_target
}

# Platform `target/` only. Invoked from `build-macos`, which shares the
# registry/git keys with `build` and must not race it for them.
cmd_save_cargo_target_cache() {
  require_trusted_writer || return 0
  prune_before_store
  store_target
}

# ---------------------------------------------------------------------------

case "${1:-}" in
  cargo-home) cargo_home_abs ;;
  bootstrap) cmd_bootstrap ;;
  # Temporary. See the comment above `cmd_diagnose_nix_env`, including when to
  # delete this arm again.
  diagnose-nix-env) cmd_diagnose_nix_env ;;
  restore-cargo-cache) cmd_restore_cargo_cache ;;
  save-cargo-cache) cmd_save_cargo_cache ;;
  save-cargo-target-cache) cmd_save_cargo_target_cache ;;
  *)
    echo "usage: $0 <cargo-home|bootstrap|diagnose-nix-env|restore-cargo-cache|save-cargo-cache|save-cargo-target-cache>" >&2
    exit 2
    ;;
esac
