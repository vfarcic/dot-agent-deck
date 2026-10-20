# Run the Daemon as a Service

Normally the daemon starts when you open the deck and stops about 30 seconds after the last client disconnects, if no agent is running and no schedule is enabled ([How it runs](getting-started.md#how-it-runs)). Nothing starts it after a reboot or a crash. That is fine while you are at the machine, but not when it has to be up on its own: a [schedule](scheduled-tasks.md) that should fire at 09:00 after an overnight reboot, a [remote host](remote-environments.md) you connect to from your laptop, or a daemon the desktop app should find already running.

`dot-agent-deck daemon install` sets the daemon up as a user service: a systemd user unit on Linux, a LaunchAgent on macOS. The service manager then starts the daemon when you log in, or at boot on Linux with lingering on, and starts it again after a crash or an upgrade. The TUI, the desktop app and `connect` find that daemon the way they find any other, with no extra setup. Setting the service up is a shell command on that machine; neither the TUI nor the desktop app has a control for it.

## Install

Run it on the machine the daemon should run on, as the user the agents should run as:

```bash
dot-agent-deck daemon install
```

On Linux it prints something like:

```
wrote /home/you/.config/systemd/user/dot-agent-deck.service (runs /home/you/.local/bin/dot-agent-deck daemon serve)
the daemon is running as dot-agent-deck.service
```

The second line appears only once the daemon answers, so when the command exits 0 the TUI, the desktop app and `connect` can reach it. The service runs the deck at the path shown in the first line. If you later install the deck somewhere else, for example by moving from Homebrew to the install script, run `daemon install` again.

**Check:** `dot-agent-deck daemon status` lists the daemon's agents, and `systemctl --user status dot-agent-deck.service` shows `active (running)`. On macOS, `launchctl print gui/$(id -u)/ai.devopstoolkit.dot-agent-deck` shows the job.

Running `daemon install` again is safe. It rewrites the unit or plist and leaves a running service running; the new settings apply from the service's next start.

**A daemon is already running.** If a daemon you started some other way is running (the TUI starts one, so does `connect`), `daemon install` sets the service up but does not start it, because a second daemon cannot run beside the first. The output says so. The service starts at your next login; to switch now, stop the running daemon with `dot-agent-deck daemon stop` (it refuses while agents are running, so close them first, or pass `--force` to stop them), then run `systemctl --user start dot-agent-deck.service`, or on macOS the `launchctl bootstrap` line the output prints.

## Start at boot (Linux)

A systemd user service starts when you log in. To start it at boot without a login, which is what an unattended machine or a remote host needs, turn on lingering once:

```bash
sudo loginctl enable-linger "$USER"
```

`daemon install` prints this line when lingering is off. It does not run `sudo` itself.

## What the service changes

- **The idle shutdown is off.** The service's daemon keeps running with no agents and no clients.
- **A crash or a kill starts it again**, about two seconds later on Linux. The agents the daemon was running are gone, as after any daemon restart; the next `connect` recreates a saved remote workspace.
- **A remote upgrade restarts it on the new release.** After `remote upgrade`, or the desktop app's **Upgrade** of a remote host, the service manager starts the new daemon. There is nothing to do.
- **A local upgrade leaves the service stopped.** When the TUI or the desktop app restarts the daemon on this machine onto a new build (after you upgrade the CLI, for example), the daemon that comes back is the app's, not the service's. Once no agents need it, run `dot-agent-deck daemon stop`, then `systemctl --user start dot-agent-deck.service` (or log out and in) to hand the daemon back to the service.
- **`daemon stop` and the TUI's Stop stop it until you start it again.** The service does not undo a deliberate stop. It starts again at your next login (or boot, with lingering), or with `systemctl --user start dot-agent-deck.service`. If you open the TUI, or press **Start daemon** in the desktop app, in the meantime, that starts a daemon of its own, as it does without a service; stop that one before you start the service again.
- **Where it runs.** The service starts in your home directory, so it reads the same `~/.dot-agent-deck.toml` as a daemon started from a shell there. If `DOT_AGENT_DECK_SOCKET`, `DOT_AGENT_DECK_ATTACH_SOCKET`, `DOT_AGENT_DECK_STATE_DIR`, `XDG_CONFIG_HOME` or `XDG_STATE_HOME` is set in the shell that runs `daemon install`, the service gets the same values, so it listens where that shell's clients look and reads the same configuration and schedules. Set them as absolute paths; `daemon install` refuses a relative one.

## Credentials and other environment

A service does not read your shell profile. The daemon takes `PATH` from your login shell when it starts, and nothing else. Anything else the agents need from the environment, such as an `ANTHROPIC_API_KEY`, has to be given to the service. An agent that logs in through its own flow (Claude Code's `/login`, for example) needs nothing here. `daemon install` never writes credentials anywhere.

**Linux:** add them in a drop-in, which `daemon install` does not overwrite:

```bash
systemctl --user edit dot-agent-deck.service
```

```ini
[Service]
Environment=ANTHROPIC_API_KEY=sk-ant-...
```

Or keep them in a file only you can read (`chmod 600`) and point at it with `EnvironmentFile=/home/you/.config/dot-agent-deck/daemon.env`. Then run `systemctl --user restart dot-agent-deck.service`, which stops the daemon's agents.

**macOS:** add them under `EnvironmentVariables` in `~/Library/LaunchAgents/ai.devopstoolkit.dot-agent-deck.plist`. Running `daemon install` again rewrites that file, so add them again after a reinstall.

## macOS

The service is a LaunchAgent, `~/Library/LaunchAgents/ai.devopstoolkit.dot-agent-deck.plist`, and its output goes to `~/Library/Logs/dot-agent-deck.log`. The macOS service has not been tested on a Mac yet. If it does not behave as this page describes, the `launchctl print` line above and that log are where to look, and please report it.

- **It starts at login, not at boot.** A LaunchAgent runs inside your user session, which is what gives the agents your environment and credentials. An unattended Mac therefore needs automatic login to come back after a reboot, and FileVault asks for a password at startup before any automatic login. Decide that trade-off deliberately.
- **Over ssh**, `daemon install` can write the plist but may not be able to load it while nobody is logged in at the Mac. It then loads at the next login.

## Remove the service

```bash
dot-agent-deck daemon uninstall
```

It removes the unit or plist, so the daemon no longer starts at login or boot. It does not stop a daemon the service is running, or its agents. The output says when one is still running. Until that daemon stops, the service manager still starts it again if it crashes; `dot-agent-deck daemon stop` stops it for good. A drop-in you added with `systemctl --user edit` stays in `~/.config/systemd/user/dot-agent-deck.service.d/`; delete it yourself.

## When it goes wrong

- **`daemon install` says no daemon answered in time.** The service started, but the daemon did not come up where this shell looks. Run `systemctl --user status dot-agent-deck.service` and `journalctl --user -u dot-agent-deck.service`. If the service is running, the shell and the service disagree about where the sockets are: compare `echo $XDG_RUNTIME_DIR` in your shell with the service's (normally `/run/user/<your uid>`), and see [Where the deck puts its files](remote-requirements.md#where-the-deck-puts-its-files).
- **`daemon install` says systemd loads `dot-agent-deck.service` from another file.** Either an older unit with that name sits elsewhere in systemd's search path (remove it), or your shell sets `XDG_CONFIG_HOME` and the systemd user manager does not, so systemd never reads the directory the unit was written to. Run `daemon install` from a shell without that `XDG_CONFIG_HOME`, or set it for the manager too.
- **`systemctl --user` fails with `Failed to connect to bus`.** No systemd user session is running for this login, as in some `su` and container shells. Log in as the user directly (ssh or a desktop session) and run `daemon install` there.
- **The service keeps failing with `daemon already running`.** Another daemon holds the sockets, usually one the TUI started while the service was stopped. Stop it with `dot-agent-deck daemon stop`, then `systemctl --user restart dot-agent-deck.service`.
- **Windows.** `daemon install` supports Linux and macOS. In WSL it works when systemd is enabled for the distribution (`systemd=true` under `[boot]` in `/etc/wsl.conf`); without systemd, `systemctl --user` fails and `daemon install` reports that error.
