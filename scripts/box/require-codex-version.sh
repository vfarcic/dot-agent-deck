#!/usr/bin/env bash
#
# Exit 0 when the installed Codex CLI is at least MIN_CODEX_VERSION; otherwise
# say how to upgrade it and exit 1.
#
# The devbox `codex-big` script runs this before starting Codex, and
# bootstrap.sh beside it runs it to decide whether an installed Codex needs
# upgrading. Codex is installed with npm outside devbox, so nothing else pins
# its version. The minimum lives here only, so the two callers cannot drift.
# It sits in scripts/box/ because inmotion.sh ships only that directory to a
# remote box, and bootstrap.sh must find it there too.
#
# 0.160.0 because `codex-big` runs gpt-6.1-sol, and Codex 0.156.1 refused that
# model with a ChatGPT sign-in ("not supported when using Codex with a ChatGPT
# account") while 0.160.0 accepted it (measured 2026-10-02, PR #1485).
set -euo pipefail

MIN_CODEX_VERSION=0.160.0

installed=$(codex --version 2>/dev/null | awk '{print $NF}') || true
if [ -z "$installed" ]; then
  echo "Codex is not installed. Install it with: npm install -g @openai/codex@latest" >&2
  exit 1
fi

lowest=$(printf '%s\n%s\n' "$MIN_CODEX_VERSION" "$installed" | sort -V | head -n 1)
if [ "$lowest" != "$MIN_CODEX_VERSION" ]; then
  echo "Codex $installed is older than $MIN_CODEX_VERSION, which this repo's Codex scripts need for gpt-6.1-sol." >&2
  echo "Upgrade it with: npm install -g @openai/codex@latest (prefix with sudo if Codex is installed system-wide)" >&2
  exit 1
fi
