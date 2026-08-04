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
# `cache store <key> <path>` STRIPS A LEADING `/` from the path, and
# `cache restore <key>` extracts RELATIVE TO THE WORKING DIRECTORY. So an
# absolute path does not round-trip: store `/home/semaphore/.cargo/registry`
# and you get a checkout-relative `home/semaphore/.cargo/registry` back, which
# no later `has_key`/path check will ever match. The first version of this
# script stored the nix store under an absolute `$HOME/...` path and tested for
# the absolute path on restore, so the restore could never hit and every job
# cold-provisioned — a cache that had never been capable of working.
#
# Consequence, and please do not "helpfully" undo it: EVERY path this script
# hands to `cache store` is CHECKOUT-RELATIVE. That is also why `CARGO_HOME` is
# moved inside the checkout (`cargo-home` subcommand) instead of being left at
# `$HOME/.cargo`.
#
# UNVALIDATED
#
# No Semaphore project is connected to this repository, so NOTHING in this file
# has ever run on a Semaphore agent. It is written against Semaphore's
# documented toolbox CLI, deliberately preferring boring constructs over clever
# ones. Expect the first real run to need fixes; the comments flag each step
# whose behaviour is assumed rather than observed.
# =============================================================================

set -euo pipefail

# Where the nix binaries land in a multi-user (daemon) install, on both Linux
# and macOS.
NIX_BIN="/nix/var/nix/profiles/default/bin"

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
#     2026-08-04 against the pinned release assets. That defends against a
#     pinned artifact being mutated later, which is the realistic risk; it does
#     not defend against the artifact having been wrong when it was recorded.
#     If Determinate ever starts publishing checksums or attestations, switch to
#     verifying those instead of trusting this line.
#
# Neither pin is Renovate-managed: `renovate.json` has no custom manager for
# shell-script pins, so bumping these is a manual step. A `customManagers`
# regex entry would fix that and is a follow-up, not something to bolt on to an
# unproven pipeline.
NIX_INSTALLER_VERSION="v3.21.9"
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
#    `$CACHE_SCRATCH_DIR` and only an ALLOWLISTED member is moved into place;
#    anything else in the archive is logged and deleted.
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
  local profile="/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh"
  if [ -f "$profile" ]; then
    # shellcheck disable=SC1090
    . "$profile"
  fi
  export PATH="$NIX_BIN:$PATH"
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
# 2. IT HAD NEVER WORKED, which is why nobody noticed (1). It stored an absolute
#    `$HOME/.cache/...` path, and `cache store` strips the leading `/`, so the
#    restore produced a checkout-relative `home/...` tree while the code tested
#    for the absolute path. Every job cold-provisioned. Any "warm bootstrap"
#    number this had produced would have been fiction.
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

# Unpack one cache key into a scratch directory, then move ONLY the member we
# expect into place. Anything else the archive contained is reported and
# deleted: a restore is untrusted input (trust-model comment above), and the one
# thing this can enforce locally is that it cannot write outside the paths this
# script owns.
#
# Two limits, stated rather than implied. The scratch directory sits inside the
# checkout so that adopting a member is a rename rather than a multi-GiB copy
# across filesystems; that contains ordinary relative members, but it does not
# defend against a cache CLI that honours `..` members — which is a property of
# Semaphore's implementation that nobody here can verify, and one more reason
# the real boundary is provider-side isolation.
restore_member() {
  local key="$1" member="$2" scratch="$CACHE_SCRATCH_DIR"

  rm -rf "$scratch"
  mkdir -p "$scratch"
  # `|| true`: a miss is normal and must not fail the job.
  (cd "$scratch" && cache restore "$key") || true

  if [ -e "$scratch/$member" ]; then
    rm -rf "$member"
    mkdir -p "$(dirname "$member")"
    mv "$scratch/$member" "$member"
    log "  restored '$member' from $key"
    # Drop the now-empty ancestor directories of the member just adopted, so
    # the leftover report below names only genuinely unexpected members. Only
    # empty directories are removed, so this cannot reach the checkout.
    rmdir -p "$(dirname "$scratch/$member")" 2>/dev/null || true
  else
    log "  MISS: '$member' not present for $key — this run builds it cold"
  fi

  # Whatever is left was not asked for. Say so; do not trust it.
  if [ -n "$(ls -A "$scratch" 2>/dev/null)" ]; then
    log "  discarding unexpected members restored under $key:"
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
  restore-cargo-cache) cmd_restore_cargo_cache ;;
  save-cargo-cache) cmd_save_cargo_cache ;;
  save-cargo-target-cache) cmd_save_cargo_target_cache ;;
  *)
    echo "usage: $0 <cargo-home|bootstrap|restore-cargo-cache|save-cargo-cache|save-cargo-target-cache>" >&2
    exit 2
    ;;
esac
