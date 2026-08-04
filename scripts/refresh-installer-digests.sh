#!/usr/bin/env bash
#
# Regenerate the six sha256 digests `.semaphore/ci.sh` pins its two installers
# to, for the versions currently pinned in that same file.
#
# WHY THIS EXISTS — it is the second half of a Renovate bump.
#
# `renovate.json`'s `semaphore-ci-installers` custom manager keeps the two
# VERSION strings in `.semaphore/ci.sh` up to date. Renovate cannot compute the
# sha256 of a GitHub release asset, so its PR bumps the version and leaves six
# now-wrong digests behind, and `ci.sh` fails closed on the first mismatch. That
# is exactly why that PR is NOT automerged: it is not mergeable until this
# script has been run and its result committed onto the branch.
#
# Usage, from the repository root:
#   scripts/refresh-installer-digests.sh                  # rewrite the digests in place
#   scripts/refresh-installer-digests.sh --check          # verify only; nonzero exit if stale
#   scripts/refresh-installer-digests.sh --verify-assets  # also re-hash the devbox tarballs
# or `task refresh-installer-digests`.
#
# WHERE EACH DIGEST COMES FROM. The two sources differ on purpose, and the
# difference IS the security claim `ci.sh` documents — do not "simplify" them
# into one path:
#
#   * devbox — read out of the VENDOR'S OWN `checksums.txt` asset in the same
#     release. Not hashed here, so what lands in `ci.sh` is jetify's published
#     value rather than a blessing of whatever bytes this machine downloaded.
#     `--verify-assets` additionally fetches each tarball and confirms it hashes
#     to that value, which is the "independently re-computed" half of ci.sh's
#     comment; without the flag nothing but `checksums.txt` is downloaded.
#   * nix-installer — Determinate publishes no checksum file and no build
#     provenance attestation for these binaries, so hashing the asset here is
#     the only option. Those digests are SELF-RECORDED, `ci.sh` discloses that,
#     and the disclosure names the date they were measured — so if this script
#     CHANGES them, update that date too. It says so when it does.
#
# It never writes a digest for an asset it could not fetch. Every download,
# every checksum lookup and every pin line it expects to find is checked, and
# any failure aborts before `.semaphore/ci.sh` is touched at all.

set -euo pipefail

CI_SH=".semaphore/ci.sh"

log() { printf '== %s\n' "$*"; }
die() { printf '\nERROR: %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '3,30p' "$0" | sed 's/^#\{1,2\} \{0,1\}//'
}

MODE=write
VERIFY_ASSETS=0
for arg in "$@"; do
  case "$arg" in
    --check) MODE=check ;;
    --verify-assets) VERIFY_ASSETS=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument '$arg' (expected --check and/or --verify-assets)" ;;
  esac
done

[ -f "$CI_SH" ] || die "run this from the repository root ($CI_SH not found in $PWD)"

# Same two-tool fallback as ci.sh's own `sha256_of`, for the same reason: the
# macOS agent has `shasum` and no `sha256sum`.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "no sha256 tool available (sha256sum/shasum)"
  fi
}

# Same flags ci.sh downloads with. No `--retry`: a transient failure here should
# abort loudly rather than be papered over, because the alternative to a clean
# abort is a half-refreshed digest table.
fetch() {
  local url="$1" out="$2"
  curl --proto '=https' --tlsv1.2 -sSf -L -o "$out" "$url" ||
    die "download failed: $url"
  [ -s "$out" ] || die "download produced an empty file: $url"
}

is_sha256() { printf '%s' "$1" | grep -qE '^[0-9a-f]{64}$'; }

# Read one `VAR="value"` pin out of ci.sh, insisting there is exactly one.
read_pin() {
  local var="$1" matches value
  matches="$(grep -c -E "^${var}=\"[^\"]*\"$" "$CI_SH" || true)"
  [ "$matches" = "1" ] ||
    die "expected exactly one ${var}=\"…\" line in $CI_SH, found $matches"
  value="$(sed -n -E "s/^${var}=\"([^\"]*)\"$/\1/p" "$CI_SH")"
  [ -n "$value" ] || die "$var is pinned to an empty string in $CI_SH"
  printf '%s' "$value"
}

NIX_VERSION="$(read_pin NIX_INSTALLER_VERSION)"
DEVBOX_VERSION="$(read_pin DEVBOX_VERSION)"

# The release URLs are built from these, so a bump that changed the SHAPE of a
# tag (dropping nix-installer's `v`, say) should say so here rather than surface
# as an opaque 404 three downloads later.
case "$NIX_VERSION" in
  v[0-9]*) ;;
  *) die "NIX_INSTALLER_VERSION='$NIX_VERSION' does not look like a v-prefixed tag" ;;
esac
case "$DEVBOX_VERSION" in
  [0-9]*) ;;
  *) die "DEVBOX_VERSION='$DEVBOX_VERSION' does not look like an unprefixed version" ;;
esac

log "pinned in $CI_SH: nix-installer $NIX_VERSION, devbox $DEVBOX_VERSION"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Three parallel arrays rather than one associative array, so this runs on the
# bash 3.2 that macOS still ships.
#   ANCHOR — the literal text in ci.sh immediately preceding the digest, used
#            both to locate the line and as the replacement's left-hand side.
#            Written as an ERE, so `.` is escaped.
ANCHORS=()
DIGESTS=()
LABELS=()
add_spec() {
  ANCHORS+=("$1")
  DIGESTS+=("$2")
  LABELS+=("$3")
}

# --- nix-installer: self-hashed, no vendor checksum exists -------------------
NIX_BASE="https://github.com/DeterminateSystems/nix-installer/releases/download/${NIX_VERSION}"
for asset in nix-installer-x86_64-linux nix-installer-aarch64-linux nix-installer-aarch64-darwin; do
  log "hashing $asset ($NIX_VERSION)"
  fetch "$NIX_BASE/$asset" "$WORK/$asset"
  digest="$(sha256_of "$WORK/$asset")"
  is_sha256 "$digest" || die "computed a non-sha256 value for $asset: '$digest'"
  add_spec "printf '$asset " "$digest" "$asset"
done

# --- devbox: the vendor's own checksums.txt ----------------------------------
DEVBOX_BASE="https://github.com/jetify-com/devbox/releases/download/${DEVBOX_VERSION}"
log "fetching devbox checksums.txt ($DEVBOX_VERSION)"
fetch "$DEVBOX_BASE/checksums.txt" "$WORK/checksums.txt"

for plat in linux_amd64 linux_arm64 darwin_arm64; do
  asset="devbox_${DEVBOX_VERSION}_${plat}.tar.gz"
  matches="$(awk -v f="$asset" '$2 == f { print $1 }' "$WORK/checksums.txt")"
  count="$(printf '%s' "$matches" | grep -c . || true)"
  [ "$count" = "1" ] ||
    die "expected exactly one checksums.txt entry for $asset, found $count
Either the release does not publish that platform any more, or the file's format
changed. Do not hand-edit a digest around this."
  is_sha256 "$matches" || die "checksums.txt entry for $asset is not a sha256: '$matches'"

  if [ "$VERIFY_ASSETS" = "1" ]; then
    log "re-hashing $asset to confirm the vendor's value"
    fetch "$DEVBOX_BASE/$asset" "$WORK/$asset"
    got="$(sha256_of "$WORK/$asset")"
    [ "$got" = "$matches" ] || die "$asset does not match the vendor's checksums.txt
  checksums.txt $matches
  computed     $got
That is either a mutated release asset or a corrupted download. Do not proceed."
  fi

  add_spec "printf 'devbox_%s_${plat}\\.tar\\.gz " "$matches" "$asset"
done

# The platform lists above MIRROR ci.sh's `nix_installer_asset`/`devbox_asset`
# case statements, and nothing links the two copies. So count the digests ci.sh
# actually carries and refuse to touch the file if it is not the number this
# script knows how to refresh — a fourth platform added there must be added
# here too, and a silent partial refresh is exactly the failure mode this whole
# script exists to prevent.
expected="${#ANCHORS[@]}"
found="$(grep -c -E "printf '(nix-installer-[A-Za-z0-9_-]+|devbox_%s_[A-Za-z0-9_-]+\.tar\.gz) [0-9a-f]{64}" "$CI_SH" || true)"
[ "$found" = "$expected" ] ||
  die "$CI_SH carries $found pinned digests but this script knows $expected.
Its platform list has drifted from ci.sh's asset case statements — update
scripts/refresh-installer-digests.sh rather than refreshing only some of them."

# --- rewrite ----------------------------------------------------------------
NEW="$WORK/ci.sh.new"
cp "$CI_SH" "$NEW"

i=0
while [ "$i" -lt "$expected" ]; do
  anchor="${ANCHORS[$i]}"
  digest="${DIGESTS[$i]}"
  label="${LABELS[$i]}"
  n="$(grep -c -E "${anchor}[0-9a-f]{64}" "$NEW" || true)"
  [ "$n" = "1" ] ||
    die "expected exactly one pin line for $label in $CI_SH, found $n (anchor: ${anchor})"
  sed -E "s|(${anchor})[0-9a-f]{64}|\\1${digest}|" "$NEW" >"$WORK/step"
  mv "$WORK/step" "$NEW"
  i=$((i + 1))
done

if cmp -s "$CI_SH" "$NEW"; then
  log "OK: all $expected digests in $CI_SH already match the pinned versions"
  exit 0
fi

diff -u "$CI_SH" "$NEW" || true

if [ "$MODE" = "check" ]; then
  die "the digests in $CI_SH are stale for the versions pinned in it (diff above).
Run: scripts/refresh-installer-digests.sh"
fi

# `cat >` rather than `mv`, to keep the tracked file's own mode (it is +x).
cat "$NEW" >"$CI_SH"
log "rewrote the digest table in $CI_SH (diff above)"
log "REMINDER: the nix-installer digests are SELF-RECORDED and the comment above"
log "    them in $CI_SH names the date they were measured. If a nix-installer"
log "    digest changed, update that date in the same commit."
