---
name: run-dot-agent-deck
description: Run, build, smoke-test, and screenshot the dot-agent-deck binary against an isolated sandbox. Use when asked to "run dot-agent-deck", "launch the TUI", "take a screenshot of the dashboard", "verify the CLI smoke", or "test a change locally" against the dot-agent-deck binary.
---

# Run dot-agent-deck

`dot-agent-deck` is a Rust TUI/CLI that orchestrates terminal agent
sessions: a `ratatui` dashboard talking over Unix-socket protocols to
a `portable-pty`-spawned daemon. Day-to-day driving lives in
`.claude/skills/run-dot-agent-deck/driver.sh` — it shells out the
binary against a fresh tempdir sandbox (so it does NOT attach to the
developer's real daemon and render their actual workflow), captures
the dashboard with tmux, and verifies clean shutdown.

All paths are relative to the repo root.

## Why a sandbox

`dot-agent-deck` binds Unix sockets and `flock`s lock files at
user-global paths (`$XDG_RUNTIME_DIR/dot-agent-deck/*`, fallback
`$HOME/.cache/dot-agent-deck/`). If you launch the TUI without
redirecting these:

- It attaches to your already-running daemon and shows your real
  Claude sessions in the dashboard (verified — it picked mine up).
- Or it contends on your daemon's lock files and fails to start.
- `hooks install` rewrites your shell rc files.

The driver redirects every relevant env var (`DOT_AGENT_DECK_SOCKET`,
`DOT_AGENT_DECK_ATTACH_SOCKET`, `DOT_AGENT_DECK_LOCK_DIR`,
`DOT_AGENT_DECK_STATE_DIR`, `DOT_AGENT_DECK_CONFIG`,
`DOT_AGENT_DECK_SESSION`, plus `HOME`, `XDG_RUNTIME_DIR`,
`XDG_STATE_HOME`) into a fresh tempdir per run, so the smoke is
hermetic.

## Prerequisites

```bash
apt-get install -y tmux              # already present on this container
```

Rust toolchain via `rustup` (cargo 1.95+ / rustc 1.95+ verified). No
other system libraries needed — the binary statically links
`vt100`, `portable-pty`, `crossterm`, etc.

## Build

```bash
cargo build --release
```

Release build because the TUI's render path benefits — debug builds
visibly stutter. Optimized build takes ~25 s clean, ~1 s incremental.

## Run (agent path) — driver.sh

```bash
./.claude/skills/run-dot-agent-deck/driver.sh all
```

Subcommands:

- `cli` — non-interactive surface only: `--version`, `--help`,
  `init`, `validate`. ~0.1 s. Right for PRs that touch the CLI or
  config-loading code paths.
- `tui` — launches the dashboard under tmux, waits for first paint,
  saves a `capture-pane` screenshot, drives the `Ctrl+C` → `Enter`
  quit dialog, verifies the tmux session ends cleanly. ~1.5 s.
- `all` — both, in order. Default.

Output of a clean run:

```
== CLI smoke ==
dot-agent-deck 0.25.0
Created ./.dot-agent-deck.toml
Config is valid.
   init + validate OK in sandbox project
== TUI smoke ==
   screenshot saved to /tmp/dad-screenshot.txt (40 lines)
   TUI exited cleanly
OK
```

The screenshot lands at `/tmp/dad-screenshot.txt` (override with
`DAD_SCREENSHOT=...`). A clean sandbox shows:

```
                                   No active sessions. Press Ctrl+n to create a pane.

...

Ctrl+n: new  Ctrl+w: close  Ctrl+t: layout  Ctrl+d: dashboard (1-9 ? /)  Ctrl+c: quit
```

`No active sessions` is the signal that the sandbox is isolated — a
non-isolated run would render whatever sessions your real daemon
knows about.

Override the binary path with `DAD_BIN=...` to smoke an alternate
build (e.g. `target/debug/dot-agent-deck` for faster iteration).

## Run (human path)

```bash
./target/release/dot-agent-deck
```

Opens the dashboard against the user's REAL state — useful only as
the developer running on a workstation with `hooks install`'d agent
hooks. Useless headless. The driver above is the agent path.

## Test

```bash
cargo test
```

~1.5 minutes on this container; 800+ tests. Covers the daemon
protocol, PTY orchestration, delegate dispatch, and F12 reattach
paths — most PRs that touch internals are verified by `cargo test`
alone, not by the driver. The driver is for the TUI/CLI surface and
the screenshot artifact.

Also gate before commit (per `CLAUDE.md`):

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Gotchas

- **The dashboard renders YOUR sessions if you skip the sandbox.**
  The first time I launched the TUI without env-var overrides, it
  attached to my own running daemon and showed my live Claude pane
  inside its own pane. That's how I noticed the isolation problem.
  Always go through `driver.sh`.
- **`Ctrl+C` doesn't quit — it opens a confirm dialog.** The dialog
  has `Stop / Cancel`. `Stop` is default-selected, so `Enter`
  confirms. The driver pauses 0.5 s between the two send-keys
  because the binary reads the dialog state synchronously between
  events; back-to-back sends drop one.
- **Don't run two `driver.sh` invocations concurrently from the
  same shell.** Each gets its own tempdir, but they share `$$`
  (which seeds the tmux session name suffix in the script). Use
  separate shells if you need parallel smokes.
- **Commands with embedded double quotes break `.dot-agent-deck.toml`
  basic strings silently** — the TOML parser returns an error and the
  role lookup falls through to "no respawn" with no visible signal in
  the TUI. Use TOML basic strings with `\"` escapes, or single-quote
  the TOML value if the shell command contains no `'`. The
  F12 e2e test in `tests/orchestration_delegate.rs` documents this.
- **TUI may not exit cleanly under tmux smaller than ~24×80.** The
  Stop/Cancel dialog needs ~5 rows; on a tiny pane the dialog clips
  and the Enter doesn't land on the focused button. The driver uses
  120×40.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `binary not found at .../target/release/dot-agent-deck` | `cargo build --release` first; driver intentionally does not auto-build (separates "build broke" from "binary broke"). |
| `TUI did not render within 8s` | The dashboard's first frame depends on the daemon coming up. Increase the timeout, or run `target/release/dot-agent-deck` under `RUST_LOG=debug` outside the driver to see what's stuck. |
| `TUI did not exit after Ctrl+C + Enter` | The Stop/Cancel dialog probably did not open — check the dumped `capture-pane`. If the dashboard is missing the bottom-row hotkey hint, the binary is in a broken state; rebuild. |
| `Address already in use` from socket bind | A previous driver run left a stale socket. `rm -rf $XDG_RUNTIME_DIR/dot-agent-deck` if you set `XDG_RUNTIME_DIR` yourself; the driver's own tempdir gets cleaned by `trap`. |
| `hooks install` modified my shell rc files | You ran `dot-agent-deck hooks install` without sandboxing. Restore from git or undo manually. The driver never runs `hooks install`. |
