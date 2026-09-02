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

Open `http://localhost:1420/`. A normal browser defaults to fixture transport; `http://localhost:1420/?fixture=1` selects it explicitly. Use `state=disconnected`, `state=error`, or `state=empty` to inspect failure and empty states, for example `http://localhost:1420/?fixture=1&state=disconnected`.

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

In live mode the preview lists daemon-owned agents, attaches xterm.js to each PTY stream, forwards terminal input and resize requests, refreshes status, and exposes a confirmed stop action. Open **Workflows**, provide the exact orchestration name from the target project's `.dot-agent-deck.toml` and an absolute project directory, then choose **Launch live loop** to start its configured role set with the enabled profile commands. **Start daemon** and **Launch live loop** both require a confirmation. The project configuration must contain exactly the submitted roles and one start role; the current bundled `dot-agent-deck` profile uses `orchestrator` as that start role and requires every listed profile to remain enabled for a live launch. Before spawning, the desktop materializes the same canonical coordinator context used by the TUI. Non-Pi coordinators use a readiness-gated, identity-bound, idempotent submission with bounded retry. Pi cannot be the desktop workflow coordinator in this preview: its native seed path has no delivery acknowledgement, so the bridge rejects that launch before spawning any role. Use a non-Pi coordinator or launch that orchestration from the TUI until acknowledged native seed delivery is available. A partially-created workflow—or one whose coordinator context cannot be delivered—is rolled back in reverse role order.

Desktop bundles include `dot-agent-deck` as a Tauri sidecar built from the same checkout. Run `pnpm bundle:app` to prepare the matching sidecar and produce the native app; the separate bundle config keeps ordinary workspace `cargo test` runs independent of generated binaries. A build/protocol mismatch exposes **Replace daemon** only when the old daemon reports zero live agents; replacement uses the bundled binary and never force-stops live agents.

The **Projects** rail opens a device-local project library. Each entry stores a display name, absolute repository directory, orchestration name, and notes. Selecting an active project updates the control-deck context and prefills the workflow launch form. Removing an entry uses a two-step confirmation and only removes local desktop metadata; it never deletes or moves the repository. Projects still need a matching `.dot-agent-deck.toml`, and launch remains the authoritative validation boundary.

Live workflow launch from the desktop preview is currently supported on macOS and Linux only. On Windows, the workflow sheet detects the platform, explains the limitation, and disables **Launch live loop**; the Rust bridge independently rejects a crafted `StartWorkflow` IPC request before validating or spawning roles. This guard is intentional because generated profile commands use POSIX shell quoting and must not be passed to `cmd.exe`. The fixture, daemon connection, and existing-agent terminal views remain available on Windows; use the TUI or launch commands manually until native Windows command construction is implemented.

Whole-run pause, fixture advancement, approval, and retry are not sent to the daemon. Workflow ordering remains a local preview and the live launch follows the role order in `.dot-agent-deck.toml`; command overrides apply to that launch only and do not rewrite project configuration.

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

The window uses a restrictive content-security policy and a minimal Tauri capability set. Bridge commands are scoped to the main webview, connection errors are sanitized before reaching it, terminal input and launch commands are bounded to 64 KiB, dimensions are constrained to `1..=4096`, and terminal sessions use opaque IDs and are detached when the bridge is disposed. A build mismatch is shown as incompatible rather than triggering an automatic daemon restart.

Treat every live terminal as equivalent to its underlying agent CLI: it has the daemon user's filesystem and process permissions, and terminal input can authorize consequential work. The stop control has a confirmation step, but this preview is not a sandbox or an access-control boundary.

## Application settings

The desktop app owns exactly one thing and gets everything else from the daemon: **its own settings**. They live in a per-installation TOML document at `platform::paths::config_dir()/desktop.toml` — a **sibling** of the TUI's `config.toml` and `keybindings.toml`, never a section inside them, because `DashboardConfig::save()` serialises its struct and would silently delete a `[desktop]` table it did not know about. `DOT_AGENT_DECK_DESKTOP_CONFIG` overrides the whole path and is the seam the tests use. The store is `desktop/src-tauri/src/settings.rs`; the surface is the **Settings** rail button, *Open settings* in the command palette (⌘K), and the same Escape/backdrop/close-button trio as every other sheet. PRD #803 has the full design; this section is the contract.

Three behaviours are worth knowing before building on it. **Loading never fails** — a missing, unparseable or unreadable file, and an unknown enum value, all yield defaults, because a settings file is not worth failing an app launch over. **Writing is atomic and owner-only** — a temp file in the same directory, then a rename, with mode `0o600` on Unix. And **writing preserves what this build does not understand**: the save merges the serialised struct into the document already on disk rather than replacing it, so a section written by a newer build survives an older build's save. That third one is not something `#[serde(default)]` gives you — without `deny_unknown_fields` it means *ignore*, not *retain*, so the unknown table is dropped at load and the next save would write over it. It preserves data, not layout: comments and inline-array formatting are lost to the canonical re-render ([#825](https://github.com/vfarcic/dot-agent-deck/issues/825)).

### The ownership boundary

The rule the whole app is arranged around, and the thing to read before deciding where a new setting goes:

> The desktop app gets everything from the daemon, wherever that daemon runs. The only thing it owns is its own settings.

| | |
| --- | --- |
| **Daemon-owned** | the project config, the coordinator context, cwd and project paths, available orchestrations/modes/roles, dispatch targets, the agent list and PTY streams, telemetry |
| **Client-owned** | settings — endpoints, appearance, model backends — plus genuinely presentational state: window size and position, focused tab, zoom |

The criterion for anything not on those lists, stated so it can be applied without re-deriving the reasoning: **a setting is client-owned when it describes the client itself — this machine, this display, this installation. If it describes the work, the project, or the machine the agents run on, it is daemon-owned and does not belong in this store, however convenient it would be to put it here.**

The failure this prevents has already shipped once. Reading `.dot-agent-deck.toml` client-side gives the right answer whenever the daemon is local, so nothing looked wrong until a real remote session put a local `/Users/…` path in the header beside the remote's `/home/…` panes. A rule that applies only to remote leaves the same trap for the next feature; a rule that applies always cannot. The converse matters too: **if a settings item ever needs the daemon to know about it, that item is by definition daemon-owned** — and at that point CLAUDE.md rule 12's cross-version contract check applies to it, on the other side of the line.

Everything in the store is deliberately about *this installation on this machine*, and none of it syncs. An endpoint describes reachability from here — a socket path, an ssh host, a forwarded socket. An appearance override is about this display. Downloaded models are on this disk. The cautionary precedent is the TUI's: `keybindings_path()` resolves through `platform::paths::config_dir()` on the machine the *process* runs on, and `dot-agent-deck connect` runs the TUI on the remote, so a `keybindings.toml` edited on the laptop is never read. Never store desktop settings daemon-side, and never read daemon-owned state from the client's disk.

### Adding a setting

Three steps, and the contract is deliberately this small. Adding a **setting** is step 1 plus an edit to your own panel; adding a **section** is one registry row and one component. Neither requires touching the store, the sheet, or anything belonging to another feature.

1. **Storage.** Add a field to your feature's section struct in `desktop/src-tauri/src/settings.rs`, or add a new section struct plus one line on `DesktopSettings`. `#[serde(default)]` throughout gives it a default; there is deliberately no `deny_unknown_fields`, so a field a newer build writes survives an older build reading the file. Field names stay `snake_case` and single-word where that is natural: the same struct is serialised to TOML for the user *and* to JSON for the webview, and every name today is one word, so the two agree byte for byte. The first genuinely multi-word field is the point at which a separate webview DTO has to be introduced — not a `rename_all` on this struct, which would make the TOML read badly.
2. **UI.** Add one row — `id`, `label`, `icon`, `component` — to `SETTINGS_SECTIONS` in `desktop/src/components/SettingsSheet.tsx`, and own your panel component. Implement `SettingsPanelProps` from `desktop/src/lib/settingsContract.ts`: that file *is* the type to build against, and it is short on purpose. A panel is an ordinary React component and owns its own layout — there is no generic key/value renderer, because #741's endpoint list and #802's model manager are not key/value widgets and a renderer built to fit both would fit neither. Send the **whole document** to `onSave` (spread `settings`, replace your own section) so a save can never drop a section this build's UI has not loaded.
3. **Secrets never go in the document, and never in `localStorage`.** The document may hold a non-secret *reference* — which backend holds the key, or a boolean saying one is stored — and nothing more. A real credential belongs behind the `SecretStore` seam PRD #803 names (store/load/delete keyed by a stable identifier, OS keychain as the intended implementation; #802 picks the backend). This is enforced, not advised: `no_settings_key_may_look_like_a_secret` in `settings.rs` fails the build on a key name containing `key`, `token`, `secret`, `password` or `credential`, with a short allowlist for genuine references. If your name really is a reference, add it to `SECRETISH_ALLOWED` with a comment saying which of the two allowed forms it is.

Two things to expect while doing it. `default_document_shape_is_pinned` asserts the exact serialised default document, so **a new field fails that test by design** — updating it is the deliberate act that puts the ownership question in front of a reviewer. And the settings sheet **drops its section column while there is only one section**, rendering that panel full width; your registry row is what brings the column back, with no layout work on your side. Both directions are pinned with stub sections in `desktop/src/components/SettingsSheet.test.tsx`.

### Two persistence mechanisms, for now

The settings document is not the app's only storage. Four keys still live in `window.localStorage`, namespaced by `modeScopedKey` (`desktop/src/lib/bridge.ts`) with a `.live`/`.fixture` suffix: the project library, agent profiles, the prompt library, and workflow role order. They are per-project draft content, which the boundary rule above puts daemon-side, so they were deliberately **not** migrated into `desktop.toml` — that store is per-installation too, and moving them there would entrench them on the same wrong side while looking resolved. [#824](https://github.com/vfarcic/dot-agent-deck/issues/824) tracks it and [#819](https://github.com/vfarcic/dot-agent-deck/issues/819) is the plausible home. Until then: **do not add a fifth `localStorage` key by pattern-matching.** Apply the criterion first — app preference goes in `desktop.toml`, project content waits for #819.

## Appearance and theming

The app follows the OS light/dark appearance by default, with an in-app override — Light, Dark or System — in the settings sheet's Appearance section. `System` is the default and it **removes** the `data-theme` attribute rather than writing a value; the two are not interchangeable, because there is no `[data-theme="system"]` block and an absent attribute is what lets the media query decide, live and in both directions. The override is `desktop/src/lib/appearance.ts`, the panel is `desktop/src/components/AppearancePanel.tsx`, and the value persists in `[appearance]` in `desktop.toml`. PRD #743 has the design.

### Every colour is a token

`desktop/src/styles.css` opens with the palette on `:root` — 66 colour tokens plus three layout ones — and **no colour is written anywhere else**. That is not a style preference: a hex literal in a rule cannot follow a theme, so it stays light in dark mode, and the decay is invisible because nothing renders dark mode in CI. The families:

- **Light chrome** — surfaces (`--canvas`, `--paper`, `--paper-strong`, `--paper-sunken`, `--paper-sunken-strong`, `--skeleton`), text (`--ink`, `--ink-soft`, `--muted`, `--faint`, `--on-accent`), lines (`--line`, `--line-strong`, `--dialog-edge`).
- **Accents**, each a family at several depths: `--teal-*` (primary), `--green-*` (passed/done), `--amber-*` (waiting/caution), `--red-*` (failed/destructive), `--violet-*` (needs a human).
- **Live status** — `--status-ok`, `--status-warn`, `--status-error`. One family rather than two, because they are read against both the light chrome and the dark instrument chrome.
- **Instrument chrome** — `--terminal`, `--shell*`: the dark surfaces wrapped around the terminals (the rail, the tab strip, the agent footer, the output panels, the reader overlay). These stay dark in both appearances, so most of them keep their light value.
- **Channel forms** — `--scrim-rgb`, `--paper-rgb`, `--teal-rgb`, for colours also used at partial alpha. `rgb(var(--scrim-rgb) / .12)` keeps the alpha at the call site so one token serves every depth. `--paper-rgb` and `--teal-rgb` track the token they are named after; **`--scrim-rgb` tracks nothing** — its role is the nine scrims and drop shadows, and it goes to black in dark rather than following `--ink`, because a shadow cast in the near-white dark `--ink` would light the app up instead of darkening it. It was called `--ink-rgb` until that name invited exactly the wrong edit.

When you need a colour, use an existing token. When you genuinely need a new one, add it to `:root` **and** to both dark blocks; the guard below fails the build if you add it to one and not the others.

### The two dark blocks, and why both are needed

Light values stay on bare `:root`, so a document with no media-query support and no override renders the app exactly as it always did. The dark values are then declared **twice**, and neither copy is redundant:

- `@media (prefers-color-scheme: dark)` **under a `:root:not([data-theme="light"])` guard** — the OS default. The guard is what lets an explicit *Light* choice win on a dark machine; without it the media query would beat the attribute.
- `:root[data-theme="dark"]` — an explicit *Dark* choice, which has to win on a **light** machine, where the media query never fires at all.

Every colour token on `:root` is re-declared in both blocks, even where the dark value is deliberately identical to the light one — the terminals and the live-status colours are the worked examples. Re-stating them is what makes the dark block a complete list of decisions rather than a diff, and it is why the guard treats a missing token as an omission instead of an intentional carry-over.

`desktop/index.html` carries the other half: `<meta name="color-scheme" content="light dark">` is what makes the webview's **own** chrome — scrollbars, form controls, the default background behind the page — follow the appearance instead of staying light under a dark document, and `theme-color` needs one element per appearance, selected by a `media` attribute.

### The `theme-invariant:` opt-out

Any line carrying `theme-invariant: <reason>` in a comment is exempt from the colour guard, for that line only. It is not a way out of doing the work — it is for colour that is genuinely outside the theme. What qualifies today: the **xterm palette** in `.ts`/`.tsx`, because the terminals stay dark in both appearances (below), and colour arriving from another branch that has not been tokenised yet — PR #779 has light hexes in flight, so whichever of the two lands second needs a note rather than a wall. What does not qualify: "it's only a shadow", "it's only a border", "it's nearly the same in both themes". A shadow tuned for a light surface is invisible on a dark one, which makes it a theme colour in the way that matters. Always write the reason; the marker without one is a `TODO` wearing a disguise.

### The terminals stay dark in both appearances

`--terminal` and the `--shell*` family are not themed, and this is a decision rather than an omission. The panes are xterm.js fed raw PTY bytes: we control the background, the foreground and the 16 ANSI slots, but not what an agent emits into them. Agent CLIs pick colours for *their* user's terminal, and two of those choices break on a light background — dim greys tuned to read on black become near-invisible, and anything emitting truecolor SGR (`\e[38;2;r;g;b m`) bypasses the 16-slot palette entirely, so remapping cannot rescue it. The failure would land on the one surface people actually work in, and it is not one we can prevent from here. So the chrome around the panes themes normally and the pane stays a dark inset — which is what it already looks like in light mode. A "terminal follows the app appearance" setting was considered and deferred: it is cheap to add to a section that now exists, and shipping it means shipping a control whose other position nobody has measured against real agent output.

### The two guards

Both live in `xtask/linkage-check/src/desktop_palette.rs` and both run in `cargo test-fast` (via `--workspace`) and in the CI `build` job, which is one of the four **required** checks. That placement is deliberate: a vitest guard would run in `desktop-web`, which is advisory and can be merged past. Run them alone with `cargo test -p xtask-linkage-check desktop_palette`.

**The colour guard** scans every source file under `desktop/src` and fails on a hard-coded colour outside the palette: hex literals (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`), the colour functions `rgb() rgba() hsl() hsla() hwb() lab() lch() oklab() oklch() color()` **unless** their arguments name a token, and the bare keywords `white` and `black`. `transparent`, `currentColor` and `inherit` are never flagged. Two things may hold a literal: a custom-property declaration inside a `:root` block of `styles.css` — that is the palette, and the exemption is custom properties specifically, so `:root { color: #1d2522; }` still fails — and any line carrying the `theme-invariant:` marker. **When it fires:** move the value onto a token (add one if no existing token has that role), or mark the line with a reason. Do not widen the guard.

**`check_dark_palette`** compares the three `:root` blocks and catches the three ways the palette can go quietly wrong: the two dark blocks drifting apart (a token declared in one with a different value, or missing from the other), a light colour token with **no** dark counterpart, and a **dark-only** token, which resolves to nothing in light mode. **When it fires:** it names each problem individually; the fix is always to make the three blocks agree, never to delete an assertion. Nothing else can see this — no test renders dark mode — so a token added to `:root` alone would otherwise ship as a colour that silently keeps its light value.

**One false positive is worth knowing about in advance**, because the failure reads as a colour problem when it is a naming one: `#` followed by three hex digits inside a **string literal** is flagged, so `"…#803…"` and `"…PR #416…"` both trip the colour guard. Comments are masked before scanning; string contents deliberately are **not**, because `"#141817"` is exactly what the guard hunts. That is correct behaviour rather than a bug, and it will recur as more strings are written under `desktop/src`. The fix is to rewrite the string — `PRD 803`, no `#` — not to touch the guard and not to reach for the opt-out.

## Models and agent profiles

The fixture seeds an orchestrator and release profile using Claude and coder, reviewer, auditor, and tester profiles using `gpt-5.6-sol` with role-appropriate reasoning effort. Editing a profile stores a draft in the webview's local storage and **Confirm draft** does not execute anything or write `.dot-agent-deck.toml`; **Launch live loop** does execute each enabled profile's launch command through the daemon after the workflow validation described above. Treat launch commands as executable configuration, never store tokens in them, and review them before starting a live loop. **Reset defaults** clears the local draft.

For OpenAI, Anthropic, and OpenCode profiles, the launch command is generated from the current provider, CLI, model, reasoning-effort, and permission fields. The generated command is a read-only preview; on macOS and Linux, values are POSIX-quoted as individual shell words and invalid, NUL-containing, blank, or oversized commands block launch. An **advanced custom command override** is available for unusual CLIs, but it is explicitly labeled as an exact shell command that bypasses the structured fields. Its permissions are unmanaged by the profile UI, may be arbitrary, and must be encoded and reviewed in the command itself. Custom roles are excluded from structured full-access counts, and a custom override does not bypass the Windows platform guard. The launch confirmation distinguishes custom-command risk from permission claims made only for generated roles. The submitted command overrides the matching project role for that launch only; format-preserving profile write-back is not implemented.

Live sessions continue to use the commands and models that created them through the daemon. The current daemon snapshot exposes agent type but not a reliable model identifier, so the live UI labels model, cost, token, branch, and lease fields as unavailable instead of guessing.

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

Run the repository-wide required fast gates from the repository root:

```sh
cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings
cargo test-fast
```

There is no full-tier obligation before a PR — CI runs the e2e tier as two lanes (CLAUDE.md rule 5, [`e2e-lanes.md`](e2e-lanes.md)). Where a desktop change touches a PTY or real-agent path, run those tests by filter:

```sh
cargo test-e2e <filter>        # lane 1, no credentials needed
cargo test-e2e-live <filter>   # lane 2, needs your own agent credentials
```

### A real agent, end to end

The live manual smoke check is: build the matching CLI, launch `pnpm tauri dev`, use **Start daemon** if needed, launch the configured live loop against a disposable project/worktree, confirm every role hydrates under one orchestration, interact with a real agent in an embedded terminal, resize the window and terminal, reconnect without duplicated output, and stop only a disposable agent through the confirmation dialog. Do not treat the fixture as proof that a real agent or daemon path works.

### Both appearances, and the settings surface

**This is the only check that covers WebKitGTK and WKWebView.** Every automated check of the theme ran in Chromium — vitest under jsdom, and the palette verification in `chrome-headless-shell` — and the app ships on WebKit: WebKitGTK on Linux, WKWebView on macOS. Two things are the most likely to differ there and neither has been exercised on WebKit: `color-scheme: light dark` on `<meta>`, which is what makes the webview's own scrollbars, form controls and default background follow the appearance, and `theme-color` with `media` attributes, where support for the `media` selector is the part that varies. There is no driver-level tier that could catch either ([#823](https://github.com/vfarcic/dot-agent-deck/issues/823)), so this walk is the whole of it.

Do the whole list twice — once with the OS set to Light and once to Dark — with `pnpm tauri dev` or a bundled build:

- **Walk every surface in both appearances.** The deck itself (agent tiles, the rail, the tab strip, the agent footer, the evidence drawer, the output reader), all four config sheets (Projects, Workflows, Profiles, Prompts), the settings sheet, the command palette (⌘K), the shortcut dialog (`?`), a confirmation dialog, the connection banner, and the failure and empty states — the fixture preview's `?state=disconnected`, `?state=error` and `?state=empty` are the cheapest way to reach the last of those, and they are exactly the surfaces least likely to be opened during development and most likely to be wrong. Nothing should be light-on-dark or dark-on-light, and no surface should stop separating from its parent.
- **Confirm the override wins in both directions.** On a dark machine choose **Light** and get a light app; on a light machine choose **Dark** and get a dark app. Both directions matter and they exercise different CSS: the media query under its `:root:not([data-theme="light"])` guard, and the `:root[data-theme="dark"]` block. Then choose **System** and confirm it follows the OS **live** — change the OS appearance with the app open and the window should follow without a restart or a reload.
- **Confirm the terminals are unchanged.** The panes stay dark in both appearances by decision. With a real agent running, its output should look exactly as it does today in light mode; the chrome around the pane themes, the pane does not.
- **Confirm the choice persists and the footer tells the truth.** Set an override, quit, relaunch, and it should still be applied — before the sheet is ever opened. The sheet's footer should name a path that exists on disk (`~/.config/dot-agent-deck/desktop.toml`) and the file should be readable and hand-editable; in the browser fixture preview the same footer must say there is **no** file rather than printing a plausible-looking path.
- **Confirm the store fails softly.** Delete `desktop.toml`, then corrupt it, then make it unreadable, launching the app each time: all three should start cleanly on defaults, with no crash and no error dialog.
