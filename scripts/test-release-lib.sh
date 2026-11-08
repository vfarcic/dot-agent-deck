#!/usr/bin/env bash
#
# The release-profile library test tier (issue #2134): the deck's own unit
# tests built with `--release`, so a test or a code path that holds only in a
# debug build is found before a release ships it. CI runs it as the advisory
# `release-lib` job (ci.yml) and the `RWX: release-lib` run
# (.rwx/release-lib.yml); docs/develop/release-lib-tier.md has why it exists,
# why it is advisory and which tests it leaves to the debug tiers.
#
# Usage: scripts/test-release-lib.sh [extra nextest args…]
#
# The bin is built first because `--lib` does not build it, and the lib tests
# that spawn a Wrapper-strategy agent (Codex) run `dot-agent-deck wrap` from
# the build beside the test binary (`target/release/dot-agent-deck`). Without
# it they fall back to whatever `dot-agent-deck` `$PATH` names, which on a
# runner is nothing (five of them then fail: the stand-in agent exits at once)
# and on a developer's machine is the installed deck.
#
# `OTEL_SDK_DISABLED` as the `test-fast` alias sets it (.cargo/config.toml).
set -euo pipefail

cargo build --release --bin dot-agent-deck
exec cargo nextest run --release --lib --config "env.OTEL_SDK_DISABLED='true'" "$@"
