#!/usr/bin/env bash
#
# Install the EXACT Rust toolchain this repo pins, on any Semaphore agent.
#
# WHY THIS EXISTS RATHER THAN `sem-version rust <v>`:
#
#   * The ubuntu2404 agent image ships Rust 1.95.0, but this repo pins 1.97.1
#     (devbox.json, and every `dtolnay/rust-toolchain` step in
#     .github/workflows). Using whatever the image happens to ship would
#     reintroduce exactly the drift that ci.yml's `devbox` job comment spends
#     three paragraphs arguing against: "green locally" and "green in CI" have
#     to be the same claim, so the toolchain is pinned, never floating.
#   * `sem-version` cannot install an arbitrary point release — it switches
#     between the versions baked into the image. There is no guarantee 1.97.1
#     is one of them, and a silent fallback to 1.95.0 is the failure mode this
#     script exists to prevent.
#   * `sem-version rust` is not available on macOS agents AT ALL (the toolbox
#     supports only ruby and node there), and the macOS images ship no Rust at
#     all — so the Linux path and the macOS path would need different
#     mechanisms anyway. rustup is the one mechanism that works on both.
#
# Inputs (all via env, so a pipeline block can vary them without editing this
# file):
#
#   RUST_TOOLCHAIN   required. e.g. 1.97.1
#   RUST_COMPONENTS  optional, comma-separated. e.g. rustfmt,clippy
#   RUST_TARGETS     optional, comma-separated. e.g. x86_64-pc-windows-msvc
#   NEXTEST_VERSION  optional. When set, installs that exact cargo-nextest.
#
# The caller must put ~/.cargo/bin on PATH itself — this runs in a subshell, so
# its own `export` cannot reach the job's shell. Every pipeline block that
# sources this follows it with an explicit `export PATH=...` line.
set -euo pipefail

TOOLCHAIN="${RUST_TOOLCHAIN:?RUST_TOOLCHAIN must be set (e.g. 1.97.1)}"
COMPONENTS="${RUST_COMPONENTS:-}"
TARGETS="${RUST_TARGETS:-}"
NEXTEST="${NEXTEST_VERSION:-}"

# The macOS agents have no Rust and therefore no rustup. Install it with the
# toolchain set to `none` so that the pinned install below is the ONLY toolchain
# that ever lands — a `--default-toolchain stable` here would download a second,
# floating toolchain and leave which one `cargo` resolves to up to ordering.
if ! command -v rustup >/dev/null 2>&1; then
  echo "rustup not found; installing it (toolchain: none)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --profile minimal --no-modify-path
fi
export PATH="$HOME/.cargo/bin:$PATH"

args=(--profile minimal --no-self-update)

# Split on commas WITHOUT `read -a`, whose behavior under `set -u` with an empty
# string differs between bash 3.2 (what macOS ships) and bash 5.x (Ubuntu).
if [ -n "$COMPONENTS" ]; then
  while IFS= read -r c; do
    [ -n "$c" ] && args+=(--component "$c")
  done <<< "${COMPONENTS//,/$'\n'}"
fi
if [ -n "$TARGETS" ]; then
  while IFS= read -r t; do
    [ -n "$t" ] && args+=(--target "$t")
  done <<< "${TARGETS//,/$'\n'}"
fi

rustup toolchain install "$TOOLCHAIN" "${args[@]}"
rustup default "$TOOLCHAIN"

# Assert rather than trust. If the pin silently failed to take effect, every
# downstream gate would still run — against the wrong compiler — and report
# green, which is precisely the drift this script exists to prevent.
actual="$(rustc --version | awk '{print $2}')"
if [ "$actual" != "$TOOLCHAIN" ]; then
  echo "ERROR: pinned toolchain is ${TOOLCHAIN} but 'rustc --version' reports ${actual}." >&2
  exit 1
fi
echo "OK: $(rustc --version)"
echo "OK: $(cargo --version)"

# A prebuilt tarball, not `cargo install cargo-nextest --locked`, which compiles
# nextest and its dependency tree from source and costs minutes on every job.
# This is the same trade `taiki-e/install-action` makes in .github/workflows.
if [ -n "$NEXTEST" ]; then
  case "$(uname -s)" in
    Darwin) nextest_platform="mac" ;;
    Linux)  nextest_platform="linux" ;;
    *)      echo "ERROR: unsupported platform for cargo-nextest: $(uname -s)" >&2; exit 1 ;;
  esac
  mkdir -p "$HOME/.cargo/bin"
  curl -LsSf "https://get.nexte.st/${NEXTEST}/${nextest_platform}" \
    | tar zxf - -C "$HOME/.cargo/bin"
  echo "OK: $(cargo nextest --version)"
fi
