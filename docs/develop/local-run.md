# Running a branch build locally (`task run`)

> **Developer / maintainer reference.** This page documents an internal development mechanism and is intentionally excluded from the published documentation site.

`task run` builds the checkout you are sitting in and starts the TUI against an **isolated sandbox daemon**, so trying a branch never bounces the deck you use every day. `task run-stop` shuts that sandbox daemon down. Both take `DIR` (the config dir, default the directory you invoke from), `SANDBOX` (default `DIR/.dad-sandbox`) and `PROFILE` (`release`|`debug`, default `release`).

The isolation itself is four documented production overrides — `DOT_AGENT_DECK_ATTACH_SOCKET`, `DOT_AGENT_DECK_SOCKET`, `DOT_AGENT_DECK_STATE_DIR`, `DOT_AGENT_DECK_SESSION`. Without them a branch build's build-version handshake (PRD #103/#161) SIGTERMs the daemon your installed build is using, silently when no agents are live.

## The trap: a pane can "test the branch" while running the installed release

This has cost real debugging time twice, and it is **silent** for every verb that exists in both builds.

Agents in a pane run whatever a bare `dot-agent-deck` resolves to on **their** `PATH` — and in this repo the role commands go through `devbox run <script>` (see `.dot-agent-deck.toml`). Two things then conspire:

1. `devbox.json`'s `init_hook` prepends `$HOME/.local/bin`, which is where the **installed release** lives.
2. A **nested** `devbox run` — deck-inside-devbox spawning an agent-inside-devbox, the normal shape here — re-derives `PATH` from devbox's own environment and **discards** whatever the parent prepended.

Measured, before the fix:

| Level | First `PATH` entry | `dot-agent-deck` resolves to |
|---|---|---|
| 1 — your shell after `task run` | the build dir | the branch build |
| 2 — the agent, via nested `devbox run` | `$HOME/.local/bin` | **the installed release** |

At level 2 the build dir was absent from `PATH` entirely. So the deck and its daemon were the branch build while every agent typed against the release — and `dispatch` (PRD #220), which does not exist in the release at all, appeared broken in ways the code did not explain.

**The fix, and why it is an env var.** Ordinary environment variables *do* survive that nesting, even though `PATH` does not. So `task run` exports `DAD_DEV_BIN="$(dirname "$bin")"` and `devbox.json`'s `init_hook` re-prepends it **after** the `$HOME/.local/bin` line, inside every devbox layer:

```
[ -n "$DAD_DEV_BIN" ] && export PATH="$DAD_DEV_BIN:$PATH" || true
```

Unset (the everyday case) it is a no-op and the installed build still wins, so nothing changes for normal use.

## Verifying which build you are actually running

**`--version` cannot tell these apart.** `main`, a feature branch, and the installed release all report the same `0.35.8`, because the version comes from `git describe --tags --abbrev=0` and they share the last tag. Read the **build id** instead, which carries the short SHA:

```sh
dot-agent-deck daemon hello
# {"ok":true,"server_version":6,"build_version":"0.35.8-gc8516ed","daemon_version":"0.35.8"}
```

Note that `daemon hello` prints the **invoked binary's** compiled-in id, not the running daemon's — it is a static print (that is deliberate; PRD #76 M2.21 uses it to detect wire skew across an ssh hop without spawning anything). To identify the live daemon, read its process instead:

```sh
pgrep -af "dot-agent-deck daemon serve"
readlink -f /proc/<pid>/exe
```

Inside a pane, the same question is answered by `command -v dot-agent-deck` — if that prints `$HOME/.local/bin/...`, the agent is on the installed release no matter what the deck itself is.

## Related

- A stale sandbox daemon outlives its TUI, which is what `task run-stop` is for. Left running, a later `task run` with a newer binary can attach to the older sandbox daemon.
- The e2e harness solves the same shadowing problem its own way — `path_with_binary_dir()` in `tests/e2e_dispatcher_mode.rs` prepends the build dir for the spawned deck. Note that its fixture agents are plain `claude`, never `devbox run`, so **no test exercises the devbox chain described above**; it is verified by hand.
