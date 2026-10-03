# Installation

dot-agent-deck is one binary, `dot-agent-deck`, which is the TUI, the background daemon and the CLI. The [desktop app](desktop/index.md) is a separate, optional download and a second client of the same daemon. Install the binary first; the desktop app connects to a daemon but does not start one (see [How the desktop app gets a daemon](#how-the-desktop-app-gets-a-daemon)).

After installing, `dot-agent-deck docs` lists the documentation built into the binary, and `dot-agent-deck docs <topic>` prints one page. That copy always matches the installed version, so prefer it over the website when the two might differ. If `dot-agent-deck docs` reports an unrecognized subcommand, the installed version predates it: read the documentation at [agent-deck.devopstoolkit.ai/llms.txt](https://agent-deck.devopstoolkit.ai/llms.txt) instead, keeping in mind that the website follows the latest release rather than your installed version.

## Platform Support

| Platform | Binary (TUI, daemon, CLI) | Desktop app (alpha) |
|---|---|---|
| Linux amd64 | Yes | Yes (`.deb`, for Debian, Ubuntu and other `apt`-based distributions) |
| Linux arm64 | Yes | No |
| macOS Apple silicon | Yes | Yes (`.dmg`) |
| macOS Intel | Yes | No |
| Windows via WSL | Yes, install the Linux binary inside WSL | No |
| Windows native | No ([#164](https://github.com/vfarcic/dot-agent-deck/issues/164)) | No |

## Choose an install method

| Method | Use it when |
|---|---|
| [Homebrew](#homebrew-macos--linux) | macOS or Linux, and `brew` is already installed |
| [Download a binary](#download-binary) | No package manager, or you want a specific release file |
| [Nix](#nix) | Nix with flakes, NixOS or home-manager (not Intel macOS) |
| [Build from source](#build-from-source) | Contributing, or a platform with no published binary |

Every method installs the same `dot-agent-deck` binary. Then [check the install](#verify) and read [Agent hooks](#agent-hooks).

## Homebrew (macOS / Linux)

```bash
brew tap vfarcic/tap
brew install dot-agent-deck
```

Pre-releases are published as a separate formula, `vfarcic/tap/dot-agent-deck-beta`. The two formulas conflict with each other, so uninstall one before installing the other.

## Download Binary

Release assets are named `dot-agent-deck-<os>-<arch>`:

| Platform | Asset |
|---|---|
| Linux amd64 | `dot-agent-deck-linux-amd64` |
| Linux arm64 | `dot-agent-deck-linux-arm64` |
| macOS Intel | `dot-agent-deck-darwin-amd64` |
| macOS Apple silicon | `dot-agent-deck-darwin-arm64` |

Download the one for your platform from the latest release, make it executable and put it on your `PATH` as `dot-agent-deck`:

```bash
ASSET=dot-agent-deck-linux-amd64   # pick from the table above
mkdir -p ~/.local/bin
curl -fsSL -o ~/.local/bin/dot-agent-deck \
  "https://github.com/vfarcic/dot-agent-deck/releases/latest/download/$ASSET"
chmod +x ~/.local/bin/dot-agent-deck
```

- If `dot-agent-deck --version` then reports `command not found`, `~/.local/bin` is not on your `PATH`. Add `export PATH="$HOME/.local/bin:$PATH"` to your shell's rc file and open a new shell.
- On macOS, if the binary is refused because it was downloaded from the internet, run `xattr -d com.apple.quarantine ~/.local/bin/dot-agent-deck`.
- For a specific release, replace `latest/download` with `download/<tag>`, for example `download/v0.44.0`.

**Optional: check where the file came from.** The release binaries, and the `checksums.txt` manifest beside them, carry build provenance from this repository's release workflow. With the [GitHub CLI](https://cli.github.com/) installed:

```bash
gh attestation verify ~/.local/bin/dot-agent-deck \
  --repo vfarcic/dot-agent-deck \
  --signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml
```

It should report a verified attestation. If it does not, delete the file and download it again from the release page.

## Nix

Requires Nix with flakes enabled (`nix-command` and `flakes` in `experimental-features`). The flake builds from source and supports `x86_64-linux`, `aarch64-linux` and `aarch64-darwin`. It has no `x86_64-darwin` (Intel Mac) build; use Homebrew or a downloaded binary there. `dot-agent-deck --version` reports the released version the flake pins.

Run it once without installing:

```bash
nix run github:vfarcic/dot-agent-deck
nix run github:vfarcic/dot-agent-deck -- hooks install   # arguments after -- reach the binary
```

Install it into your user profile:

```bash
nix profile install github:vfarcic/dot-agent-deck
```

To pin a release, append its tag: `github:vfarcic/dot-agent-deck/<tag>`. Tags from before the flake was added have no flake and fail to build.

**As a flake input** (NixOS or home-manager):

```nix
{
  inputs.dot-agent-deck.url = "github:vfarcic/dot-agent-deck";
  # Optional: build against your nixpkgs instead of the one this flake pins.
  # Your nixpkgs must then carry rustc 1.97.1 or newer.
  inputs.dot-agent-deck.inputs.nixpkgs.follows = "nixpkgs";

  # NixOS
  environment.systemPackages = [ inputs.dot-agent-deck.packages.${pkgs.system}.default ];

  # home-manager
  home.packages = [ inputs.dot-agent-deck.packages.${pkgs.system}.default ];
}
```

**Via the overlay**, to reach it as `pkgs.dot-agent-deck`. The overlay builds against your nixpkgs, so it needs rustc 1.97.1 or newer there; an older rustc makes cargo stop and name the version it needs.

```nix
{
  nixpkgs.overlays = [ inputs.dot-agent-deck.overlays.default ];
  environment.systemPackages = [ pkgs.dot-agent-deck ];
}
```

### The home-manager module

`homeModules.default` installs the package and can write `~/.config/dot-agent-deck/config.toml` and `~/.config/dot-agent-deck/keybindings.toml`:

```nix
{
  imports = [ inputs.dot-agent-deck.homeModules.default ];

  programs.dot-agent-deck = {
    enable = true;

    # Rendered to ~/.config/dot-agent-deck/config.toml
    settings = {
      default_command = "claude";
      bell.on_idle = true;
    };

    # Rendered to ~/.config/dot-agent-deck/keybindings.toml
    keybindings = {
      global = {
        toggle_layout = "Alt+Shift+l";
        new_pane = "";            # an empty string unbinds the action
      };
      dashboard.help = "F1";
    };
  };
}
```

| Option | Type | Default | Effect |
|---|---|---|---|
| `enable` | bool | `false` | Installs the package. |
| `package` | package | `pkgs.dot-agent-deck` when the overlay is applied, otherwise this flake's package built against your nixpkgs | The package to install. Set it to `inputs.dot-agent-deck.packages.${pkgs.system}.default` to build against the flake's pinned nixpkgs. |
| `settings` | TOML attribute set | `{ }` | Content of `config.toml`. Keys: see [Configuration](configuration.md). No file is written while it is empty. |
| `keybindings` | TOML attribute set | `{ }` | Content of `keybindings.toml`. Actions and key notation: see [Keyboard Shortcuts](keyboard-shortcuts.md#customizing-keybindings). No file is written while it is empty. |

home-manager links these files from the Nix store, so change `settings` and `keybindings` rather than editing the files or running `dot-agent-deck config set`. It does not manage `session.toml`, `remotes.toml` or `schedules.toml`, which the deck and its CLI write themselves. It does not run `dot-agent-deck hooks install`; run that once yourself after the first activation (see [Agent hooks](#agent-hooks)).

`nix develop` gives a shell with the Rust toolchain only. To work on this repository, use the repository's devbox environment instead.

## Build from Source

Requires Rust 1.97.1 or newer.

```bash
git clone https://github.com/vfarcic/dot-agent-deck.git
cd dot-agent-deck
cargo build --release --locked
```

The binary is `target/release/dot-agent-deck`. Copy it onto your `PATH` rather than running it from `target/`, because agent hooks record the binary's path. A source build's `--version` reports a version derived from `git describe`.

## Verify

```bash
dot-agent-deck --version    # prints: dot-agent-deck <version>
dot-agent-deck --help       # lists the subcommands
dot-agent-deck docs         # lists the embedded documentation topics
```

If `--version` prints the version you installed, the binary works. If another copy is found first on your `PATH`, `command -v dot-agent-deck` shows which one runs.

## Agent hooks

The deck learns each agent's status (Thinking, Working, Needs Input, and so on) from hooks or plugins installed into that agent's own configuration. Whenever the TUI or the daemon starts, it installs them for every agent it detects:

| Agent | Installed when | What is written |
|---|---|---|
| Claude Code | `~/.claude` exists | Hook entries in `~/.claude/settings.json`. The `StopFailure` hook (used for **Error** and **Blocked**) only when `claude --version` reports 2.1.78 or newer. |
| OpenCode | `$XDG_CONFIG_HOME/opencode` (default `~/.config/opencode`) or `~/.opencode` exists | The plugin file `plugin/dot-agent-deck.js` under that directory. |
| Codex | `codex` is on the daemon's `PATH` | Hooks in `$CODEX_HOME/hooks.json` (default `~/.codex`), and trust for those hooks. |
| Devin | `devin` is on the daemon's `PATH` | Hook entries in `$XDG_CONFIG_HOME/devin/config.json` (default `~/.config/devin/config.json`). |
| Pi | Every time the deck starts a Pi pane | A bundled extension; nothing to install. |

The hooks call the installed binary by its absolute path, so moving or deleting the binary breaks them until you reinstall them.

To install or reinstall by hand (for example, after installing an agent for the first time, or after moving the binary):

```bash
dot-agent-deck hooks install                    # Claude Code (the default)
dot-agent-deck hooks install --agent opencode
dot-agent-deck hooks install --agent codex
dot-agent-deck hooks install --agent devin
```

`--agent` accepts `claude-code` (default), `opencode`, `codex` and `devin`; Pi has no hooks to install. Unlike the automatic install, these commands write the configuration even when the agent's directory does not exist yet. On success they print what they installed, for example `Installed hooks: SessionStart, SessionEnd, …` and `Settings file: /home/you/.claude/settings.json` for Claude Code, or `Trusted hooks: <n>` for Codex, followed by a note naming any of the deck's hooks you have turned off in Codex's `/hooks` list ([Codex events not showing](troubleshooting.md#codex-events-not-showing)). On failure they print `Failed to install <agent> hooks: <reason>` and exit non-zero. `dot-agent-deck hooks uninstall --agent <agent>` removes them.

An agent that was already running when the hooks were installed may need a restart to load them. If a card stays on its first status while the agent works, see [Troubleshooting → Hooks](troubleshooting.md#hooks).

## Desktop app

The desktop app is published alongside the CLI in releases, as an **alpha**: its assets are named `dot-agent-deck-desktop-alpha-*` and are not covered by the CLI's support expectations. What it does and lacks compared with the TUI is on [Desktop app](desktop/index.md).

| Platform | Asset | Signed |
|---|---|---|
| macOS Apple silicon | `dot-agent-deck-desktop-alpha-macos-arm64.dmg` | Signed and notarized from v0.42.0, unless that release's notes say otherwise |
| Linux amd64 | `dot-agent-deck-desktop-alpha-linux-amd64.deb` | Unsigned |

Download from the [latest release](https://github.com/vfarcic/dot-agent-deck/releases/latest) and read that release's notes: when the release has a `.dmg`, they say whether it is signed. A release the project built without signing ships an unsigned `.dmg`, and its notes say so. If the macOS package fails to build or to sign, the release ships no `.dmg` at all rather than an unsigned one. A release can also ship without one or both desktop packages while its CLI binaries are published as usual, so check the assets on the release you open.

### Verify the download

Check the file's build provenance before installing it. For the unsigned `.deb` it is the only check. It needs the [GitHub CLI](https://cli.github.com/):

```bash
gh attestation verify dot-agent-deck-desktop-alpha-macos-arm64.dmg \
  --repo vfarcic/dot-agent-deck \
  --signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml
```

Use the name of the file you downloaded. A pass reports a verified attestation from this repository's release workflow. If it does not, do not install the file and do not override any warning your OS raises about it.

The same check works on every file a release uploads: the CLI binaries, the desktop packages, and both checksum manifests, `checksums.txt` and `checksums-desktop-alpha.txt`. It does not cover GitHub's own **Source code** archives, which GitHub generates from the tag rather than the release workflow uploading them.

### macOS

1. Open the `.dmg` and drag **Agent Deck** to **Applications**.
2. Launch **Agent Deck** from Applications. For a signed release, macOS asks only to confirm opening an app downloaded from the internet.

If macOS reports the app as damaged or from an unidentified developer:

- The release notes say the `.dmg` is signed and notarized: do not override the warning; [report it](https://github.com/vfarcic/dot-agent-deck/issues).
- The release notes say the `.dmg` is unsigned: the warning is expected. Once the file has passed [the provenance check](#verify-the-download), follow the workaround in those notes.

The app carries its own copy of the binary at `/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck` and does not put it on your `PATH`. To use `dot-agent-deck` in a terminal, install the CLI too, at the same version as the app.

### Linux

```bash
curl -fsSL -O \
  https://github.com/vfarcic/dot-agent-deck/releases/latest/download/dot-agent-deck-desktop-alpha-linux-amd64.deb
# verify it as above, then:
sudo apt install ./dot-agent-deck-desktop-alpha-linux-amd64.deb
```

`apt` installs the dependencies (`libwebkit2gtk-4.1-0`, `libgtk-3-0`); `sudo dpkg -i` does not. The package is named `agent-deck`. It adds **Agent Deck** to the application menu and installs `/usr/bin/dot-agent-deck-desktop` (the app) and `/usr/bin/dot-agent-deck` (the same binary as the CLI downloads). If another `dot-agent-deck` comes earlier on your `PATH`, `command -v dot-agent-deck` shows which one runs.

Launch it from the application menu or with `dot-agent-deck-desktop`. Remove it with `sudo apt remove agent-deck`.

### How the desktop app gets a daemon

The desktop app connects to a daemon and does not start one. With no daemon running, its Dashboard shows **Daemon disconnected** with a **Reconnect** button. (Starting a daemon from inside the app is one of the [features behind the `experimental` flag](desktop/index.md#features-behind-the-experimental-flag).) Start a daemon, then press **Reconnect**:

- **Run the TUI**: `dot-agent-deck` starts a daemon if none is running. Both clients can be open at once.
- **Run the daemon alone**: `dot-agent-deck daemon serve` runs it in the foreground of that terminal until `Ctrl+C`. On macOS without a CLI install: `"/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck" daemon serve`.

A daemon with no clients, no agents and no enabled [schedules](scheduled-tasks.md) exits after about 30 seconds, and a daemon that has just started counts as having no clients. So:

- A `daemon serve` that nothing connects to within 30 seconds exits. Connect promptly, or run `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0 dot-agent-deck daemon serve` to keep it up until it is stopped.
- While the desktop app is connected, or any agent is running, the daemon stays up.
- Quitting the TUI with **Detach** leaves the daemon running under the same rule. Quitting it with **Stop** shuts the daemon down, and the desktop app then shows **Daemon disconnected**.
- Quitting the desktop app leaves agents running.

The app looks for the daemon at the same default socket as the TUI. If you set `DOT_AGENT_DECK_ATTACH_SOCKET` for the TUI, set it in the app's environment too. A **remote** daemon must already be running on its host; see [Desktop app → Daemons](desktop/daemons.md#what-a-remote-daemon-must-already-have).

### Keep the app and the daemon on the same release

When it connects, the app checks whether it and the daemon can work together:

| Situation | What the Dashboard shows | What to do |
|---|---|---|
| The two are compatible | Connects normally | Nothing |
| One of them is older, and the app could misread some of what the daemon reports | **Incompatible daemon**, saying which of the two is older and that the app has not connected, with **Connect anyway** | Update the older one. **Connect anyway** connects until you quit the app, but some of what the daemon shows may be wrong. |
| The two cannot work together | **Incompatible daemon**, saying which of the two is older, without Connect anyway | Update the older one, then restart the daemon with the matching binary: `dot-agent-deck daemon restart`, then start it again with the TUI or `daemon serve` |

**Technical details** under the message shows the exact versions on each side, which is what to include in a bug report.

`daemon restart` refuses while agents or orchestration roles are live; see [Recycling the local daemon](#recycling-the-local-daemon). Upgrade the CLI and the desktop app together to avoid all of this.

## How it runs

The first `dot-agent-deck` run starts a per-user background daemon and connects to it over a Unix socket: `$XDG_RUNTIME_DIR/dot-agent-deck-attach.sock` when `XDG_RUNTIME_DIR` is set, otherwise a per-user directory under the system temp directory. `DOT_AGENT_DECK_ATTACH_SOCKET` overrides the path. The same daemon serves local runs; a [remote](remote-environments.md) host runs its own.

The daemon owns the agents. Quitting the TUI with **Detach** leaves them running, and the next `dot-agent-deck` shows them again. About 30 seconds after the last client disconnects, if no agent is running and no enabled schedule is registered, the daemon exits. `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` sets that window in seconds; `0` disables it.

## Upgrading

Upgrade with the method you installed with (`brew upgrade dot-agent-deck`, a new download over the old file, `nix profile upgrade` on your profile entry, or a new build), then relaunch:

```bash
dot-agent-deck
```

On launch the TUI compares its build with the running daemon's. If they differ:

- **No agents running**: the old daemon is restarted on the new binary without asking.
- **Agents running, TUI in a terminal**: the TUI prints `Daemon version mismatch`, the two builds, and the agents a restart would stop. Press `S` to restart the daemon (stopping those agents), or any other key to keep the current daemon and attach to it with your agents intact. Upgrade later, when no agents are running.
- **Agents running, protocol changed**: the prompt says this binary cannot attach. `S` restarts as above. Any other key exits with `error: daemon speaks attach protocol vN, but this binary speaks vM` and leaves the daemon and its agents running. To keep working with them, run the binary version the daemon came from (the message names it). To move to the new binary, stop the daemon when you are ready to lose those agents (see [Recycling the local daemon](#recycling-the-local-daemon)).
- **Agents running, TUI not attached to a terminal** (a script or CI): it cannot ask, so it prints a recovery hint to stderr and exits non-zero. Run `dot-agent-deck daemon stop` first, then relaunch.

If you keep an older daemon, features added by the newer release may not work against it; see [Troubleshooting → Delegate prompts silently no-op after staying on an older daemon](troubleshooting.md#delegate-prompts-silently-no-op-after-staying-on-an-older-daemon).

If the upgrade moved the binary to a new path (for example, you switched from a download to Homebrew), run `dot-agent-deck hooks install` for each agent you use so the hooks point at the new path. For the desktop app, install the new release's package the same way as the first time.

## Versioning

While the version is `0.x`:

- A change after which an older and a newer build can no longer safely work together bumps the **minor** digit (`0.31.x` → `0.32.0`).
- Features and fixes bump the **patch** digit (`0.31.1` → `0.31.2`).

A minor bump is the cue to upgrade the daemon, the TUI and the desktop app together, and to run `dot-agent-deck remote upgrade` for each [remote](remote-environments.md). Builds that differ only in the patch digit work together.

## Inspecting the local daemon

`dot-agent-deck daemon status` prints what the local daemon is managing. It is read-only: it does not start a daemon, and it changes nothing.

```bash
dot-agent-deck daemon status
```

```text
PANE	AGENT	ROLE	STATUS	TOOL	LABEL	CWD
1	1	lead (orchestrator)	Thinking	-	api	/home/you/src/api
2	2	-	Working	Bash	api	/home/you/src/api
```

The columns are tab-separated (`column -t -s $'\t'` aligns them). `-` means no value.

| Column | Content |
|---|---|
| `PANE` | Pane id; a managed agent sees it as `DOT_AGENT_DECK_PANE_ID`. |
| `AGENT` | The daemon's id for the agent. |
| `ROLE` | The role name for an [orchestration](orchestration.md) pane (with `(orchestrator)` on the start role), `mode:<name>` for an agent still running from a workspace mode started by a release before 0.44.0, `-` otherwise. |
| `STATUS` | `Thinking`, `Working`, `Compacting`, `WaitingForInput`, `Idle`, `Error` or `Blocked`. See [Session statuses](session-management.md#session-statuses). |
| `TOOL` | The name of the tool running now, without its arguments. |
| `LABEL` | The pane's display name. |
| `CWD` | The directory the agent was started in. |

With no agents it prints `no managed agents` and exits 0.

### JSON for scripts

Parse `--json`, not the table. It prints one line; reformatted here:

```bash
dot-agent-deck daemon status --json
```

```json
{
  "schema_version": 2,
  "agents": [
    { "agent_id": "1", "pane_id": "1", "label": "api", "cwd": "/home/you/src/api", "role": "lead (orchestrator)", "status": "Thinking" },
    { "agent_id": "2", "pane_id": "2", "label": "api", "cwd": "/home/you/src/api", "status": "Working", "active_tool": { "name": "Bash" } }
  ]
}
```

- `schema_version` increases when a field is removed or changes meaning. New fields can appear without an increase, so ignore keys you do not recognise.
- Every field except `agent_id` is omitted when it has no value, so read with a fallback, for example `jq -r '.agents[] | "\(.pane_id)\t\(.status // "unknown")\t\(.active_tool.name // "-")"'`.
- With no agents the document is `{"schema_version":2,"agents":[]}` and the exit code is 0.
- Neither form includes prompt text or tool arguments.

### When the daemon is unreachable

The command writes one line to stderr, nothing to stdout, and exits **1**:

```text
daemon status: unavailable (I/O error talking to daemon: No such file or directory (os error 2))   # no daemon has run
daemon status: unavailable (I/O error talking to daemon: Connection refused (os error 111))        # daemon exited, socket left behind
daemon status: unavailable (no response within 3s)                                                # daemon is not answering
```

It waits at most 3 seconds and does not retry. Exit code **2** means the invocation itself was malformed (for example, a binary too old to know `daemon status`). This command covers only the daemon on this machine; each remote has its own (see [Remote Environments](remote-environments.md)).

## Recycling the local daemon

```bash
dot-agent-deck daemon stop
```

- **No daemon running**: prints `no daemon running` and exits 0.
- **Managed agents running**: refuses, lists their ids and exits non-zero, because stopping the daemon stops them. Detach or close them first, or pass `--force`.
- **Orchestration roles held**: if the daemon holds [orchestration](orchestration.md) roles whose panes still have a live agent, it refuses and lists each pane id, role and orchestration. The daemon keeps role registrations in memory only, so after a forced stop an agent that survives keeps running but can no longer delegate; its card is marked `orphaned` (see [Session statuses](session-management.md#diagnostic-markers-on-a-card)). Let the orchestration finish, or pass `--force` accepting that.
- **Shutdown**: sends `SIGTERM` and waits up to 5 seconds for the daemon to stop. If it has not, the command exits non-zero; with `--force` it sends `SIGKILL` instead.

```bash
dot-agent-deck daemon stop --force   # stops managed agents and strands live orchestrations
```

`dot-agent-deck daemon restart` (also `--force`) runs `daemon stop`. The next `dot-agent-deck` starts a fresh daemon. Both commands act only on this machine's daemon; remotes are covered in [Remote Environments](remote-environments.md).

## Uninstall

```bash
dot-agent-deck daemon stop                     # add --force if it refuses and you accept losing the agents
for agent in claude-code opencode codex devin; do
  dot-agent-deck hooks uninstall --agent "$agent"
done
brew uninstall dot-agent-deck                  # Homebrew; or delete the downloaded binary, or `nix profile remove`
sudo apt remove agent-deck                     # desktop app on Linux; on macOS delete /Applications/Agent Deck.app
```

Your settings, keybindings, saved workspace and registered remotes are in `~/.config/dot-agent-deck/`, and schedules are in `$XDG_CONFIG_HOME/dot-agent-deck/schedules.toml` when `XDG_CONFIG_HOME` is set (otherwise the same directory). Delete them to remove your configuration too.
