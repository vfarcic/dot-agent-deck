# Desktop GUI developer preview

The desktop GUI under `desktop/` is an opt-in Tauri preview for PRD #176. It is a second local client of the existing daemon, not a replacement for the TUI, and it is not included in the default release artifacts. The current M0/M1 spike deliberately depends on the root `dot-agent-deck` library before the protocol crate is extracted so that terminal transport and the control-deck interaction model can be tested first.

## Prerequisites

- Enter `devbox shell` for the repository's pinned toolchain: Rust 1.97.1, `cargo-nextest`, Clippy, rustfmt, Node.js 24.12.0, and pnpm 10.34.5. Provide equivalent versions yourself if you do not use Devbox — the frontend needs Node.js 20.19 or newer, and pnpm 10.x, which is the line that reads `desktop/pnpm-lock.yaml`'s `lockfileVersion: '9.0'` without rewriting it. CI's `desktop-web` job deliberately runs Node 20 rather than the Devbox pin, so the stated floor stays tested rather than merely claimed.
- Install the [Tauri 2 system prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform. On macOS this includes the Xcode command-line tools. On Linux a `devbox shell` already carries them — see [Linux system libraries](#linux-system-libraries) below — and only a non-Devbox Linux setup installs WebKitGTK and the related `-dev` packages by hand.
- Install the desktop JavaScript dependencies once:

```sh
cd desktop
pnpm install
```

Agent CLIs and their credentials are needed only for agents you deliberately start through the daemon. The fixture preview does not call an LLM, execute an agent command, or modify project files.

### Linux system libraries

This is not optional reading on Linux, because `desktop/src-tauri` is a **workspace member**: both gates CLAUDE.md mandates carry `--workspace`, so `cargo clippy --workspace --all-targets --features e2e -- -D warnings` and `cargo test-fast` build this crate whether or not you are working on the GUI. Without GTK 3, WebKitGTK and glib they fail — the first at build time (`The system library 'glib-2.0' required by crate 'glib-sys' was not found`), the second at run time (`libgdk-3.so.0: cannot open shared object file`). Issue #771.

**In a `devbox shell` there is nothing to install.** `devbox.json` carries a `path:tauri-deps#tauri-deps` entry; `tauri-deps/flake.nix` builds the transitive pkg-config closure of the same libraries `ci.yml`'s `build` job installs with apt, and `devbox.json`'s `env` block points `PKG_CONFIG_PATH` at it. `pkg-config` itself is pinned there too, so the resolution does not depend on the host having one. That `env` entry **sets** `PKG_CONFIG_PATH` rather than appending to it — devbox's `env` expands only `$PATH` and `$PWD`, so appending is not expressible — which means a value from your login shell does not survive into the devbox shell. That is the wanted behaviour rather than a limitation worked around: a shell that quietly fell back to `/usr/lib`'s `.pc` files would build and then fail at run time, which is the trap described below. Nothing else is exported — in particular `LD_LIBRARY_PATH` stays unset.

**Outside Devbox, on a distribution toolchain, plain apt is enough.** The Debian/Ubuntu set is the one CI installs:

```sh
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev libxdo-dev
```

**Do not mix the two.** `apt-get install` inside a `devbox shell` looks like it should work and does not, which is what made issue #771 expensive rather than merely annoying:

- A devbox shell runs under **Nix glibc**, whose loader cache does not exist — `ldconfig -p` returns *zero* entries. Libraries under `/usr/lib` are therefore invisible to the dynamic linker no matter what apt put on disk, so `pkg-config` can report `glib-2.0` present, the build can succeed, and the test binary still dies at `libgdk-3.so.0: cannot open shared object file`.
- The obvious second workaround breaks something else. `export LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu` does make both Rust gates pass, and then Nix's own `node` fails with `undefined symbol: uv_tcp_keepalive_ex` because it picks up the system `libuv` ahead of its own. `pnpm install`, `pnpm build` and `pnpm test` are all unusable while that variable is set.

The fix avoids `LD_LIBRARY_PATH` entirely rather than pointing it somewhere safer, and the reason is worth knowing if you touch `tauri-deps/flake.nix`: each `.pc` file names absolute `/nix/store` paths, so the linker is invoked with `-L/nix/store/…-gtk+3-3.24.52/lib`, and Nix's `ld` wrapper turns every in-store `-L` into a matching `-rpath`. The test binary finds its libraries through its own RUNPATH, which is per-binary and cannot leak into `node`. `LD_LIBRARY_PATH` takes precedence over `DT_RUNPATH` for *every* process in the shell, which is the same class of problem in the other direction.

CI's `devbox` job runs `scripts/devbox-smoke.sh`, which resolves each module through `pkg-config` and fails the job if any is missing. It is the only job that can see this regress: every other job installs the compile set with apt and would stay green with `devbox.json` empty of GTK.

Bundling a `.deb` locally needs more than the compile set — `patchelf`, `fakeroot`, `file` and `desktop-file-utils` — which nothing in this repository's gates exercises, so they are deliberately not in `devbox.json`. Install them yourself before `pnpm tauri build`.

## Fixture preview

Run the web frontend without Tauri for a deterministic, safe UI fixture:

```sh
cd desktop
pnpm dev
```

Open `http://localhost:1420/`. A normal browser defaults to fixture transport; `http://localhost:1420/?fixture=1` selects it explicitly. `?state=` picks the scenario, and the five it accepts are `connected` (the default four-agent deck), `crowded`, `empty`, `disconnected` and `error` — for example `http://localhost:1420/?fixture=1&state=disconnected`. An unrecognised value falls back to `connected` rather than failing. See [The four connection states](#the-four-connection-states) for what each one is for.

The fixture's **Advance fixture** control walks a fixed review → test → human-approval sequence. Terminal input, pause/resume, retry, approval, workflow ordering, and agent-profile editing affect fixture or browser-local state only.

## Live Tauri preview

The live preview requires a daemon built from the same checkout. The exact build match is intentional: the desktop refuses to attach across a protocol or build mismatch and never recycles a daemon that may own valuable running agents.

Build the matching CLI once so the desktop can resolve a sibling executable:

```sh
devbox shell
cargo build --locked --bin dot-agent-deck
```

Then start the desktop window:

```sh
devbox shell
cd desktop
pnpm tauri dev
```

Tauri selects live transport automatically and initially performs a connect-only probe. If no daemon owns the socket, use the visible **Start daemon** action; daemon creation never happens merely because the window opened. The Rust core resolves only a pre-launch `DOT_AGENT_DECK_BINARY`, a sibling `dot-agent-deck` binary, or one already on `PATH`—the webview cannot supply an executable path. You may instead run `cargo run --locked --bin dot-agent-deck -- daemon serve` in another terminal and leave it running.

If another build already owns the default socket, do not stop it until you have confirmed that terminating its agents is safe; use `dot-agent-deck daemon stop` only as a deliberate lifecycle action.

### Attaching across a build stamp difference (development only)

The handshake makes two checks, and they are not equally strict. `PROTOCOL_VERSION` must match — that is the wire contract. The **build stamp** (`git describe`) must then match too, which is right for a shipped app but hostile in development: every commit restamps the desktop on its next rebuild, and a released daemon never matches a branch build at all. The result is that the daemon you actually use is the one daemon the preview refuses to open.

Set `DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH=1` before launching to downgrade the **stamp** difference from a refusal to a warning:

```sh
DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH=1 pnpm tauri dev
```

The protocol check is unaffected and still refuses an incompatible daemon, and the mismatch is not swallowed — the connection banner keeps naming both builds for the whole session. Only `1` and `true` arm it.

This is a **development** switch, not a compatibility guarantee. Two builds can agree on `PROTOCOL_VERSION` and still disagree about what a field means — precisely the semantic break `CLAUDE.md` rule 12 describes — so the stamp check is what protects a daemon that owns live agents from a client that reads its state differently. Use it to inspect a daemon, not to run work you care about, and never in a packaged build.

Users get the same escape hatch without a shell, through **Connect anyway** (issue #801). It appears in the connection banner — and on the agent overview's incompatible note — only when the desktop crate reports the mismatch as **stamp-only**, meaning the protocol check already passed. It is confirmation-gated, it is **session-scoped** (a process-global flag, never written to disk, so quitting the app restores the refusal), and it is offered whatever the live-agent count, which is the case a packaged `.app` could not otherwise recover from: **Replace daemon** is correctly refused while agents are live, and an env var is not something a `.app` launched from Finder ever receives. Accepting it does not clear the caveat — the connection banner stays up while connected, naming both builds, for the rest of the session.

A **protocol** mismatch offers nothing. The version check runs first in `classify_handshake`, so an allowance cannot reach it, and the crate never sets the stamp-only flag on that path — so no screen puts a button on it either. `protocol_mismatch_never_advertises_an_override` in `desktop/src-tauri/src/daemon_bridge.rs` pins both halves.

In live mode the preview lists daemon-owned agents, attaches xterm.js to each PTY stream, forwards terminal input and resize requests, refreshes status, and exposes a confirmed stop action. Open **Workflows**, provide the exact orchestration name from the target project's `.dot-agent-deck.toml` and an absolute project directory, then choose **Launch live loop** to start its configured role set with the enabled profile commands. **Start daemon** and **Launch live loop** both require a confirmation. The project configuration must contain exactly the submitted roles and one start role; the current bundled `dot-agent-deck` profile uses `orchestrator` as that start role and requires every listed profile to remain enabled for a live launch. Before spawning, the desktop materializes the same canonical coordinator context used by the TUI. Non-Pi coordinators use a readiness-gated, identity-bound, idempotent submission with bounded retry. Pi cannot be the desktop workflow coordinator in this preview: its native seed path has no delivery acknowledgement, so the bridge rejects that launch before spawning any role. Use a non-Pi coordinator or launch that orchestration from the TUI until acknowledged native seed delivery is available. A partially-created workflow—or one whose coordinator context cannot be delivered—is rolled back in reverse role order.

Desktop bundles include `dot-agent-deck` as a Tauri sidecar built from the same checkout. Run `pnpm bundle:app` to prepare the matching sidecar and produce the native app; the separate bundle config keeps ordinary workspace `cargo test` runs independent of generated binaries. A build/protocol mismatch exposes **Replace daemon** only when the old daemon reports zero live agents; replacement uses the bundled binary and never force-stops live agents. A stamp-only mismatch additionally exposes **Connect anyway**, described above, which starts and stops nothing.

The **Projects** rail opens a device-local project library. Each entry stores a display name, absolute repository directory, orchestration name, and notes. Selecting an active project updates the control-deck context and prefills the workflow launch form. Removing an entry uses a two-step confirmation and only removes local desktop metadata; it never deletes or moves the repository. Projects still need a matching `.dot-agent-deck.toml`, and launch remains the authoritative validation boundary.

Live workflow launch from the desktop preview is currently supported on macOS and Linux only. On Windows, the workflow sheet detects the platform, explains the limitation, and disables **Launch live loop**; the Rust bridge independently rejects a crafted `StartWorkflow` IPC request before validating or spawning roles. This guard is intentional because generated profile commands use POSIX shell quoting and must not be passed to `cmd.exe`. The fixture, daemon connection, and existing-agent terminal views remain available on Windows; use the TUI or launch commands manually until native Windows command construction is implemented.

Whole-run pause, fixture advancement, approval, and retry are not sent to the daemon. Workflow ordering remains a local preview and the live launch follows the role order in `.dot-agent-deck.toml`; command overrides apply to that launch only and do not rewrite project configuration.

## The agent overview

PRD #745 adds the app's second screen: a fleet overview of every agent the daemon owns, with **no terminal anywhere on it**. It lives in `desktop/src/components/AgentOverview.tsx`, is reached from the **Overview** button in the left rail, and works in both fixture and live mode. It renders *instead of* the deck rather than as an overlay sheet, so the view state lives in `App` above `ControlDeck` as a discriminated union (`DeckView`) carrying `deck` and `overview` — a union from the start, even at two values, so later destinations arrive as added variants rather than as a refactor of a boolean. The deck is still the screen the app opens on; the overview is not yet the landing screen, because there is nowhere to drill in to yet (see [Current milestone limits](#current-milestone-limits)).

### What it shows, and why not more

**Four columns by default, nine available, and the operator chooses.** Out of the box: the agent's **name**, its **status**, its **working directory** and its **uptime** — who it is, whether it is healthy, where it is working, and how long it has been at it. The other five — last activity, CLI, active tool, tool count, last prompt — are one click away in the **Columns** picker in the top bar, and the choice is remembered. See [Columns, and who chooses them](#columns-and-who-chooses-them). Every one of the nine is a value the daemon genuinely reports, and there is no `Unavailable` anywhere on the screen. Model, tokens, cost, context window, git branch and attempt count are **absent rather than shown as unavailable**, because none of them exists in daemon state at all — not on the wire, not in `RunningAgent` (`src/agent_pty.rs`), not in `SessionState` (`src/state.rs`). Inventing a source of truth for them is [PRD #633](https://github.com/vfarcic/dot-agent-deck/issues/633)'s discovery work, not this screen's. Session *duration* was on that list too and is no longer: the uptime column ships one, from a source the daemon genuinely holds — see [Uptime](#uptime).

That decision is enforced by the compiler rather than by review. The screen renders from `OverviewAgent`, a `Pick<>` projection of `AgentSession`, and never from `AgentSession` itself — so reaching for `model`, `cost`, `tokens`, `contextPercent`, `worktree`, `attempt` or `duration` on this screen is a **compile error** rather than something to remember. Two fields are widened past the raw `Pick<>` for the same reason a `Pick<>` exists: it closes dishonest field *names* but cannot close a dishonest *sentinel* inside an allowed one. `cwd` is optional here so a directory the daemon did not report travels as absence the whole way, and `writeLease`'s `"unknown"` sentinel is reversed to absent at this boundary. A cell with nothing to say renders blank — not a dash, and not a placeholder.

Sanitisation is a property of the screen rather than of individual cells: every daemon-supplied string passes through `desktop/src/lib/displayText.ts` before React sees it — rendered text, `title` attributes, and the copies behind `data-*`, DOM ids, IDREFs and React keys — while grouping, sorting and the `(daemonId, agentId)` identity keep the raw values. An agent whose reported display name consists entirely of invisible characters renders as `unnamed agent <id>` rather than as a blank cell, on the one screen whose whole job is telling agents apart.

Two fabricated values on the **deck** were withdrawn by the same principle, since a fabricated value is worse than an absent one — it looks like data. Every tile printed `ATT 01` and the top bar printed `ATTEMPT 01`, both read from a hardcoded `1`, and no daemon tracks a retry count anywhere; the branch line printed a literal `Unavailable` where a branch name belongs, and no daemon tracks a per-agent branch either. In live mode the attempt readouts now show the deck's established em dash and the branch chip is simply absent until something reports a branch. The deterministic fixture keeps its own attempt counts and branch, which are legitimately fixture data.

### Columns, and who chooses them

The picker lives in the overview's top bar, so choosing columns never leaves the screen. It offers exactly nine options, and **it cannot offer a tenth by mistake**: the option list is `COLUMN_FIELDS` in `desktop/src/components/AgentOverview.tsx`, whose ids are declared `satisfies readonly (keyof OverviewAgent)[]`, so a column named `model`, `cost`, `tokens`, `contextPercent`, `worktree`, `attempt` or `duration` is a **compile error** rather than something review has to catch. The picker inherits the screen's honesty guarantee from the same `Pick<>` that enforces it everywhere else, instead of restating it.

**The CLI column names a binary, not an identifier.** It shows the *command* that launches the agent (`claude`, `opencode`, `pi`, `codex`, `devin`), resolved from `AgentSpec::default_command` in `src/agent_registry.rs` and carried to the webview as `DesktopAgent.cli_name` — not the serialised `AgentType`, which is the wire identity and reads `claude_code` and `open_code`, neither of them a name anybody types. Deriving it from the registry rather than restating it desktop-side is what stops a second copy drifting: a new agent added there arrives on this column with nothing to update. Where this build cannot name a binary — the daemon reported `none`, which is also where an agent type from a *newer* daemon lands, since `AgentType::None` carries `#[serde(other)]` — the field is absent and the deck's generic word stands in rather than an invented name.

**The name column is permanent.** Its checkbox is checked and disabled, and `orderedColumns` puts it back whatever a stored value asks for — a disabled checkbox governs the UI, not a `localStorage` key an older build wrote. A row with no name is not a shorter row, it is an anonymous one, on the screen whose whole job is telling agents apart.

**There is a way back.** A **Restore defaults** entry at the foot of the menu puts the four back; without it the only route out of a set somebody unticked their way into was remembering which four the screen opened on. It persists through exactly the path every other change does — the selection goes up to the screen, whose effect writes it — so there is no second way for a choice to be saved.

**The menu dismisses on an outside click, and three details are the whole thing.** It listens on `pointerdown` rather than `click`: a click fires *after* focus has already moved, so a menu closing on it closes after whatever was clicked took focus, which reads as the menu lagging behind the user. Anything inside the picker's root element is ignored, and the trigger button lives inside that root — were it outside, its own pointer-down would close the menu and its click would toggle it straight back open, so the button would appear not to work at all. And the listener is bound only while the menu is open and removed when it closes, not merely on unmount, because `open` is in the effect's dependency list. `Escape` still works and was previously the only way out.

**Status is ONE column, and it used to be two.** There was a coloured mark in the first track and a textual State column in the third, both rendered from `agent.status` and neither saying anything the other did not. Two picker entries for one field would have been a question with no right answer, so the mark and the label now share one cell: the colour is still what you scan, the word is still what you read, and removing the column removes both. Row-level status signalling is unaffected either way — `data-status` on the `<tr>` is what tints a failed row, and it is not a column.

**The choice is remembered, per mode.** It is written to `dot-agent-deck.desktop.overview-columns.v1.<mode>` through `modeScopedKey`, for the reason every persisted key in this app is scoped that way: a fixture visit must not hand live mode a layout, and vice versa. `readStoredColumns` is where every way a stored value can be unusable turns back into something renderable, and each case is about a value an **older build** wrote rather than about today:

- **Absent, unparseable, or the wrong shape** — a bare string, a number, an object with no `columns` array — takes the defaults. A `SyntaxError` on mount would take the whole screen down.
- **A column that no longer exists is dropped**, not carried through. A retired or renamed id would otherwise render as a `<th>` with no cell under it and one dead grid track down every card — a fault that looks like a layout bug and is really a migration one.
- **Nothing recognisable left** — every stored id unknown, or an empty array — takes the defaults rather than collapsing the screen to the single permanent column. A user cannot reach that state by unticking, because the permanent column has no checkbox to untick.

Whatever survives is written straight back, so the next visit reads a value this build understands.

**No column is hidden by viewport any more.** Two media queries used to drop uptime, CLI and the working directory below 1180px and the active tool, tool count and prompt below 680px, by `nth-child` index. Once the operator picks the columns, hiding one by window width fights them silently — and the index-sensitivity is what made adding a column a five-rule renumbering across two queries plus three hardcoded templates. The narrow case is a **horizontal scroll of the whole table region** instead: the legend and every group card sit inside one `.overview-table-region`, which is the load-bearing part. All the cards share one `grid-template-columns` — generated from the selection and published as the `--overview-grid` custom property on the region's track — and that shared template is what makes the fleet read as a single table across card boundaries. Scrolling cards individually would let two of them sit at different horizontal offsets and the columns would stop lining up, precisely when the chosen set is wide enough to overflow. `min-width: min-content` on the track is what produces a scrollbar at all: every flexible column's track carries a fixed px minimum (`minmax(150px, 1.3fr)`, not `minmax(0, 1.3fr)`), so the grid's min-content width is the sum of those minimums — a fixed number — rather than zero (tracks crushed to nothing, no overflow) or the width of the longest prompt on screen.

### Grouping

The daemon's own `TabMembership` is the grouping key, so the overview groups agents exactly as the deck's tabs already do: one group per orchestration, one per mode name, and one standalone bucket for dashboard panes. **Standalone leads, then orchestrations, then modes.** That order is TUI parity rather than aesthetics — the TUI's dashboard tab is always first, so an overview that buried the same agents at the bottom would describe a different deck than the one running beside it. Orchestrations and modes follow in first-appearance order; roles inside an orchestration are sorted by `roleIndex`, and the start role carries a `COORDINATOR` badge.

The outermost unit is a **daemon group**, not an agent group, even though there is exactly one daemon today — with one it renders as minimal chrome: a connection lamp, the words `Local daemon`, and the per-status pips. **No socket filename is on screen.** A shortened label used to sit under the name, and its stated purpose was keeping a uid or a username out of screenshots — but on the default socket that label reads `dot-agent-deck-attach-501.sock`, so it printed the very uid it existed to hide and told the reader nothing they could act on either way. The full path is genuinely diagnostic, so it lives on the name's own `title`, where it costs no layout; `data-daemon-id` on the section still carries the sanitised, bounded identity for tests and a future drill-in. The two build stamps disclose themselves through that same hover when they differ (issue #801) — they used to hang off the connection message, which for a healthy connection no longer renders, and on the daemon's own name they are more discoverable than they were, because a reader hovers a thing they can see. Agents are keyed by the composite `(daemonId, agentId)` from day one, because agent ids are per-daemon monotonic integers starting at 1 and two daemons both mint `"1"`. Both decisions cost nothing now and are what stop [#742](https://github.com/vfarcic/dot-agent-deck/issues/742) from being a rewrite: a second daemon becomes a sibling group and changes no inner component.

**The connection message renders only when it says something the lamp does not.** The lamp is `connection-<status>`; the message beside it, for a healthy connection, was literally `Daemon responding` — two renderings of one bit. It is suppressed for that case and for no other: the disconnected and incompatible explanations stay, and so does the build-mismatch caveat that must remain visible for the whole session after **Connect anyway**. That last one is why the test is `!connected || buildStampMismatchOnly`, the same flag Connect anyway is gated on, and not a naive "hide when connected" — the bypassed-mismatch state *is* `connected`, and hiding its message would silently undo #801's guarantee.

### Last activity

The **Last activity** column is the one thing on the screen that needed a daemon-side change. `last_activity_ms` was added to `SessionSnapshot` as an additive optional field tagged `#[serde(default, skip_serializing_if = "Option::is_none")]`, which `src/daemon_protocol.rs`'s own policy names as an explicit do-not-bump case, so `PROTOCOL_VERSION` stays 8 and older and newer peers interoperate in both directions — a new app against an older daemon simply shows an empty column. It crosses the wire as epoch milliseconds rather than a formatted string, so the relative wording stays the webview's decision, and it carries its unit in its name because a bare integer invites the seconds-versus-milliseconds mistake that turns every reading into fifty-seven years.

It is on the screen where a duration built from `SessionState.started_at` is not, and the line between them is honesty rather than taste. `started_at` is invented as `Utc::now()` on hydration, so a duration built on it resets under a restarted daemon and silently lies about long-running work. (The screen *does* carry a duration now — see [Uptime](#uptime) — from a source the daemon genuinely holds. The narrow claim here, about `started_at`, still stands; it is only ever that source this section is rejecting.) `last_activity` is a high-water mark of observed event timestamps, advanced only when a newer frame arrives, so an agent quiet for an hour snapshots as quiet for an hour. And the daemon-restart case resolves to **absence** rather than to a lie, for free: the daemon persists no `AppState`, so a restarted daemon has no sessions at all, `AgentRecord.live` is `None`, the field never reaches the wire, and every cell renders empty. A duration had no absent state to fall back to, because `started_at` is always populated with *something*.

`displayActivity` in `desktop/src/lib/displayText.ts` renders one unit, largest that fits, floored — `just now`, `34m ago`, `2h ago`, `3d ago` — with the exact UTC instant on hover. It renders **nothing at all**, with no hover, for every value it cannot express honestly: absent, non-finite, outside `Date`'s ±100,000,000-day range, or more than a minute in the future. That last one is the clock-skew rule. The instant is stamped by whichever hook process emitted the event rather than by the daemon, so a small positive skew is the ordinary case and one minute of it reads `just now`; beyond that the value is deliberately *not* rewritten into `just now`, because a cheerful `just now` for a stamp ten years out is the same fabrication as a `started_at`-derived duration would be, and a negative `-60m ago` is merely the more obvious bug of the two. Note the desktop is **stricter than the TUI** here, whose `format_elapsed` clamps unconditionally to `0s`; the divergence is intentional and one-directional, since the TUI never renders a negative either.

One consequence is deliberately left alone: the TUI's reconnect hydration does not overlay `last_activity` from the snapshot, so a reconnected TUI card's `Last:` readout still resets. That is a pre-existing inaccuracy which this field has made *fixable* rather than fixed — `last_activity` is the ordering evidence `supersedes_generation` weighs and the key the `ListAgents` newest-wins join selects on, so seeding it belongs in its own change with its own tests.

### Uptime

The **Uptime** column is the second daemon-side change, and it is the duration this screen used to refuse. The refusal was aimed at `SessionState.started_at`, and that aim was right but the reason given for it — that `started_at` is invented as `now` on hydration — understated the problem. `started_at` is **event-derived**: a session exists only once a hook event has arrived, so an agent that has never emitted one has no start instant at all, and that is exactly the agent whose uptime a reader most wants.

The source that works is **when the daemon forked the process**. `AgentPtyRegistry::spawn_agent` stamps `RunningAgent.spawned_at` immediately after `spawn()` returns and nothing ever rewrites it; it reaches the wire as `AgentRecord.spawned_at_ms`, an additive optional field on the same do-not-bump basis as `last_activity_ms`, so `PROTOCOL_VERSION` stays 8. It is an **observation rather than an inference** — signal-independent, present for an agent that has never reported anything, and something the daemon definitionally knows. It is also never invented: `spawn_agent` is the only site that writes a value, so a record that arrives any other way (an older daemon's id-only `ListAgents` reply, the synthetic test seam) carries `None` and every consumer renders nothing. There is no `Utc::now()` fallback anywhere on the path.

**What the number means follows from where it comes from, with no flag needed.** A `clear = true` delegate respawns its worker by removing the old registry record outright and spawning a fresh one, so a restarted worker reports the age of its **current iteration**; a role nobody has restarted — an orchestrator, typically — keeps its original record and reports its **whole lifetime**. And because `agent_records()` filters entries whose child has exited, a spawn instant never outlives the process it describes and ticks up as a phantom uptime.

`displayUptime` in `desktop/src/lib/displayText.ts` renders it: the same buckets `displayActivity` uses, worded as a span rather than as a moment — `<1m`, `34m`, `2h`, `3d`, with the exact UTC spawn instant on hover. No `ago`, because the interval named is still running. The two functions share **one** clock-skew and usability guard (`relativeTo`) and fork only the vocabulary, so the rule a reader learns for one cell holds for the other: absent, non-finite, outside `Date`'s range or more than a minute in the future all render as nothing at all. Sharing the guard is deliberate — the three refusals *are* the policy, and a second copy of them is a second thing to keep true.

### The attach model

This is the least obvious part of the desktop client and the easiest to break, so it is written out in full.

**"Shows no output" and "opens no PTY" are separate properties, and only the first comes free.** Attaching a PTY was never tied to rendering one. `attachAgents` used to fire from `connect()`, from every `desktop://snapshot` event, and from the `start_daemon` action, attaching every agent in the snapshot regardless of what was on screen — filtered on an `attached` set rather than on anything mounted, and `TerminalViewport` triggered nothing at all. Each attach opens a real daemon `AttachStream` socket, and an `AttachStream` replays the full scrollback before it starts streaming, so a nine-agent fleet cost nine sockets and nine replays behind a screen displaying none of them.

Attach is now driven by one declaration, `DeckBridge.setShownTerminals(agentIds)` — real on `TauriDeckBridge`, a genuine no-op on `FixtureDeckBridge`, which owns no PTYs. It is the only attach trigger left. Its semantics, in the order they matter:

- **Shown terminals are always attached, and are never capped.** This is the constraint that shaped everything else: the deck mounts a terminal on every tile, so a bound applied over everything *attached* would kill six of nine visible panes the moment a nine-agent fleet is displayed.
- **Leaving a terminal does not detach it.** It moves into a bounded **warm** set, still attached, so bouncing between the handful of agents you are actually working with costs no scrollback replay.
- **The warm set alone is bounded**, by `MAX_WARM_TERMINALS` (`3`, exported from `desktop/src/lib/bridge.ts`), least-recently-left evicted first. Eviction is a full client-side teardown before the daemon is told: every map keyed by agent id — sessions, terminal channels, pending attachments, pending resizes, resize frames — is cleared, and only then does `desktop_terminal_detach` go out. A surviving channel entry would let a dead attach keep delivering output, and a surviving resize entry would push a size computed for the old pane at the next session.
- **An empty shown set flushes warm to zero**, not down to the bound. That is what makes the overview's "no terminals attached" true when you arrive from a nine-tile deck, rather than true only on a cold start.

**`setShownTerminals` is declarative, and must be called once per render commit with the whole set — never once per tile.** Nine single-id calls would leave eight of the nine warm and evict five of them, which is the same broken deck the bound exists to avoid. The deck does this in `desktop/src/App.tsx`, deriving the shown ids from the tiles whose tab is `"terminal"` (repeating `AgentTile`'s `?? "terminal"` default exactly) and keying one effect on those ids joined with a newline. That joined string is a dependency key and nothing else — never split it back apart, since a raw agent id containing a newline would come back out as two shown agents. The overview declares the empty set the same way, from a `useEffect` at the top of `AgentOverview`.

What the overview does **not** claim is that every socket a previous screen opened is already gone by the time it renders. The declaration is fire-and-forget, and an attach still in flight is cancelled by *marking* rather than by awaiting — one slow attach must not freeze every later terminal switch — so its daemon-side tear-down lands shortly afterwards. The screen used to say exactly that in a footnote rather than claim the stronger thing; the footnote is gone (the app does not annotate its own design), so the precise claim lives here and in `AgentOverview`'s docstring.

The property is pinned at the bridge level rather than by inspection. `desktop/src/lib/bridge.test.ts` drives `TauriDeckBridge` directly — the live bridge, the only one that owns a PTY at all — and asserts that a nine-agent snapshot attaches zero terminals while nothing is shown, and that `connect()` alone attaches nothing. Note the asymmetry when you touch this code: deleting the deck's effect fails no bridge test, because the bridge is behaving correctly; it silently leaves the deck with no attached terminals at all.

### The four connection states

The overview owns four states, and every one of them is reachable in the fixture without a daemon:

| State | What the screen says | Fixture URL |
|---|---|---|
| Connected | The fleet, grouped. `state=crowded` is fifteen agents across two orchestrations, a mode bucket and standalone panes — a four-agent fixture cannot answer a question about many agents | `?fixture=1&state=crowded`, or `?fixture=1` for the default four |
| Connected, zero agents | "No agents are running yet" — the first-run screen, stated explicitly as what a fresh install looks like rather than as a failure, with what to do next | `?fixture=1&state=empty` |
| Disconnected | Nothing can be said about the fleet until a daemon answers, so the list is blank rather than stale | `?fixture=1&state=disconnected` |
| Incompatible | A daemon answered the handshake but this build cannot read its agent list, with its reported running-agent count when it gave one | `?fixture=1&state=error` |

A fifth, `loading`, is the initial seed and is not selectable from the URL.

The header instruments read `—` rather than `0` in the three non-connected states. This is not decoration: a reconnect failure replaces the connection but *keeps* the previous snapshot's agents, and both the `disconnected` and `error` fixtures ship the default four, so deriving the counts unconditionally printed `AGENTS 4 · GROUPS 1` above a body correctly saying the fleet could not be read. "No agents" and "we cannot see the agents" are different statements and only one of them is true there.

The overview offers no daemon lifecycle action — Start daemon, Replace daemon and Stop all stay on the deck, and the incompatible note says so. The one exception is **Connect anyway**, which is not a lifecycle action: it changes this app's own classification of a handshake it already performed, so it belongs on the screen that is refusing to show you the fleet. It appears on the overview's incompatible note under exactly the conditions described in [Attaching across a build stamp difference](#attaching-across-a-build-stamp-difference-development-only) — live mode, and a mismatch the desktop crate reports as stamp-only.

## Release bundles

Every tagged release publishes the GUI as an **unsigned alpha** bundle, built by `release.yml`'s `desktop-bundle` job: a `.dmg` for macOS arm64 and a `.deb` for Linux x86_64. They arrive on the release as `dot-agent-deck-desktop-alpha-<platform>.<ext>` with their own `checksums-desktop-alpha.txt`, and the release notes carry a fixed section saying they are unsigned and how to clear the OS warning. There is no user-facing install page on purpose while the GUI is alpha; [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) writes one when that changes.

Three things about that job are worth knowing before editing it, because each is a single line that reads as housekeeping and is not. It hangs off `prepare` rather than `build`, so it runs beside the CLI matrix instead of extending the critical path. `finalize` — which creates the release and uploads the five CLI assets — deliberately does **not** list it in `needs:`, so a bundler that fails, hangs, or is skipped cannot delay or break an ordinary tag; the run goes red while the release stays green and complete. And `finalize`'s artifact download is constrained by `pattern: dot-agent-deck-*` because `merge-multiple: true` would otherwise flatten the desktop bundles into the same `dist/` directory, where the release glob would sweep them up and `task checksums`' `shasum -a 256 dot-agent-deck-*` would choke on the space in Tauri's `Agent Deck_<version>_amd64.deb`. All three are asserted by `xtask/linkage-check/src/release_workflow_wiring.rs`, since nothing else can check a workflow that only fires on a tag.

`tauri.conf.json` hardcodes `version: "0.1.0"` — the same deliberate placeholder as `Cargo.toml`. The job merges the real version into the bundle config at build time with `jq` and then fails the build if no output filename carries it, so a bundle labelled `0.1.0` cannot be published. `bundle.active` is likewise `false` in the base config and only the overlay flips it, which means an invocation that loses its `--config` produces no bundle *and still exits 0*; the job asserts the output exists rather than trusting the exit code.

**Windows is not built.** A bundle carries the daemon as a sidecar and no Windows daemon binary is published; a Windows GUI cannot attach to a remote Linux daemon instead, because the IPC transport is a named pipe on Windows and a Unix socket everywhere else. `prepare-sidecar.sh` nonetheless handles the `.exe` suffix on `*-pc-windows-*` triples — in both the cargo output path and the staged name Tauri resolves as `{path}-{triple}{ext}` — so that groundwork is done when [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) makes a Windows GUI reachable. It is covered by `xtask/linkage-check/src/sidecar_staging.rs`.

Bundling locally needs more than the compile set `ci.yml` installs: on Linux the `deb` bundler additionally wants `patchelf`, `fakeroot`, `file` and `desktop-file-utils`.

AppImage is deliberately not built. Its bundler downloads five third-party artifacts at build time, two of them shell scripts from a `master` ref, which is not a supply-chain property a release pipeline should have; it also failed on the first real attempt while the `.deb` from the same run verified clean.

One trap if you bundle locally in a `devbox shell`: the shell runs under nix's glibc, whose `ld.so.cache` does not exist, so `ldconfig -p` returns nothing and system libraries are invisible to the loader. Exporting `LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu` fixes the Rust side but then breaks nix's `node` (`undefined symbol: uv_tcp_keepalive_ex`, from picking up the system libuv). Bundle without it; the Rust link resolves through pkg-config's `-L` flags and does not need it.

## Security and ownership model

The daemon remains the single source of truth for agent lifecycle, PTYs, hooks, and orchestration. The desktop opens no HTTP API or TCP listener: the Rust core connects to the same per-user local IPC endpoint as the TUI, verifies the endpoint's ownership/permissions, performs the existing `Hello` protocol and build handshake, and then bridges typed snapshots and bounded terminal chunks through Tauri IPC. PTY output travels daemon → Tauri channel → xterm.js; focused terminal input and resize requests travel in the opposite direction.

The window uses a restrictive content-security policy and a minimal Tauri capability set. Bridge commands are scoped to the main webview, connection errors are sanitized before reaching it, terminal input and launch commands are bounded to 64 KiB, dimensions are constrained to `1..=4096`, and terminal sessions use opaque IDs and are detached when the bridge is disposed. A build mismatch is shown as incompatible rather than triggering an automatic daemon restart, and the only way past it is an explicit, confirmed, session-scoped **Connect anyway** that relaxes the stamp comparison alone.

Treat every live terminal as equivalent to its underlying agent CLI: it has the daemon user's filesystem and process permissions, and terminal input can authorize consequential work. The stop control has a confirmation step, but this preview is not a sandbox or an access-control boundary.

## Models and agent profiles

The fixture seeds an orchestrator and release profile using Claude and coder, reviewer, auditor, and tester profiles using `gpt-5.6-sol` with role-appropriate reasoning effort. Editing a profile stores a draft in the webview's local storage and **Confirm draft** does not execute anything or write `.dot-agent-deck.toml`; **Launch live loop** does execute each enabled profile's launch command through the daemon after the workflow validation described above. Treat launch commands as executable configuration, never store tokens in them, and review them before starting a live loop. **Reset defaults** clears the local draft.

For OpenAI, Anthropic, and OpenCode profiles, the launch command is generated from the current provider, CLI, model, reasoning-effort, and permission fields. The generated command is a read-only preview; on macOS and Linux, values are POSIX-quoted as individual shell words and invalid, NUL-containing, blank, or oversized commands block launch. An **advanced custom command override** is available for unusual CLIs, but it is explicitly labeled as an exact shell command that bypasses the structured fields. Its permissions are unmanaged by the profile UI, may be arbitrary, and must be encoded and reviewed in the command itself. Custom roles are excluded from structured full-access counts, and a custom override does not bypass the Windows platform guard. The launch confirmation distinguishes custom-command risk from permission claims made only for generated roles. The submitted command overrides the matching project role for that launch only; format-preserving profile write-back is not implemented.

Live sessions continue to use the commands and models that created them through the daemon. The current daemon snapshot exposes agent type but not a reliable model identifier, so the deck's live tiles still label model, cost, token and lease as unavailable instead of guessing. Two of that set have since moved: the branch chip and the attempt readouts are **gone** rather than labelled, because a fabricated `ATT 01` reads as data in a way an explicit "unavailable" does not (see [What it shows, and why not more](#what-it-shows-and-why-not-more)), and the write lease is genuinely reported on the [agent overview](#the-agent-overview), which reads it from `SessionSnapshot` rather than from the deck's fixture-shaped model.

## Current milestone limits

- The Tauri crate directly reuses the root library; the standalone protocol-crate extraction remains pending.
- **Local daemons only.** The GUI assumes the daemon is on the same machine. A forwarded socket does attach and streams remote agents and their PTYs correctly, but project resolution stays client-side, so a remote workflow launch reads the wrong `.dot-agent-deck.toml` and writes coordinator context to the wrong host. Treat remote as observation-only until that moves daemon-side.
- The deterministic loop, transition evidence, diffs, checks, artifacts, spend, and profile/workflow editors are preview data or local-only UI where the daemon does not expose structured data yet.
- Live orchestration graph events (`delegate`, `work-done`, and `dispatch`) and the graph view remain pending.
- OS-native notifications, signing, notarization, auto-update, remote/web hosting, and mobile layouts are out of the current spike. Packaging is no longer — see **Release bundles** above — but the bundles it produces are unsigned, so every OS still warns on first launch.
- Native Windows workflow-command construction is pending; the webview and Rust bridge both block desktop workflow launch on Windows rather than forwarding POSIX-quoted commands to `cmd.exe`.
- Pi coordinator launch is blocked in the desktop preview pending an acknowledged native seed-delivery protocol; Pi may still be used for non-start worker roles.
- Daemon startup is an explicit user action; visual rename and standalone start-agent flows are not exposed by the current frontend.
- Multi-pane throughput still needs the explicit M1.3 stress qualification, and a real-agent terminal session still needs the PRD's pre-release end-to-end gate.
- **The agent overview is not the landing screen, and has no drill-in.** The deck is still what the app opens on, and the overview's only exits are back to the deck. A group view (the deck filtered to one tab bucket) and a single-agent view are deferred; promoting the overview before they exist would land users somewhere they cannot leave.
- **There is no end-to-end harness for the real Tauri window.** No Playwright, no WebdriverIO, no `tauri-driver`; nothing drives the real window, real IPC, real xterm or real WebKitGTK. Coverage is vitest + Testing Library against a hand-built runtime, plus the desktop crate's Rust tests, plus the manual smoke check below — which is the compensating control rather than parity with `CLAUDE.md` rule 4's L2 tier.

## Verification commands

Run frontend tests and the production web build from `desktop/`:

```sh
pnpm test
pnpm build
```

Run the desktop Rust bridge tests and lints from the repository root:

```sh
cargo test --locked -p dot-agent-deck-desktop
cargo fmt --check
cargo clippy -p dot-agent-deck-desktop --all-targets -- -D warnings
```

Run the repository-wide required fast gates from the repository root. Both flags on the clippy line and `--workspace` on the test alias are load-bearing — `CLAUDE.md` rules 2 and 5 explain each of them:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings
cargo test-fast
```

There is no full-tier obligation before a PR — CI runs lane 1 on every PR (CLAUDE.md rule 5, [`e2e-lanes.md`](e2e-lanes.md)). What you do owe is the tests covering what you changed, so where a desktop change touches a PTY or real-agent path, run those by filter:

```sh
cargo test-e2e <filter>        # lane 1, no credentials needed
cargo test-e2e-live <filter>   # lane 2, needs your own agent credentials — and runs in no CI job
```

The live manual smoke check is: build the matching CLI, launch `pnpm tauri dev`, use **Start daemon** if needed, launch the configured live loop against a disposable project/worktree, confirm every role hydrates under one orchestration, interact with a real agent in an embedded terminal, resize the window and terminal, reconnect without duplicated output, and stop only a disposable agent through the confirmation dialog. Do not treat the fixture as proof that a real agent or daemon path works.

Then, with that same live daemon still owning those agents, extend it over the overview. This half is the only check that exercises the attach model against real sockets, since no automated harness drives the real window:

1. **Land on the overview.** Click **Overview** in the rail. Confirm the fleet renders grouped — standalone panes first, then the orchestration you launched as one card with its roles in role order and its coordinator badged — and that the counts in the header are real numbers rather than `—`. The daemon card's header should read `Local daemon` with no socket filename and no `Daemon responding` beside it; hover the name and the full socket path is there. If your desktop and daemon builds differ, the two stamps are on that same hover.
2. **Confirm zero terminals stay attached while it is up.** The app has no readout for this, so observe it from outside: the desktop process's open connections to the attach socket (`DOT_AGENT_DECK_ATTACH_SOCKET`, via `lsof -p <desktop pid>`) should fall to zero. Arriving here from a deck full of tiles is the case that matters — the warm set flushes to zero rather than down to `MAX_WARM_TERMINALS`, so a lingering three would be a regression rather than by design. Give the fire-and-forget declaration a moment to land before concluding anything.
3. **Read the honest columns.** Tick every column in the **Columns** picker first — the screen opens on four, and this step is about the other five. Confirm the **CLI** column names a binary you could type — `claude`, not `claude_code` — for every agent, and check the picker dismisses when you press the pointer down outside it and closes rather than reopening when you click its own button. **Restore defaults** should put the four back. Active tool and tool count should move as a real agent works. **Last activity** should read `just now` for the agent you are driving and a real age for one you left alone — never `just now` for all of them at once. A blank cell means the daemon reported no live session for that agent at all (`AgentRecord.live` absent), which is expected for one that has not yet emitted a hook event and is *not* the same thing as an agent that has been quiet. **Uptime** should be populated for *every* agent the daemon spawned, including one whose Last activity is blank — that pairing is the whole reason it comes off the registry record rather than off the session. Drive a `clear = true` delegate and watch that worker's uptime reset while the orchestrator's keeps climbing.
4. **Choose columns and leave.** Untick a column and confirm it disappears from the legend, every group card's header row and every row at once — one template, one scroll region. Narrow the window until the table overflows and confirm it scrolls **as a whole**: the group cards must stay aligned with each other and with the legend, since a card scrolling on its own is the failure this layout exists to prevent. Then quit the app, reopen it and return to the overview: the choice is still there. Check the fixture and live modes do not share it — `?fixture=1`'s columns are stored under a different key.
5. **Drill back to the deck with terminals still working.** Click **Open deck**, confirm every tile re-attaches and streams, and type into one to see it reach the agent. Coming back from the overview costs a replay **by design** — the empty declaration flushed the warm set to zero — so do not read that as a regression. The warm set is exercised without leaving the deck: switch one tile off its terminal tab and back, and it must return with no scrollback replay; do that for more tiles than `MAX_WARM_TERMINALS` and the least-recently-left one pays a replay when it is evicted.
6. **Stop the daemon under a live app** (`dot-agent-deck daemon stop`, from a shell — the app exposes no such action). The overview must switch to the disconnected note with the list blank rather than stale, and the header instruments must read `—` rather than `0` — the app keeps the previous snapshot's agents, so a `0` there is a regression. Reconnect and confirm the fleet comes back. A daemon's agents are its own children and it persists no session state, so a *restarted* daemon owns nothing and correctly lands on the zero-agent first-run screen rather than on a fleet with blank activity cells.
