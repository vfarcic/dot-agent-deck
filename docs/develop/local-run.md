# Running a branch build locally (`task run`, `task run-all`)

> **Developer / maintainer reference.** This page documents an internal development mechanism and is intentionally excluded from the published documentation site.

These tasks build the checkout the `Taskfile.yml` lives in and run it against an **isolated sandbox daemon**, so trying a branch never bounces the deck you use every day.

| task | what it runs | `PROFILE` default |
| --- | --- | --- |
| `task run` | the TUI, in this terminal | `release` |
| `task run-desktop` | the desktop GUI (`pnpm tauri dev` from this checkout's `desktop/`), in this terminal | `debug` |
| `task run-all` | **both**, against one sandbox daemon: the TUI in this terminal, the desktop in the background | `debug` |
| `task run-stop` | stops a background desktop, then the sandbox daemon | the build that started the sandbox |

All four take `DIR` (the config dir the TUI starts in, default the directory you invoke from) and `SANDBOX` (the isolation state dir, default `DIR/.dad-sandbox`), either as `task run-all SANDBOX=…` or from the environment. `run-stop` passes anything after `--` to `daemon stop` — `task run-stop -- --force` stops a sandbox daemon that still has agents, which stops those agents. (Before this page gained `run-all`, `SANDBOX=` given as a task variable and the arguments after `--` were both silently dropped: the old script read `$SANDBOX` from the environment only and passed `"$@"`, which Task leaves empty.)

The desktop half needs its prerequisites first — a `devbox shell`, and `pnpm install` in `desktop/` once ([desktop-gui.md](desktop-gui.md#prerequisites)). `run-desktop` and `run-all` refuse to start when `pnpm` is not on `PATH` or `desktop/node_modules` is missing, and when port 1420 is taken, because vite binds it with `strictPort` — so at most one of them runs on a machine at a time, and neither beside another `pnpm dev`.

Everything below is implemented once, in [`scripts/sandbox-run.sh`](../../scripts/sandbox-run.sh), which all four tasks call. That is deliberate rather than tidy: the stop path has to export the same sockets as the start path, and a stop path missing the attach socket would stop **your** daemon.

## What the sandbox isolates

Every process the tasks start inherits these, and every one is an override the code already reads:

| variable | set to | why |
| --- | --- | --- |
| `DOT_AGENT_DECK_ATTACH_SOCKET`, `DOT_AGENT_DECK_SOCKET`, `DOT_AGENT_DECK_STATE_DIR`, `DOT_AGENT_DECK_SESSION` | `SANDBOX/attach.sock`, `hook.sock`, `state`, `session.toml` | Without them a branch build's build-version handshake (PRD #103/#161) SIGTERMs the daemon your installed build is using, silently when no agents are live. The desktop's local deck is `Endpoint::local()`, which resolves through the same `attach_socket_path()`, so this also puts the desktop on the sandbox daemon. |
| `DOT_AGENT_DECK_SCHEDULES` | `SANDBOX/schedules.toml` (normally absent) | The daemon loads the **global** `schedules.toml` at startup and registers every enabled entry, so a sandbox daemon would fire your real scheduled tasks a second time. A registered schedule is also an idle keep-alive, so it never exited on its own either: measured with two enabled schedules in the real file, a no-client, no-agent sandbox daemon without this override was still alive after 40s, and with it one idled out at 31s. |
| `DOT_AGENT_DECK_LOG` | `SANDBOX/deck.log` | The TUI and the daemon both append to this when it is set, and it is often exported by a shell profile — which put the sandbox's lines into your real `deck.log` (CLAUDE.md rule 12). Set unconditionally, so the sandbox always has a log. |
| `DOT_AGENT_DECK_DESKTOP_CONFIG` | `SANDBOX/desktop.toml` | The desktop's own settings document (`desktop/src-tauri/src/settings.rs`) otherwise lives in your real config dir, and its deck selection and remote rows would have the sandbox desktop connect to your **real** decks — and its Deck selector, zoom and settings sheet would write back to it. The override already existed, and since `settings_path()` is a plain environment read with no platform branch it moves the path the same way on Linux, macOS and Windows; `the_path_resolves_to_a_sibling_of_the_tui_config_and_honours_the_override` in `settings.rs` is its test. An absent file loads as defaults, which observe the local deck only — the sandbox's — and a path the desktop refuses (a relative one, say) falls back to defaults in memory and refuses to save, never to the real file. What you save in the sandbox desktop stays in `SANDBOX/desktop.toml`; delete it to start over from empty. |
| `DOT_AGENT_DECK_BINARY` | the build the task just made | Read only by the desktop, for **Start deck** and **Replace deck**, so a daemon it starts is this build rather than a sibling binary or whatever is first on `PATH`. |
| `PATH`, `DAD_DEV_BIN` | the build's directory, prepended | So an agent in a sandbox pane that types a bare `dot-agent-deck` reaches this build — see [the trap](#the-trap-a-pane-can-test-the-branch-while-running-the-installed-release) below. |

## What it does not isolate

- **The TUI's own preferences** — `config.toml`, `keybindings.toml`, `remotes.toml`, `config-gen-state.json`, `star-prompt-state.json` — stay in your real config dir, so whatever the sandbox TUI reads or saves there is your everyday copy. Each has an override; the tasks set none of them, so the branch runs with your preferences.
- **The `experimental` flag is not pinned.** The TUI resolves it from `DIR`'s `.dot-agent-deck.toml`, walking up the tree, so a `DIR` without one can pick up `~/.dot-agent-deck.toml`; the daemon the TUI starts does the same from the same directory, so the two agree. The desktop does not read the flag at all. The sandbox's `deck.log` says which file won (`experimental flag: ON (from …)`), and `DOT_AGENT_DECK_EXPERIMENTAL=1` (or `0`) in front of the task pins it, since the environment wins. Left unpinned because `DIR` is the project you chose to run the branch against, and its flag is part of that choice — unlike rule 12's cross-version comparison, where a flag inherited from the real home would skew a result.
- **Under `run-desktop` there is no TUI**, so the sandbox daemon starts when you press **Start deck**, in the directory the app runs in — inside this checkout's `desktop/` under `tauri dev`. Its startup-cwd project and its `[features]` therefore come from this checkout, not from `DIR`, which under `run-desktop` only locates the default sandbox.
- **The webview's `localStorage`** — overview columns, agent-profile drafts, prompts, workflow order ([the table](desktop-gui.md#the-five-localstorage-keys-and-the-one-that-is-gone)) — lives in the platform webview data directory for the app's identifier and is shared with every other `tauri dev` of this app on the machine. None of those keys names a deck or an endpoint, so it cannot connect the sandbox desktop anywhere; it is simply not reset per sandbox.
- **Agent hook configs.** At startup the deck auto-installs its hooks into agents' user-level configs, as your everyday deck does. The binary it pins there comes from `durable_binary_path()` (PRD #381), which never persists a cargo build artifact: from a `target/` build it pins `~/.local/bin/dot-agent-deck` or another non-artifact `dot-agent-deck` on `PATH`, and otherwise writes nothing.
- **Project files.** An orchestration launched in the sandbox writes `.dot-agent-deck/` in its project, like any other. Do not launch one in a project a live orchestration of your real deck is using.

## `task run-all`: the TUI and the desktop against one sandbox daemon

This is the shape `desktop-gui.md`'s [agent pane walk](desktop-gui.md#the-agent-pane-and-the-pty-resize-nobody-else-checks) needs — a TUI attached to the same agent as the desktop pane — without either client touching your real deck.

**What it does, in order.** It builds `PROFILE` (default `debug`, because `tauri dev` as the task runs it builds the desktop in debug and its `pretauri` hook rebuilds the debug daemon anyway, so a `release` default would add a second full compile of the same code). It starts the desktop in the background, in a process group of its own, and the TUI in the foreground, which lazy-spawns the sandbox daemon exactly as `task run` does. The desktop does not start `pnpm tauri dev` until `dot-agent-deck daemon status` answers from the sandbox (bounded at 30 seconds), because the desktop makes one connect-only probe at launch and a warm desktop could otherwise beat the daemon up and open on an error only a manual Reconnect clears.

**The desktop's output goes to `SANDBOX/desktop.log`**, never into the terminal the TUI owns. The path is printed before the TUI starts and again after it exits. The first run of `tauri dev` compiles the desktop crate, which can take minutes; `tail -f` that log from another terminal to watch it. If the desktop exits by itself — a failed build, a window that cannot open — nothing interrupts the TUI; when the TUI exits you are told, with the log's last lines.

**When the TUI exits, the desktop is stopped.** `tauri dev` is a tree — pnpm, the tauri CLI, vite, cargo and rustc while it builds, then the app and its WebKit processes — so the whole process group gets SIGTERM, and SIGKILL after five seconds if anything is left. In every teardown measured for this page on Linux — with the app running, and after the desktop had exited by itself — SIGTERM alone emptied the group. The group is led by a **keeper** (the script re-invoked with the sandbox path in its argv) that stays alive until the group is killed, so its id cannot be recycled while the desktop runs, and `task run-stop` checks the leader's command line names this sandbox before it signals anything — a recorded id that now belongs to something else is left alone. The keeper also stops the desktop itself in the two cases where nothing else would:

- **The shell running the TUI is killed outright.** SIGKILL skips its cleanup; the keeper sees its parent gone. Measured: the group was empty about 0.5s later.
- **The terminal hangs up, and nothing forwards SIGHUP.** Closing a terminal whose interactive shell ran `task run-all` is the ordinary case and is fine — the shell forwards SIGHUP to the job, the TUI exits and cleanup runs (measured: everything gone in about 0.5s). But the kernel signals only the session leader, and when that is `task` itself — `ssh -t host task run-all`, or a tmux pane whose command is `task` — `task` (3.53, measured) catches the signal and the TUI never receives one. The keeper watches for the session losing its terminal and stops the desktop (measured: about 1.25s). **The TUI itself keeps running in that case, with no terminal, burning CPU** (measured between 20% and 90% of a core); that is not new — the `task run` from before this page gained `run-all` does the same — and it is the TUI's to fix, not this script's.

**The sandbox daemon outlives the TUI**, as with `task run`: `task run-stop` shuts it down, and a sandbox daemon with no clients and no agents exits on its own after its 30-second idle window. `run-all` deliberately leaves `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` alone. The hazard rule 12 sets it to `0` for is a daemon started by hand and attached to later; here the TUI attaches the moment it starts the daemon and stays attached for as long as the desktop runs, so the window never opens underneath the desktop, and leaving the default in place lets an abandoned sandbox daemon clean itself up.

**Platforms.** The script is bash and is written for Linux and macOS (it reads no `/proc` and does not use `setsid`, which macOS lacks), but it has been **run only on Linux**. Windows is not supported: it needs bash job control and POSIX process groups.

### Do not run it from a worktree agents are editing (CLAUDE.md rule 16)

`tauri dev` watches the tree it runs from. On start it logs `Watching <checkout>/desktop/src-tauri`, `Watching <checkout>`, and the `xtask` members for changes, and vite serves `desktop/` with hot reload — so edits there rebuild and restart the app or reload its window, which rule 16 records destroying every terminal pane mid-run. Nothing in the task can prevent that. Run `run-all` and `run-desktop` from a checkout nobody is editing, such as a dedicated GUI worktree, and point agents at a different one.

### A commit breaks the pairing until you restart both (CLAUDE.md rule 15)

The build stamp is `<version>-g<sha>[-dirty]`, and `build.rs` recomputes it when `HEAD`, the branch ref or `.git/index` moves. So a commit — or staging that flips the `-dirty` suffix — restamps the desktop on its next rebuild, while the sandbox daemon keeps the stamp it started with. The desktop then refuses the sandbox daemon with a build mismatch. The recovery for the sandbox is:

1. Quit the TUI. The desktop goes with it.
2. `task run-stop` — or `task run-stop -- --force` if the sandbox still has agents, which stops them; it only ever reaches the sandbox's daemon.
3. `task run-all` again, which rebuilds both halves from the same commit.

`DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH=1` would also get the desktop connected, but it is for inspecting a daemon, not for a smoke check whose point is that the two sides are the same build.

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
