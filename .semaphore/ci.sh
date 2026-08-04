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
# are Semaphore toolbox commands with no GHA equivalent, and the /nix handling
# is coupled to how nix installs on a CI agent. The *build* steps, by contrast,
# are provider-agnostic: they are `task ci-*` entrypoints from `Taskfile.yml`,
# identical to what a contributor runs locally.
#
# UNVALIDATED
#
# No Semaphore project is connected to this repository, so NOTHING in this file
# has ever run on a Semaphore agent. It is written against Semaphore's
# documented toolbox CLI and against nix's documented store-transfer commands
# (`nix-store --dump-db` / `--load-db`), deliberately preferring boring
# constructs over clever ones. Expect the first real run to need fixes; the
# comments flag each step whose behaviour is assumed rather than observed.
# =============================================================================

set -euo pipefail

# Where the nix binaries land in a multi-user (daemon) install, on both Linux
# and macOS. Referenced by absolute path because `sudo` resets PATH.
NIX_BIN="/nix/var/nix/profiles/default/bin"

# Scratch dir holding the nix-store archive + db dump that Semaphore's cache
# stores and restores as a unit. An absolute path, on the assumption that
# `cache store` / `cache restore` round-trip one; if the first real run shows
# they only handle paths relative to the checkout, point this at a directory
# inside the working copy instead — that is the whole change, and it is why the
# path is a single variable.
CACHE_DIR="${SEMAPHORE_NIX_CACHE_DIR:-$HOME/.cache/semaphore-nix-store}"

# Upper bound on the `target/` directory we are willing to push through the
# cache, in MiB. See the long comment in `save_cargo_cache` for why a cap is
# not optional: a bloated `target/` restore can be slower than a cold build
# (PRD #376 Open Question 2), and Semaphore documents a per-project cache
# quota (9.6 GB at the time of writing) with LRU eviction that a couple of
# multi-GB archives per platform would thrash.
: "${CARGO_TARGET_CACHE_MAX_MB:=4000}"

log() { printf '\n== %s\n' "$*"; }

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
    echo "no checksum tool available (checksum/md5sum/shasum)" >&2
    return 1
  fi
}

# Keys are readable on purpose — they show up in `cache list` output, and a
# human comparing a Semaphore run against a GHA run needs to see at a glance
# which toolchain and which lockfile a hit came from. Keys must not contain a
# comma: `cache restore` uses commas to separate fallback keys.
short() { printf '%.8s' "$1"; }

platform() { printf '%s-%s' "$(uname -s)" "$(uname -m)"; }

# The toolchain fingerprint — three files, because three files decide what the
# nix store ends up holding:
#   * devbox.json       — WHICH packages the shell has
#   * devbox.lock       — WHICH store path each pinned package resolves to
#   * gcloud/flake.lock — the one devbox.json entry (`path:gcloud#google-cloud-sdk`)
#                         that devbox.lock does NOT pin; its nixpkgs rev is
#                         pinned by that flake.lock instead
# This is also the "toolchain version" half of the cargo cache key, playing the
# role `Swatinem/rust-cache` fills by hashing `rustc -vV`: a Renovate devbox
# bump moves devbox.lock, which moves every key here, which invalidates both
# caches. Deriving it from the lockfiles instead of from `rustc --version`
# avoids a chicken-and-egg problem — the keys are needed before a toolchain
# exists to interrogate.
toolchain_fingerprint() {
  printf '%s-%s-%s' \
    "$(short "$(file_checksum devbox.json)")" \
    "$(short "$(file_checksum devbox.lock)")" \
    "$(short "$(file_checksum gcloud/flake.lock)")"
}

# Platform-scoped: aarch64-darwin and x86_64-linux resolve to different store
# paths for every package.
nix_cache_key() { printf 'nix-store-v1-%s-%s' "$(platform)" "$(toolchain_fingerprint)"; }

# NOT platform-scoped: the registry holds downloaded `.crate` sources and git
# checkouts, which are identical on Linux and macOS, so the two jobs can share
# one entry.
cargo_registry_key() { printf 'cargo-registry-v1-%s-%s' "$(toolchain_fingerprint)" "$(short "$(file_checksum Cargo.lock)")"; }
cargo_git_key() { printf 'cargo-git-v1-%s-%s' "$(toolchain_fingerprint)" "$(short "$(file_checksum Cargo.lock)")"; }

# Platform-scoped: compiled artifacts obviously are not portable.
cargo_target_key() { printf 'cargo-target-v1-%s-%s-%s' "$(platform)" "$(toolchain_fingerprint)" "$(short "$(file_checksum Cargo.lock)")"; }

# ---------------------------------------------------------------------------
# bootstrap — nix, the nix store cache, devbox, and the devbox environment
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
  # install, which is what the store handling below assumes: a root-owned
  # /nix/store, reached through the daemon.
  #
  # DELIBERATELY NOT PINNED, and that is a small irony worth naming: this whole
  # PRD is about pinning the toolchain, and the installer that provisions it
  # floats. Pin it (`.../nix/tag/v3.x.x`) once a real run has proved the
  # unpinned one works — pinning an untested URL first only doubles the number
  # of unknowns.
  log "installing nix (Determinate Systems nix-installer, unattended)"
  curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix \
    | sh -s -- install --no-confirm
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

# Restore a previously-built nix store.
#
# This is the cache that does not exist on GHA today (there, the devbox action's
# `enable-cache: true` owns it) and it is the reason a cold bootstrap costs
# minutes rather than seconds. M2 measured the Rust toolchain closure alone at
# ~1.5 GiB unpacked / ~330 MiB of compressed nar; the FULL devbox.json profile
# measures 4.4 GiB unpacked across 442 store paths on x86_64-linux, because the
# dev shell also carries ffmpeg, asciinema/agg, the Google Cloud SDK, gh, vals
# and upcloud-cli — none of which any CI step here uses. Trimming the CI closure
# is the single biggest available win and is deliberately NOT attempted in this
# spike: it would mean restructuring devbox.json, which is the contributor
# shell's contract.
#
# MECHANISM, and why it is this and not something simpler:
#   * The store is root-owned, so `cache store /nix/store` cannot work — the
#     restore runs as the agent user and could not recreate root-owned paths.
#     Hence an inner tar created under `sudo`.
#   * The tar carries ONLY `nix/store`, never `nix/var`. Overwriting the live
#     `/nix/var/nix/db` from an archive is how you get a store whose database
#     disagrees with its contents. Instead the paths are re-registered from a
#     `nix-store --dump-db` dump, which is nix's own documented mechanism for
#     moving a store between machines and is additive: the freshly-installed
#     nix's own paths stay registered.
#   * Restore happens AFTER `install_nix`, because on macOS the /nix volume has
#     to exist before anything can be extracted into it.
#
# ASSUMED, NOT OBSERVED: that `nix-store --load-db` is happy to be handed a
# dump on a store whose database already has entries. If the first real run
# fails here, the fallback is `nix copy --from file://<dir> --all
# --no-check-sigs`, which is slower (it re-imports NARs) but touches no
# database internals.
restore_nix_store() {
  local key
  key="$(nix_cache_key)"

  if ! cache has_key "$key"; then
    log "nix store cache MISS ($key) — this run pays the cold bootstrap"
    return
  fi

  log "nix store cache HIT ($key) — restoring"
  cache restore "$key" || true

  if [ ! -f "$CACHE_DIR/nix-store.tar.gz" ] || [ ! -f "$CACHE_DIR/nix-db.dump" ]; then
    log "restored archive is incomplete; falling through to a cold provision"
    return
  fi

  gzip -dc "$CACHE_DIR/nix-store.tar.gz" | sudo tar -C / -xpf -
  sudo "$NIX_BIN/nix-store" --load-db < "$CACHE_DIR/nix-db.dump"
  log "nix store restored and re-registered"
}

install_devbox() {
  if command -v devbox >/dev/null 2>&1; then
    log "devbox already installed: $(devbox version)"
    return
  fi
  # `FORCE=1` skips the installer's interactive confirmation; the script sudo's
  # the binary into /usr/local/bin, which is why the prologue puts that on PATH.
  log "installing devbox"
  curl -fsSL https://get.jetify.com/devbox | FORCE=1 bash
  export PATH="/usr/local/bin:$PATH"
}

# First `devbox run` is what actually realises the environment — nix fetches
# whatever the store restore did not supply. Doing it here, in the prologue,
# rather than implicitly inside the first build step, is what makes the
# bootstrap a separately-timed line in the Semaphore UI. M5 needs that number.
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
  restore_nix_store
  install_devbox
  provision_devbox_env
}

# ---------------------------------------------------------------------------
# save-nix-cache — runs from the pipeline's `always` epilogue
# ---------------------------------------------------------------------------

# Stored even when the job failed: the bootstrap succeeding is independent of
# the tests passing, and a red run that still warms the store is worth having.
# Stored only when the key is absent, mirroring how a content-derived key
# behaves on GHA — one write per (toolchain, platform) pair.
cmd_save_nix_cache() {
  local key
  key="$(nix_cache_key)"

  if cache has_key "$key"; then
    log "nix store cache already stored under $key — nothing to do"
    return
  fi

  log "storing the nix store under $key"
  mkdir -p "$CACHE_DIR"
  sudo "$NIX_BIN/nix-store" --dump-db > "$CACHE_DIR/nix-db.dump"
  # `gzip -1`: ~4.4 GiB of store is the input, and on a 4-vCPU agent the
  # difference between -1 and -6 is minutes of CPU for a fraction of the size.
  # The archive is created through a pipe so that only `tar` needs root and the
  # resulting file belongs to the agent user, which is what `cache store` runs
  # as. No `-p` on the create side: ownership and modes are recorded in the
  # archive either way, and bsdtar (macOS) rejects mode-specific extract flags
  # in `-c` mode. The extract side keeps `-p`.
  sudo tar -C / -cf - nix/store | gzip -1 > "$CACHE_DIR/nix-store.tar.gz"
  cache store "$key" "$CACHE_DIR"
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
#   * the three cached locations: `~/.cargo/registry`, `~/.cargo/git`, `target/`
#   * a prefix fallback on restore, so a lockfile bump starts from the previous
#     `target/` and rebuilds only what moved instead of building cold
#   * incremental compilation disabled (`CARGO_INCREMENTAL=0` in semaphore.yml)
#     and any incremental directory deleted before storing — rust-cache
#     disables incremental by default for exactly this reason
#   * dropping `~/.cargo/registry/src` (cargo re-extracts it from the `.crate`
#     files in `registry/cache`) and the `registry/index/*/.cache` blobs
#   * writing once per key, and only for a job that passed
#
# NOT reimplemented, and this is the part that matters:
#   * pruning `target/` down to artifacts the CURRENT dependency graph actually
#     references. rust-cache walks the metadata and deletes `deps/` entries for
#     versions that no longer exist. Without that, `target/` only grows: every
#     prefix-fallback restore carries forward artifacts for dependency versions
#     nobody builds any more.
#   * per-package pruning of build-script output directories
#   * cache-key invalidation on rustc/cargo environment changes beyond the
#     lockfiles
#
# The measured stakes: a long-lived local `target/` in this repo is 8.5 GiB
# (8.0 GiB debug, of which 1.4 GiB is incremental; 528 MiB release). A CI
# `target/` built by exactly `cargo build --release` plus `cargo nextest run`
# is smaller than that — but nobody has measured how much smaller, because
# nobody can run this pipeline yet. So there is a hard cap below, and when it
# trips it says so loudly instead of silently degrading: a restore that takes
# longer than the build it saves is the failure mode the PRD calls out, and it
# is invisible unless something prints it.

cmd_restore_cargo_cache() {
  local reg_key git_key tgt_key
  reg_key="$(cargo_registry_key)"
  git_key="$(cargo_git_key)"
  tgt_key="$(cargo_target_key)"

  # Comma-separated fallbacks, tried in order. The trailing-dash entries rely
  # on `cache restore` matching keys by prefix when there is no exact hit —
  # documented behaviour, but VERIFY IT on the first real run: if it does not
  # hold, every lockfile bump becomes a cold build and the numbers M5 collects
  # will be pessimistic for the wrong reason.
  log "restoring cargo caches"
  cache restore "$reg_key,cargo-registry-v1-$(toolchain_fingerprint)-,cargo-registry-v1-" || true
  cache restore "$git_key,cargo-git-v1-$(toolchain_fingerprint)-,cargo-git-v1-" || true
  cache restore "$tgt_key,cargo-target-v1-$(platform)-$(toolchain_fingerprint)-,cargo-target-v1-$(platform)-" || true
}

cmd_save_cargo_cache() {
  local reg_key git_key tgt_key size_mb
  reg_key="$(cargo_registry_key)"
  git_key="$(cargo_git_key)"
  tgt_key="$(cargo_target_key)"

  log "pruning before storing (partial rust-cache emulation — see comment above)"
  rm -rf target/debug/incremental target/release/incremental
  rm -rf "$HOME/.cargo/registry/src"
  if [ -d "$HOME/.cargo/registry/index" ]; then
    find "$HOME/.cargo/registry/index" -name .cache -type d -prune -exec rm -rf {} +
  fi

  if [ -d "$HOME/.cargo/registry" ]; then
    cache has_key "$reg_key" || cache store "$reg_key" "$HOME/.cargo/registry"
  fi
  if [ -d "$HOME/.cargo/git" ]; then
    cache has_key "$git_key" || cache store "$git_key" "$HOME/.cargo/git"
  fi

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

# ---------------------------------------------------------------------------

case "${1:-}" in
  bootstrap) cmd_bootstrap ;;
  save-nix-cache) cmd_save_nix_cache ;;
  restore-cargo-cache) cmd_restore_cargo_cache ;;
  save-cargo-cache) cmd_save_cargo_cache ;;
  *)
    echo "usage: $0 <bootstrap|save-nix-cache|restore-cargo-cache|save-cargo-cache>" >&2
    exit 2
    ;;
esac
