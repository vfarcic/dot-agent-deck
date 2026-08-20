---
sidebar_position: 3
title: Installation
---

# Installation

## Platform Support

| Platform | Status |
|---|---|
| macOS (Intel & Apple Silicon) | Supported |
| Linux (amd64 & arm64) | Supported |
| Windows (via WSL) | Supported (runs as Linux) |
| Windows (native) | Coming soon ([#42](https://github.com/vfarcic/dot-agent-deck/issues/42)) — comment on the issue if you need this! |

## Homebrew (macOS / Linux)

```bash
brew tap vfarcic/tap
brew install dot-agent-deck
```

## Nix

Requires Nix with flakes enabled (`nix-command` and `flakes` in `experimental-features`). The flake builds from source against the committed `Cargo.lock`, so there is no per-release hash to maintain, and it pins the released version, so `dot-agent-deck --version` reports the real version rather than a source-build placeholder. It covers `x86_64-linux`, `aarch64-linux` and `aarch64-darwin`. Intel Macs are served by the release binaries and the Homebrew tap instead, because the nixpkgs this flake pins has dropped `x86_64-darwin`.

Run it once without installing anything:

```bash
nix run github:vfarcic/dot-agent-deck
```

Arguments after `--` reach the binary, so `nix run github:vfarcic/dot-agent-deck -- hooks install` works too. Pin a specific release by appending its tag: `github:vfarcic/dot-agent-deck/<tag>`. That works for any release from the first one that ships the flake onwards; earlier tags predate it, so there is no flake at that revision to build.

Install it into your user profile:

```bash
nix profile install github:vfarcic/dot-agent-deck
```

**As a flake input.** NixOS and home-manager users add the flake as an input and take the package from it:

```nix
{
  inputs.dot-agent-deck.url = "github:vfarcic/dot-agent-deck";
  # Optional. Build against your nixpkgs rather than the one this flake pins.
  inputs.dot-agent-deck.inputs.nixpkgs.follows = "nixpkgs";

  # NixOS
  environment.systemPackages = [ inputs.dot-agent-deck.packages.${pkgs.system}.default ];

  # home-manager
  home.packages = [ inputs.dot-agent-deck.packages.${pkgs.system}.default ];
}
```

The `follows` line is the usual tradeoff: one nixpkgs in your closure instead of two, paid for by building against a nixpkgs this project has not tested against, which has to be recent enough to carry rustc 1.85 or newer.

**Via the overlay.** If you would rather reach it as `pkgs.dot-agent-deck` everywhere, apply the overlay instead:

```nix
{
  nixpkgs.overlays = [ inputs.dot-agent-deck.overlays.default ];

  environment.systemPackages = [ pkgs.dot-agent-deck ];
}
```

The overlay always builds against *your* nixpkgs, never the pinned one, so that rustc 1.85 minimum applies here whether or not you set `follows`. The crate is edition 2024, which is where the floor comes from.

### The home-manager module

`homeModules.default` installs the binary and writes your configuration declaratively. Import it and turn it on:

```nix
{
  imports = [ inputs.dot-agent-deck.homeModules.default ];

  programs.dot-agent-deck = {
    enable = true;

    # ~/.config/dot-agent-deck/config.toml
    settings = {
      default_command = "claude";
      worker_response_timeout_minutes = 90;
    };

    # ~/.config/dot-agent-deck/keybindings.toml
    keybindings = {
      global = {
        toggle_layout = "Alt+Shift+l";
        new_pane = "";            # empty string unbinds an action
      };
      dashboard.help = "F1";
    };
  };
}
```

`settings` and `keybindings` are freeform attribute sets rendered to TOML, so anything the two files accept can go in them. See [Configuration](configuration.md) for `config.toml` and [Keyboard Shortcuts](keyboard-shortcuts.md#customizing-keybindings) for the keybinding actions and their defaults. Each file is written only when its attribute set is non-empty, so `enable = true` on its own installs the package and leaves any config you already have untouched.

`package` defaults to this flake's package built against your own nixpkgs, which is exactly what the overlay produces, so applying the overlay and importing the module gets you one build of the tool rather than two. Set it explicitly to `inputs.dot-agent-deck.packages.${pkgs.system}.default` if you would rather build against the nixpkgs this flake pins.

**What the module deliberately does not manage.** Only `config.toml` and `keybindings.toml`. The other three files in that directory are left alone on purpose:

| File | Why it is left alone |
|---|---|
| `session.toml` | Runtime state the deck writes itself (your saved workspace). home-manager symlinks its files read-only out of the Nix store, so managing it would stop the deck saving. |
| `remotes.toml` | Written imperatively by `dot-agent-deck remote add`, same problem. |
| `schedules.toml` | The one file whose location honours `$XDG_CONFIG_HOME`, unlike its neighbours. Managing it correctly needs handling the other two files must not get, so it is left for a follow-up. |

**Hooks are still a one-off imperative step.** The module does not run `dot-agent-deck hooks install` for you, because that command edits *other* tools' configuration (Claude, OpenCode, Codex and friends), which sits outside home-manager's ownership and does not roll back when you switch generations. Run it once yourself after the first activation:

```bash
dot-agent-deck hooks install
```

### Contributing?

`nix develop` is a consumer-oriented shell carrying just the Rust toolchain; contributors should use devbox instead, which pins the toolchain version and ships the recording and docs tooling the test suites need.

## Download Binary

Download the latest binary for your platform from the [Releases](https://github.com/vfarcic/dot-agent-deck/releases/latest) page. Binaries are available for Linux (amd64, arm64) and macOS (amd64, arm64).

## Build from Source

```bash
git clone https://github.com/vfarcic/dot-agent-deck.git
cd dot-agent-deck
cargo build --release
```

The binary will be at `target/release/dot-agent-deck`.

## Verify

```bash
dot-agent-deck --help
```

## How it runs

The first time you run `dot-agent-deck`, the binary auto-spawns a small per-user background daemon and connects to it over a Unix socket (under `$XDG_RUNTIME_DIR` when available, otherwise a per-uid path in `/tmp`). The same daemon is used for both local and remote (`dot-agent-deck connect`) sessions; there is no separate "local mode".

The daemon outlives the TUI: detach the deck, your agents keep running, reattach later and they're still there. About 30 seconds after the TUI has detached *and* every managed agent is gone, the daemon exits on its own. Set `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` to override the window (`0` keeps it up indefinitely).

## Upgrading

After upgrading the `dot-agent-deck` binary, just relaunch it:

```bash
dot-agent-deck
```

On every launch, the TUI performs a build-version handshake with the running daemon. If a daemon spawned by the previous version is still alive, the binary versions differ and the TUI resolves it for you — what happens depends only on whether managed agents are running:

- **No agents running** — the stale daemon is restarted **silently**. There is nothing to lose, so you are not prompted; the TUI lazy-spawns a fresh daemon at the new version and continues into the dashboard. This is the common case after a quiet upgrade.
- **Agents running** — the TUI prompts you in your terminal. The prompt **names the live agents** and warns that restarting the daemon stops them. Press **S** to restart and continue on a fresh daemon at the new version (your agents are stopped), or press any other key to **keep the current daemon** and stay attached to it with your agents intact. Declining never strands you — you always land on a working session.

You are never forced to upgrade-and-restart just to keep working: declining the prompt keeps you on the existing daemon, and you can finish or detach your agents and relaunch later, at which point (with no agents running) the daemon restarts silently.

If the TUI is not attached to a terminal (CI, scripts, piped stdout) **and** agents are running, it cannot prompt for the restart, so it prints a recovery hint to stderr and exits non-zero. In that case, run `dot-agent-deck daemon stop` explicitly before relaunching — see [Recycling the local daemon](#recycling-the-local-daemon) below. (With no agents running, the non-interactive case still restarts silently.)

See [Troubleshooting › Delegate prompts silently no-op after staying on an older daemon](troubleshooting.md#delegate-prompts-silently-no-op-after-staying-on-an-older-daemon) for the symptom you'll see if you keep an older daemon and then expect newer features to work against it.

## Versioning

`dot-agent-deck` is still in its `0.x` series, and the version digits follow a compatibility-first cadence while the major version is `0`:

- A **protocol-/compatibility-breaking** change — one where an older and a newer build can no longer safely interoperate — bumps the **minor** digit (for example `0.31.x → 0.32.0`).
- **New features and bug fixes** are **patch** releases (for example `0.31.1 → 0.31.2`).

So while in `0.x` the minor digit signals **"compatibility broke"**, not "has new features". A bump from `0.31.x` to `0.32.0` is the cue to align both sides (see [Recycling the local daemon](#recycling-the-local-daemon) locally, or `dot-agent-deck remote upgrade` for a [remote](remote-environments.md)); a patch bump is always safe to mix.

## Inspecting the local daemon

`dot-agent-deck daemon status` prints a read-only snapshot of the agents the local daemon is managing. It is a diagnostic: it never starts, stops, attaches to, or writes to anything, so it is safe to run at any time — including from a script, a `watch`, or while the TUI is open in another terminal.

```bash
dot-agent-deck daemon status
```

```text
PANE	AGENT	ROLE	STATUS	TOOL	LABEL	CWD
1	1	mode:review	Thinking	-	api	/home/you/src/api
2	2	-	Working	Bash	api	/home/you/src/api
```

The columns are tab-separated, so pipe the output through `column -t -s $'\t'` if you want it aligned in a terminal. A `-` means the daemon has no value for that cell yet.

| Column | What it holds |
|---|---|
| `PANE` | The pane id, the same value a managed agent sees as `DOT_AGENT_DECK_PANE_ID`. |
| `AGENT` | The daemon's own id for the agent. |
| `ROLE` | `mode:<name>` for a pane launched into a [mode](configuration.md), the role name for an [orchestration](orchestration.md) pane (suffixed `(orchestrator)` for the start role), `-` for a plain dashboard pane. |
| `STATUS` | The live status: one of `Thinking`, `Working`, `Compacting`, `WaitingForInput`, `Idle`, `Error`. |
| `TOOL` | The name of the tool the agent is running right now — the name only, never its arguments. |
| `LABEL` | The pane's display name. |
| `CWD` | The directory the agent was launched in. |

When no agents are managed, the command prints `no managed agents` and still exits 0 — an empty deck is a successful answer, not a failure.

### JSON for scripts

`--json` emits a versioned document instead of the table. This is the form to parse; the human table's columns are not a stable interface. The command prints it on a single line — the example below is reformatted for readability.

```bash
dot-agent-deck daemon status --json
```

```json
{
  "schema_version": 2,
  "agents": [
    {
      "agent_id": "1",
      "pane_id": "1",
      "label": "api",
      "cwd": "/home/you/src/api",
      "role": "mode:review",
      "status": "Thinking"
    },
    {
      "agent_id": "2",
      "pane_id": "2",
      "label": "api",
      "cwd": "/home/you/src/api",
      "status": "Working",
      "active_tool": { "name": "Bash" }
    }
  ]
}
```

`schema_version` is the contract. It is bumped when a field is **removed** or changes meaning; new fields can appear without a bump, so parse tolerantly and ignore keys you do not recognise. Version `2` removed `active_tool.detail`, which version `1` carried — a script written against v1 that read the tool's arguments gets nothing under v2 rather than something subtly different.

Every field except `agent_id` is optional and is **omitted entirely** when the daemon has no value for it, which is what the table renders as `-`. In the document above, agent `2` has no `role` key and agent `1` has no `active_tool` key. Read them with a fallback rather than assuming they are present:

```bash
dot-agent-deck daemon status --json | jq --raw-output '.agents[] | "\(.pane_id)\t\(.status // "unknown")\t\(.active_tool.name // "-")"'
```

```text
1	Thinking	-
2	Working	Bash
```

With no agents managed the document is `{"schema_version":2,"agents":[]}` and the exit code is still 0, so a filter like the one above simply produces no lines.

Both forms deliberately omit your prompt text and the tool's arguments. A pane whose card reads `Bash — cargo test --package billing -- --nocapture` in the TUI appears as the bare tool name `Bash` in the table and as `{"name": "Bash"}` in the JSON. A status snapshot is routinely pasted into a bug report or run in a terminal someone else is watching, so it carries diagnostics and nothing private.

### When the daemon is unreachable

If the query gets no answer, the command writes a one-line reason to **stderr**, prints nothing to stdout, and exits **1**. Unlike `daemon stop`, this is not treated as a benign no-op: a status query that got no answer learned nothing about the daemon, so reporting success would be a lie.

```text
# no daemon has ever run for this user
daemon status: unavailable (I/O error talking to daemon: No such file or directory (os error 2))

# a daemon ran and has since exited, leaving its socket behind
daemon status: unavailable (I/O error talking to daemon: Connection refused (os error 111))

# a daemon is listening but wedged
daemon status: unavailable (no response within 3s)
```

The whole round trip is bounded at 3 seconds. On expiry the command **abandons** the query rather than retrying — a retry loop would add load to the exact daemon you are trying to diagnose.

Exit 1 is reserved for "the daemon did not answer". A malformed invocation exits **2** instead, so a script can tell an unreachable daemon apart from a binary that is too old to understand the subcommand at all.

**Status inspection never starts a daemon.** Launching the TUI lazy-spawns one (see [How it runs](#how-it-runs) above); `daemon status` never does, in either output form. Spawning a daemon to answer "is a daemon running?" would make the question unanswerable, so an absent daemon is reported as the failure above and the socket is left exactly as it was found.

> Like `daemon stop`, this reports on the daemon on **this machine** only. Each remote attach has its own per-host daemon, covered in [Remote Environments](remote-environments.md).

## Recycling the local daemon

`dot-agent-deck daemon stop` shuts down the running daemon gracefully. Use it after a binary upgrade or any time you want to start a fresh daemon process.

```bash
dot-agent-deck daemon stop
```

- **Idempotent.** If no daemon is running, the command prints `no daemon running` and exits 0.
- **Data-loss guard.** If managed agents are still alive, the command refuses with a list of agent IDs and exits non-zero — terminating the daemon would kill their PTYs. Detach the agents first (close their panes, or quit the TUI to detach the deck while keeping the agents running), or pass `--force`. Run [`daemon status`](#inspecting-the-local-daemon) first if you want to see what is running before you decide.
- **Grace window.** Sends `SIGTERM` and polls for the socket to disappear for up to 5 seconds. With `--force`, escalates to `SIGKILL` after that window. A `SIGTERM` timeout without `--force` exits non-zero so you can re-run with `--force` consciously.

```bash
# Force shutdown even when managed agents are running. This kills the agents.
dot-agent-deck daemon stop --force
```

`dot-agent-deck daemon restart` is a thin wrapper: it runs `daemon stop`, then returns. The next `dot-agent-deck` invocation lazy-spawns a fresh daemon (see [How it runs](#how-it-runs) above). `--force` works the same way.

```bash
dot-agent-deck daemon restart
```

> Stopping a *remote* daemon works differently — each remote attach has its own per-host daemon, governed by the lifecycle in [Remote Environments](remote-environments.md). The local `daemon stop` only touches the daemon on this machine.
