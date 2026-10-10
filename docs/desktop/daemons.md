# Daemons

The desktop app can watch several daemons at once: the one on this machine and any number of remote ones reached over ssh. The TUI attaches to one daemon at a time and reaches a remote one with `dot-agent-deck connect` (see [Remote Environments](../remote-environments.md)).

The app shares its list of remote daemons with the CLI: both read and write `remotes.toml` in `~/.config/dot-agent-deck/` (or the file `DOT_AGENT_DECK_REMOTES` names). So a host registered with `dot-agent-deck remote add` is already in the app's list, and a daemon added in the app is listed by `dot-agent-deck remote list`. The app shows only `ssh` entries.

## Choose which daemons the Dashboard shows

The **Daemon** selector under the Dashboard's title chooses what the Dashboard shows, and the choice is saved:

- **All daemons**: every daemon at once, one section each. The **DAEMONS** counter then says how many of them answered.
- **This machine**: the daemon on this computer, at the same default address the TUI uses. This is the default. If you point the TUI at another socket with `DOT_AGENT_DECK_ATTACH_SOCKET`, launch the app with the same variable.
- One entry per remote daemon, called by its name: the same name `dot-agent-deck connect <name>` takes. A daemon with no usable name is called by its user, host and port (the port only when it is not 22).

The selector only chooses. Daemons are added and removed in **Settings → Daemons**.

## Watch a daemon on another machine

The app reaches a remote daemon through an ssh tunnel, using the `ssh` program on this computer and your ssh config. It does not install the deck on a new host, so do that first. Once the deck is installed there, the app can [start its daemon](#start-a-daemon-from-the-app) and [upgrade it](#upgrade-a-remote-daemon).

1. **Make ssh to the host work without a prompt, with its host key already accepted.** The app cannot answer ssh's questions, so a password prompt fails the connection. So does a host key this computer has not accepted yet, even when your ssh config would accept a new one on its own: the app connects only to a host whose key this computer has already accepted, both to watch its daemon and to start one. Check from a terminal: connect once with `ssh <user>@<host>` and accept the host key if asked, then `ssh -o BatchMode=yes -o StrictHostKeyChecking=yes <user>@<host> true` must exit 0 without asking anything.
2. **Install the deck on the host.** See [What a remote daemon must already have](#what-a-remote-daemon-must-already-have).
3. **Add the daemon in the app.** Open **Settings** in the rail, choose **Daemons**, press **Add a daemon**, fill in the fields (below), check the **Deck name** the app suggests or type your own, and press **Add this daemon**. The daemon is added and chosen; from then on, changes to its fields are saved as you make them. If you registered the host with `dot-agent-deck remote add` while the app was open, restart the app to see it; it is then already listed.
4. **Press Test connection.** It checks the daemon and, when **Daemon socket** is empty, finds the socket path on the host and saves it.
5. **Choose the daemon** in the Dashboard's **Daemon** selector, or choose **All daemons**.

**Check it worked:** **Test connection** reports `<daemon> answered and is compatible with this app.`, and on the Dashboard the daemon's section lists its agents (or **No agents are running yet**) instead of a title such as **Daemon disconnected**.

![Settings → Daemons with a remote daemon named build, its address build-box beside the name, chosen in the Daemon row beside All daemons and This machine; below it the Deck name field reads build with a Rename button, Host is filled in, the other fields show their placeholders, and Test connection is not yet pressed](/img/settings-daemons-desktop.png)

A remote daemon has these fields:

| Field | What it is | Required |
| --- | --- | --- |
| **Deck name** | What the app calls the daemon, and the name `dot-agent-deck connect <name>` takes. The app suggests one from the host and user. | No, empty takes the suggested name |
| **Host** | The host name or address to ssh to. | Yes |
| **User** | The ssh user. Empty takes it from your ssh config. | No |
| **Port** | The ssh port, 1 to 65535. | No, `22` by default |
| **Key file** | The ssh identity file to offer (`ssh -i`), as a path starting with `/` or `~/`, for example `~/.ssh/id_ed25519`. Empty uses ssh's defaults. | No |
| **Jump host** | The name of a `Host` block in your `~/.ssh/config` to connect through (`ssh -J`). The jump host's own address, user and key stay in that config. | No |
| **Daemon socket** | The path of the daemon's attach socket on the host. Leave it empty: **Test connection** finds it and fills it in. | No |

A field whose value the app refuses (a character ssh would misread, a key path that is not absolute) shows the problem under it, and **Test connection** stays disabled until it is fixed. A **Deck name** the app cannot use, such as one another daemon already has, is refused with the reason under the field; a daemon you were adding stays on screen so you can pick another name.

## Test connection

**Test connection** checks the chosen daemon and says what it found in one sentence, with a command to run when there is one. A line under the sentence can carry the exact versions or the error behind it, which is what to include in a bug report. It works for **This machine** and for each remote daemon; **All daemons** has nothing to test, so choose one daemon first.

| Result | What to do |
| --- | --- |
| `<daemon> answered and is compatible with this app.` | Nothing: the daemon is ready. |
| `This daemon is older than this app. The app has not connected, because it could misread …` (or `This app is older than the daemon. …`) | Update the older of the two so both run the same version. For a remote daemon older than the app, the Dashboard offers **Upgrade** ([below](#upgrade-a-remote-daemon)). Until then the Dashboard offers **Connect anyway**, which uses the daemon as it is until you quit the app. |
| `This daemon is older than this app, and the two cannot work together.` (or `This app is older than the daemon, …`) | Update the older of the two so both run the same version; for a remote daemon older than the app, press **Upgrade** on the Dashboard ([below](#upgrade-a-remote-daemon)). Nothing overrides this. |
| `This daemon and this app are different versions. …` | Each has changes the other lacks, which usually means two development builds. Run the same version of both. |
| `The daemon turned this app away.` | Test again in a moment. If it keeps happening, restart the daemon on its host. |
| `The ssh connection to <daemon> works, but nothing is listening on its daemon socket over there.` | Start a daemon on the host (below), then test again. |
| `No daemon answered at <daemon>. Start Agent Deck on this machine, then test again.` | For **This machine**: start a daemon here, for example with `dot-agent-deck`. |
| `This machine has not verified <daemon>'s host key.` | Run the `ssh` command shown under it once in a terminal, accept the key, then test again. |
| `ssh reached <daemon> and the login was refused.` | Check **User** and **Key file**, and that the key is loaded in your ssh agent if it has a passphrase. |
| `ssh could not reach <daemon>.` | Check **Host**, **Port**, **Jump host** and your network. |
| `The ssh tunnel to <daemon> could not be established.` | Read the detail the test shows. `dot-agent-deck remote doctor <name>` diagnoses the ssh setup of a host registered with the CLI. |
| `<daemon> has no daemon socket path yet, and this test could not discover one.` | Check that `dot-agent-deck` is installed on the host, in `~/.local/bin`, on its `PATH` or in a Homebrew `bin` directory, and that a daemon is running there; or fill in **Daemon socket** yourself. |
| `No ssh program was found on this machine, so no remote daemon can be reached.` | Install an OpenSSH client. |

A test also lists what your ssh config adds to the tunnel (for example port forwards) and which files host keys are checked against, because the tunnel carries those for as long as it is open.

## What a remote daemon must already have

1. **The deck installed on the host.** From a machine with the CLI, `dot-agent-deck remote add <name> <user@host>` installs `dot-agent-deck` into `~/.local/bin` on the host, sets up the agent hooks, and registers the host in `remotes.toml`, so it then appears in the app too. [Remote Environment Requirements](../remote-requirements.md) and [Remote Recipes](../remote-recipes.md) cover what the host needs. For the app to start the daemon there, the host's login shell must be a POSIX shell such as `bash` or `zsh`.
2. **A daemon running on it, when you want one to stay up.** The app's [Start daemon](#start-a-daemon-from-the-app) starts one whenever you need it. A daemon exits about 30 seconds after its last client disconnects when it has no agents and no enabled schedules, so a daemon started and left alone does not stay up. To keep one up, either keep agents running on it (start them with `dot-agent-deck connect <name>` and detach, or from the desktop app while the daemon is up), or run it with the idle shutdown turned off, on the host:

   ```bash
   DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0 nohup ~/.local/bin/dot-agent-deck daemon serve >/dev/null 2>&1 &
   ```

   To have it come back after a logout, a crash or a reboot, run it under `systemd --user` or a macOS LaunchAgent as [Remote Environment Requirements](../remote-requirements.md#recommended-for-persistent-and-safe-use) describes.

While the desktop app is connected to a daemon, that connection counts as a client, so the daemon does not idle out under it.

## Start a daemon from the app

When a daemon is not running, the app starts it for you, on this machine or on a remote host. You do not need the `experimental` flag for this. The TUI has no button for it because it does this on its own: `dot-agent-deck` starts the daemon on this machine when none is running, and `dot-agent-deck connect <name>` starts the one on a remote host.

1. On the Dashboard, find the daemon's section. It says **Daemon disconnected**, with a sentence saying why.
2. Press the one button the section offers:
   - **Start daemon**, when no daemon is running there. The sentence reads `No daemon is running on this machine.`, or names the remote host, such as `No daemon is running on deploy@build-box:2222.`
   - **Reconnect**, when a daemon is running there but the app is not connected to it, or when the app cannot tell, for example because the host cannot be reached. The sentence says which.
3. For **Start daemon**, the app asks first and names the machine the daemon will start on. Press **Start daemon** in that dialog; the button reads **Starting…** until the daemon answers, and **Cancel** starts nothing.

**Check it worked:** the section lists the daemon's agents, or **No agents are running yet**, and the **DAEMONS** counter at the top counts it among the daemons that answered.

A remote daemon's section reads `Checking whether a daemon is running on <host>.` with **Reconnect** for a few seconds after it first appears, while the app asks the host over ssh; it then shows **Start daemon** if nothing is running there. The app starts a remote daemon over ssh with the daemon's settings in **Settings → Daemons**, so it listens at that daemon's **Daemon socket** and the app finds it.

A daemon started from the app follows the same rule as any other: while the app is connected, or any agent is running on it, it stays up, and it exits about 30 seconds after its last client leaves when it has nothing to do. To keep one up regardless, see [What a remote daemon must already have](#what-a-remote-daemon-must-already-have).

### When Start daemon fails

The section shows why, under its message; **Technical details** under it holds what went wrong underneath, such as the error the daemon failed to start with or what ssh printed. The usual reasons:

| Message | What to do |
| --- | --- |
| `The app cannot reach <host> over ssh. Check that the host is up and reachable from this machine.` | Check the host is up, and that `ssh <user>@<host>` works from this computer. |
| `ssh could not log in to <host>. …` | Check the key in this daemon's settings, or your `~/.ssh/config`; `ssh -o BatchMode=yes <user>@<host> true` must work without a prompt. |
| `The ssh host key of <host> is not trusted yet. …` | The app uses only a host key this computer has already accepted, even when your ssh config would accept a new one automatically. Run the command the message names once in a terminal, check the key and accept it, then try again. |
| `dot-agent-deck is not installed on <host>. …` | Run `dot-agent-deck remote add <name> <user@host>` from a terminal, then press **Start daemon** again. |
| `The dot-agent-deck on <host> is too old to report whether its daemon is running. …` | Upgrade it with `dot-agent-deck remote upgrade <name>`. |
| `The app could not read this deck's entry in the deck list (remotes.toml), so it does not know which dot-agent-deck to run on <host>.` | Check that the daemon is still listed by `dot-agent-deck remote list` and that its entry in `remotes.toml` is valid; **Technical details** says what the app could not read. |
| `The daemon was started on <host> but did not answer at <socket> within 25s. …` | The daemon started, but not where the app looks for it. Check that **Daemon socket** in this daemon's settings is where its daemon listens; clearing it and pressing **Test connection** finds the path. |
| `Could not start the daemon on this machine.` or `The daemon was started on this machine but did not answer at <socket> in time.` | Open **Technical details** for the error. To see everything the daemon prints, start one from a terminal with `dot-agent-deck daemon serve`; [Troubleshooting](../troubleshooting.md) covers the common causes. |
| `The daemon is running on <host>, but the app could not connect to it: …` | Press **Reconnect**. If it keeps happening, press **Test connection** and follow what it says. |

## Upgrade a remote daemon

When a remote daemon runs an older release than the app, the app offers **Upgrade** for it. It installs the app's version on that machine and restarts the daemon onto it, without leaving the app. It is the same upgrade as `dot-agent-deck remote upgrade <name>` in a terminal ([Remote Environments → Upgrade a remote](../remote-environments.md#upgrade-a-remote)): the same question when agents are running, and the same results.

**Where it appears:**

- On the Dashboard, on the daemon's section header, when the daemon is connected. Hovering it shows both versions.
- In the **Incompatible daemon** note, when the app has refused the daemon because it is older, beside the note's other buttons.

It does not appear when the daemon runs the same release as the app, when the daemon is newer than the app (update the app instead; upgrading the app is not done from here), or when the app could not learn the daemon's version. For the daemon on this machine, **Upgrade** installs nothing: it restarts the daemon onto the app's own version, and the app does that by itself when it finds an older one. See [Upgrade the daemon on this machine](#upgrade-the-daemon-on-this-machine).

**What happens when you press it:**

1. A dialog says what it will do, for example `This installs 0.45.0 on build-box (its daemon runs 0.44.0 now) and restarts the daemon onto it.` Press **Upgrade**, or **Cancel** to do nothing.
2. The dialog shows its progress: **Installing the new version**, **Restarting the daemon**, **Checking the new daemon answers**.
3. If agents or orchestration roles are running on that daemon, it stops before restarting and lists every one the restart would stop, by name, with its pane and directory where the daemon knows them.
   - **Restart now** stops exactly those and restarts the daemon onto the new version.
   - **Keep current daemon**, the default, stops nothing: the new version stays installed, the old daemon keeps running your agents, and it runs the new version the next time it restarts. Closing the dialog or pressing `Escape` at this point is the same as **Keep current daemon**.
   - If what is running changes while you decide, nothing is stopped and the dialog shows the new list and asks again.

   With nothing running, there is nothing to ask, and the daemon restarts straight away.

   Once the daemon has agreed to restart, it starts no new agents: **New agent** on that daemon, or a start from a terminal, fails with `the daemon is restarting; start the agent again once it is back`. Start it again once the upgrade has finished.
4. The dialog ends with what happened, in one of these forms:

| Title | What it means | What to do |
| --- | --- | --- |
| **Daemon upgraded** | The daemon now runs the app's version. The dialog lists anything the restart stopped. The panes you had open on that daemon's agents close. | Nothing. Start new agents with **New agent**. |
| **Daemon kept running** | The new version is installed, and the daemon keeps running the old one with the listed agents, because you chose to keep it, or because what was running kept changing while you were asked. | Press **Upgrade** again when those agents have finished. |
| **Installed — the daemon could not restart itself** | The new version is installed, but the running daemon is from a release that cannot be asked to restart, so it keeps running the old version. | Do what the dialog says: from a terminal, `dot-agent-deck connect <name>` and accept its restart prompt, or `dot-agent-deck daemon restart` on that machine. Once the daemon runs this release or a later one, it can restart itself. |
| **Installed — the daemon was not restarted** | The version that landed is older than this upgrade support, which can happen with a Homebrew install whose tap has an older release, so it could not restart the daemon. Nothing was stopped, and the daemon keeps running. | From a terminal, `dot-agent-deck connect <name>`: the TUI on that machine restarts the daemon onto the installed version, asking first when agents are running. |
| **Installed — no daemon was running** | Nothing was running there, so nothing was restarted. | Press **Start daemon** on its section of the Dashboard ([above](#start-a-daemon-from-the-app)); it runs the new version. |
| **Another restart is already running** | Someone else, from a terminal or another app, is restarting the same daemon. | Press **Reconnect** in a moment. |
| **Upgrade failed** | A step failed, and the dialog says which one and why. When the install failed, the old daemon keeps running; if the new version landed before a later step failed (reinstalling the hooks, or recording it in the deck list), the dialog says it is installed. If the new binary was put in place but did not pass its version check, the reason says it `was replaced`, and the dialog names what is installed now: the version the check read, or `An unverified build`. When the restart failed, the new version is installed and the old daemon keeps running, unless the dialog says it `may have stopped`: then the daemon's answer to the restart never arrived and the old daemon no longer answers, so press **Reconnect** to see what is running now. | Fix what the reason names, then press **Upgrade** again. `dot-agent-deck remote doctor <name>` diagnoses the ssh setup and the install of a host. If it failed while checking the restarted daemon, see [Restarted, but the new daemon did not answer](#restarted-but-the-new-daemon-did-not-answer). |
| **Upgrade stopped unexpectedly** | The upgrade stopped before it could report how far it got, so the new version may or may not be installed, and the daemon may or may not have restarted. The panes you had open on that daemon's agents close. | Press **Reconnect** to see which daemon is answering now, then press **Upgrade** again if it still runs the old version. |

![The Upgrade dialog over the Dashboard for the remote daemon dev@build-box, titled Restart and stop these?: Installing the new version is ticked, Restarting the daemon is in progress, and the dialog lists the three agents the restart would stop, each with its directory, above a Keep current daemon button and a red Restart now button](/img/daemon-upgrade-desktop.png)

**Check it worked:** the daemon's section shows its agents again (or **No agents are running yet**) and no longer offers **Upgrade**.

The app installs over ssh from this computer, the way `remote upgrade` does, so the host needs what `remote add` needs: `curl` to download the release, or a Homebrew install of the deck, which it then upgrades with `brew` ([Remote Environment Requirements](../remote-requirements.md)). If the daemon is no longer in the deck list (`remotes.toml`), for example because it was removed with the CLI, the app says so and suggests `dot-agent-deck remote add`, or `dot-agent-deck remote upgrade` from a terminal. Pressing **Upgrade** again while an upgrade of the same daemon is running is refused.

### Restarted, but the new daemon did not answer

`It failed while checking the restarted daemon: restarted, but the new daemon did not answer within 20s` means the old daemon stopped and its replacement did not come up in time. If a systemd user service runs the daemon on that machine, systemd normally starts the new one itself: check that the unit keeps `Restart=on-failure` and run `systemctl --user restart dot-agent-deck.service` there ([Keep the daemon running](../remote-requirements.md#keep-the-daemon-running)). Otherwise, press **Start daemon** on its section of the Dashboard ([Start a daemon from the app](#start-a-daemon-from-the-app)), or run `dot-agent-deck connect <name>`, which starts one, and press **Reconnect**.

When the reason ends with `and it is still the daemon that was asked to restart`, the old daemon agreed to restart but never stopped, and nothing replaced it. Press **Upgrade** again, or run `dot-agent-deck daemon restart` on that machine and press **Reconnect**.

## Upgrade the daemon on this machine

When the daemon on this machine runs an older release than the app, for example because you updated the app while the daemon kept running, the app restarts the daemon onto the app's version, as the TUI does when it starts. The daemon it starts is the one that came with the app, the same one **Start daemon** starts, so nothing is downloaded or installed.

**What happens when the app finds an older daemon here**, at start or while it is open:

- **Nothing is running on the daemon**: the app restarts it without asking. The **Upgrade** dialog opens on its progress (**Preparing this app's daemon**, **Restarting the daemon**, **Checking the new daemon answers**) and ends with **Daemon upgraded** and both versions, for example `The daemon on this machine now runs 0.47.0 (it was 0.46.0).` Press **Close**.
- **Agents or orchestration roles are running on it**: the dialog says so and lists every one the restart would stop, by name, with its pane and directory where the daemon knows them, the same question the remote [Upgrade](#upgrade-a-remote-daemon) asks.
  - **Restart now** stops exactly those and restarts the daemon onto the app's version.
  - **Keep current daemon**, the default, stops nothing, and the daemon keeps running the older version with your agents. Closing the dialog or pressing `Escape` is the same as **Keep current daemon**.
  - If what is running changes while you decide, nothing is stopped and the dialog shows the new list and asks again.

**In the TUI**, the same happens when `dot-agent-deck` starts and finds a daemon from a different build than its own: with nothing running it restarts the daemon without asking, and with agents running it prints `Daemon version mismatch`, the two builds and the agents a restart would stop. Press `S` to restart the daemon, stopping those agents, or any other key to keep the current daemon and attach to it with your agents intact. [Installation → Upgrading](../installation.md#upgrading) covers the TUI's other cases.

The app does this once for each daemon version while it is open, so after **Keep current daemon**, or a restart that failed, it does not ask again until the next time you start the app. If an agent's pane or **New agent** is open when the app finds the older daemon, the dialog waits until you close it. To upgrade sooner, press **Upgrade** on the **Local daemon** section of the Dashboard; hovering it shows both versions. The dialog asks first, then goes on as above.

The restarted daemon sets up the agent hooks again, as every daemon start does.

It applies only while the app is connected to the daemon on this machine, so the **Daemon** selector must show **This machine** or **All daemons**. Nothing happens when the daemon runs the same release as the app or a newer one. A daemon the app cannot work with shows **Incompatible daemon** with **Replace daemon** instead, which restarts it the same way ([Installation → Keep the app and the daemon on the same release](../installation.md#keep-the-app-and-the-daemon-on-the-same-release)).

A daemon from a release before 0.46.0 cannot be asked to restart. The app restarts it only when nothing is running on it; otherwise the dialog ends with **Daemon kept running** and lists what is running. Stop those agents, or let them finish, then press **Upgrade**.

The dialog ends with one of the titles in the [remote table above](#upgrade-a-remote-daemon), without the install: **Daemon upgraded**, **Daemon kept running**, **Another restart is already running**, **Upgrade failed** or **Upgrade stopped unexpectedly**, each with what to do. **No daemon was running** means the daemon stopped before it could be restarted; press **Start daemon** on its section of the Dashboard.

**Check it worked:** the **Local daemon** section of the Dashboard lists its agents (or **No agents are running yet**) and no longer offers **Upgrade**.

**Update the CLI along with the app.** The daemon now runs the app's version, but the `dot-agent-deck` on your `PATH`, which the TUI and your terminal commands run, is whatever you installed last. If that is an older release, the TUI started from it restarts the daemon onto its own older version, asking first when agents are running ([Installation → Upgrading](../installation.md#upgrading)).

## Rename a remote daemon

In **Settings → Daemons**, the **Daemon** row shows each remote daemon's address beside its name. To rename one, choose it, change **Deck name**, and press **Rename**. The new name is saved to `remotes.toml`, so the CLI uses it straight away: `dot-agent-deck connect <new name>`.

If the daemon was changed somewhere else since the app showed it, for example renamed with the CLI, nothing is renamed: the app says so and shows the list as it is now, and you can rename it again from there. If it was removed, for example with `dot-agent-deck remote remove`, the app says the daemon is no longer in the deck list and drops it from the list.

## Remove a remote daemon

In **Settings → Daemons**, press the trash icon beside the daemon. This removes its entry from `remotes.toml`, so `dot-agent-deck connect <name>` and `dot-agent-deck remote list` no longer know it either. It does nothing on the host: the binary, the hooks and any running daemon stay there. `dot-agent-deck remote remove <name>` does the same from the CLI.

If another program changed or removed the same entry since the app loaded the list, the app refuses the save rather than guess which host you meant; the list is shown as it now is, and you make the change again.

## Limits

- Remote daemons need macOS or Linux on the computer running the app; the ssh tunnel is not available on other platforms.
- Entries in `remotes.toml` of a type other than `ssh`, or whose values the app's ssh validation refuses, are left out of the app's list and left untouched in the file.
