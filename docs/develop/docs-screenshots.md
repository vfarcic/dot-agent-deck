# Docs screenshots from code

`cargo docs-screenshots` regenerates the docs screenshots of both clients, the TUI and the desktop app, from named scenarios into `docs/img/` (issue #1322). It exists so that a screenshot made stale by a UI change is a command away rather than a manual session. It decides nothing about *which* screenshots the docs need; that belongs to the docs work that uses it (#1321).

`docs/img` is a symlink to `site/static/img`, so the files land in `site/static/img/` and are committed there.

## Prerequisites

- The Rust toolchain and `cargo-nextest` from `devbox.json`, the same ones `cargo test-fast` needs.
- The desktop's Node dependencies and Playwright's Chromium: `pnpm install` and then `pnpm exec playwright install chromium`, both in `desktop/`. The Chromium build is the one `@playwright/test` pins in `desktop/package.json`.
- **DejaVu Sans Mono** for the TUI images. The renderer's font stack is `"DejaVu Sans Mono", "Liberation Mono", monospace`, and the committed images were made with DejaVu Sans Mono; `fc-match "DejaVu Sans Mono"` says whether it is installed. A host without it renders in the next family that is, and the images change.
- A Unix host for the TUI half. `tests/e2e_docs_screenshots.rs` is `#![cfg(all(feature = "e2e", unix))]` because the harness injects hook events over a Unix socket, so on Windows only `--client desktop` works.

## The command

```bash
cargo docs-screenshots --list                          # the scenarios and their clients
cargo docs-screenshots                                 # every scenario, both clients, into docs/img
cargo docs-screenshots --scenario dashboard            # one scenario (repeatable)
cargo docs-screenshots --client tui                    # one client (repeatable): tui or desktop
cargo docs-screenshots --out ../scratch/shots          # anywhere else, e.g. to compare two runs
```

Each image is named `<scenario>-<client>.png`, so the `dashboard` scenario produces `docs/img/dashboard-tui.png` and `docs/img/dashboard-desktop.png`. The command prints every file it wrote and fails if an expected one is missing.

## Registered scenes

| Scenario | Clients | Screen |
| --- | --- | --- |
| `dashboard` | TUI, desktop | Four agents in mixed states. |
| `dashboard-empty` | TUI, desktop | First-run empty state. |
| `dashboard-fleet` | desktop | Agent dashboard with six agents across two connected daemons. |
| `new-agent` | TUI, desktop | New Agent form with a project directory chosen. |
| `orchestration` | TUI, desktop | Activated `demo-loop` with planner and builder roles. |
| `agent-pane` | desktop | Agent pane over the Dashboard, showing a fixed implementation transcript. |
| `settings-daemons` | desktop | Daemons settings with one configured remote. The browser fixture cannot produce a successful Test connection result. |
| `settings-voice` | desktop | Voice settings. |
| `voice-typing-mode` | desktop | Agent pane with voice typing mode on, entered through the fixture's scripted microphone (`?voice=type%20on`). |
| `schedules` | TUI | Schedules manager with one disabled task, keeping the next-fire field stable. |
| `help` | TUI | The `?` keyboard shortcut overlay. |

The docs-only fleet fixture lives in `desktop/src/data/fixture.ts` and is selected by `/?fixture=1&state=docs-fleet`.

It runs two stages:

1. **TUI capture.** `cargo nextest run --features e2e --test e2e_docs_screenshots --run-ignored only` with an exact filter for the selected scenarios. Each capture drives the real binary in the L2 PTY harness (`tests/common/mod.rs`), inside the harness's isolated sandbox: its own `HOME`, sockets, state dir and lazily spawned daemon, so it never attaches to your running deck. It launches the deck with `without_agent_credentials()`, so no agent credential is in its environment even when one is ambient on your machine (on Linux the capture reads `/proc/<pid>/environ` back to prove it), puts the scene on screen with stand-in commands in real panes and synthetic hook events (no real agent), then writes the vt100 frame, every cell with its character, colours and attributes, as `<scenario>-tui.html` under this invocation's own directory, `target/docs-screenshots/run-<pid>/tui-html/`. That directory is created empty (a leftover from a dead run that had the same pid is cleared first), so a stale HTML file is never rasterized, and the whole `run-<pid>/` directory is removed when the command exits, whether it succeeded or failed. A run that is killed, `Ctrl+C` included, leaves its directory behind; nothing reuses it except a later run that happens to get the same pid, which clears it.
2. **Rasterize.** Playwright runs `desktop/playwright.screenshots.config.ts` in Chromium. It screenshots each desktop scenario off the production web build (`vite build` into `run-<pid>/web/`, then `vite preview` of that directory on a free localhost port the command picks for this invocation; a run with no desktop scenario starts no server and picks no port) and loads each TUI HTML file and screenshots its `#terminal` element. Both clients' PNGs come out of one Chromium with one set of settings. The web build runs only when the selection includes a desktop scenario: the command sets `DAD_DOCS_SCREENSHOTS_WEB=0` otherwise and the config then starts no web server, so `--client tui` never waits on a `vite build`. Running the config by hand without that variable builds. The port reaches the config as `DAD_DOCS_SCREENSHOTS_PORT` and the run directory as `DAD_DOCS_SCREENSHOTS_RUN_DIR`; a hand-run without them serves vite's default `dist/` on port 4183 and writes Playwright's output under `desktop/test-results/docs-screenshots/`.

The target directory is the one cargo reports: the command runs `cargo metadata --format-version 1 --no-deps` from the directory you ran it from and reads its `target_directory`, so `CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR` and a `[build] target-dir` in a project or user `.cargo/config.toml` all resolve as they do for any cargo command run there, relative values included. If that call fails the command stops with cargo's error rather than guessing `<repo>/target`. Both stages are handed the absolute path it reports: they run from different directories (the TUI capture from the repo root, Playwright from `desktop/`), so a relative path passed through as-is would name two different places, and the nested `cargo nextest` gets it as `CARGO_TARGET_DIR` so it builds where the command did.

### Concurrent runs

Two `cargo docs-screenshots` invocations can run at once on one machine, from one worktree or from several sharing a box, as long as they write to different `--out` directories. Each has its own `run-<pid>/` directory for the TUI HTML, the web build and Playwright's output, and, when it serves the web build, its own port. The config keeps `--strictPort`, so if another process takes the port between the command choosing it and `vite preview` binding it, the run fails instead of screenshotting another run's server; rerun it. The TUI captures share the target dir's build like any two cargo commands, so the second one waits on cargo's build lock rather than building twice. Two runs at once into the same `--out` would each overwrite the other's PNGs; with the same inputs they write the same bytes, but nothing orders the writes. Measured on 2026-09-26: two concurrent full runs into two `--out` directories both succeeded and wrote PNGs byte-identical to each other and to a sequential run.

### Why the screenshot code cannot run by accident

- The TUI captures are `#[ignore]`d, so `cargo test-e2e` and CI's `e2e-deterministic` job skip them. Each one also panics unless `DAD_DOCS_SCREENSHOTS_TUI_HTML` is set, so even `--run-ignored all` writes nothing. Only `cargo docs-screenshots` sets it.
- The desktop captures live under `desktop/screenshots/` with a `*.shot.ts` suffix, and only `playwright.screenshots.config.ts` looks there. `pnpm test:browser` uses `playwright.config.ts`, whose `testDir` is `./e2e`.

## Adding a scenario

1. Add an entry to `SCENARIOS` in `xtask/screenshots/src/scenarios.rs`: a kebab-case `name`, a one-line `description`, and the `clients` it is captured from.
2. For the TUI, add an `#[ignore]`d test named `docs_screenshot_<name>` (with `-` spelled `_`) to `tests/e2e_docs_screenshots.rs`. Launch with the file's `launch()` (or `launch_with` to adjust the builder), drive the deck to the state you want with `send`, `send_keys` and the harness's waits, and finish with `capture(&deck, "<name>", |grid| …)`. The closure is the readiness check: it runs under the parser lock against the same frame that gets written, so make it name everything the image must show, pane content included.

   A scene that shows agents should show them in panes, because that is what a user sees: with no pane open the dashboard gives the whole screen to the card grid, a layout no user with a running agent ever gets. `docs_screenshot_dashboard` is the pattern. It launches with `with_launch_subdir` (so the new-pane form offers a directory with a fixed name) and `impersonating_pane_signals()` (the hook events come from the test process, which cannot present a pane's capability), opens each pane with `open_pane` — `Ctrl+N`, the directory picker, then the form's Name and Command, as a user does — and runs a **stand-in, never a real agent**: `STAND_IN_SCRIPT`, one fixed script that `write_stand_ins` writes once and every pane runs with two positional arguments, the file to record the environment's variable names in and the transcript to print. It records names only, through awk's `ENVIRON`, which splits each environment entry on its first `=`, so no value, not even a fragment of one that contains a newline, is ever written to disk; the capture reads the names back and asserts no agent credential reached the pane. It then prints a fixed transcript with no timestamps, pids or host paths, and sleeps. No path is spliced into the script, and the pane command quotes each path with `shell_quote` (single quotes, each `'` spelled `'\''`), because the harness temp root comes from `DAD_E2E_TMPDIR` and may contain anything; `shell_quote_keeps_hostile_paths_one_word` runs quoted apostrophes, spaces, `$(…)`, backticks and newlines through `sh` to check it. Measured on 2026-09-26: with `DAD_E2E_TMPDIR` set to a directory named `it's a $(test)`, the previous unquoted command failed in the pane with `Unterminated quoted string` and the current one wrote PNGs byte-identical to the committed ones. The hook events then address each pane by the `pane_id_env` and registry id the daemon reports for it, each confirmed through `ListAgents` before the next. Leave `Ctrl+T` alone: the stacked/tiled toggle is being retired (PRD #312), so a scene must not depend on it.
3. For the desktop, add a `desktopScenario("<name>", async (page) => { … })` call to `desktop/screenshots/desktop.shot.ts`. Load a fixture state (`/?fixture=1&state=…`; the states are listed in `desktop/src/data/fixture.ts`), navigate, and end on a state wait such as `expect(locator).toBeVisible()`. Never wait on a timer.
4. Run `cargo docs-screenshots --scenario <name>`, look at the images, and commit them.

`cargo test-fast` checks the registry against both capture files: a scenario registered without a capture, or a capture that is not registered, fails `every_tui_scenario_has_exactly_one_capture_and_vice_versa` or its desktop twin.

A feature both clients have should get the **same** scenario name on both, so the docs can show the two images as TUI | Desktop tabs, and the two images should depict **the same state**. `dashboard` is the worked example: `DASHBOARD_AGENTS` in `tests/e2e_docs_screenshots.rs` and `docsAgents` in `desktop/src/data/fixture.ts` (the fixture's `docs` state, `/?fixture=1&state=docs`) carry the same names, agent types, working directory (`/home/dev/storefront`), prompts, active tools and ages, and every desktop agent has an uptime so that column is not blank. The depicted state is agents up for hours, each active seconds ago. The TUI capture stamps each agent's `session_start` `up_for_minutes` back, and the fixture's `DOCS_UP_MINUTES` feeds the desktop's Uptime column from the same numbers. It stamps each status event `quiet_for_secs` before the capture, which the cards show as `Last:`, and the fixture's `DOCS_QUIET_MINUTES` is `0`, so the desktop's Last activity column (not shown by default) reads `just now`. The TUI cannot show hour-scale `Last:` ages here: a card's last activity is a high-water mark that starts when its pane is spawned, and the capture spawns the panes. Change the two lists together.

Give a docs scenario its own fixture state rather than reusing or editing a shared one: `connected` is what the desktop unit and snapshot tests are written against, and it carries demo-run paths such as `/dev/active/dot-agent-deck-gui` that do not belong in docs.

Statuses do not have the same vocabulary in both clients, so give each desktop agent the status live mode would show for the TUI's state, from `DAEMON_STATUS` in `desktop/src/lib/bridge.ts`:

| TUI card status (the hook event `dashboard` sends for it) | daemon status | desktop status |
| --- | --- | --- |
| Working (`tool_start`) | `working` | `running` (RUNNING) |
| Needs Input (`waiting_for_input`) | `waiting_for_input` | `waiting` (WAITING) |
| Idle (`idle`) | `idle` | `waiting` (WAITING) |

The desktop folds Idle and Needs Input into one status, so the two images of `dashboard` read 2 working / 1 waiting / 1 idle in the TUI and 2 running / 2 waiting on the desktop. That is the same state, not a mismatch.

**An agent-backed scenario must redact before it writes anything.** Today's scenes run no agent, and their panes print only the fixed transcripts in the capture file, so no frame can hold a secret. A scenario added later that runs a real agent in a pane must replace sensitive cells (credentials, tokens, real home paths, anything from the agent's environment) before it writes the HTML and the PNGs, because both are committed and published.

## How the output is made deterministic

Measured on 2026-09-26: two consecutive full runs on one Linux machine produced byte-identical PNGs for all four images (identical `sha256sum` and `cmp`), and a third run into `docs/img/` matched them too. Re-measured the same day after `dashboard` moved to panes: two sequential runs, two concurrent runs into separate `--out` directories and a run into `docs/img/` all produced the same `sha256sum` for all four images. That is the claim this section supports; the determinism across different machines is covered under the limits below.

Common to both clients:

- One engine, Chromium, the build `@playwright/test` pins. WebKit stays in the test tier, where it earns its place because the app ships on it.
- A fixed viewport of 1280×800 CSS pixels, `deviceScaleFactor: 2`, the `en-US` locale, the `UTC` timezone, `colorScheme: "dark"` and `reducedMotion: "reduce"`.
- Screenshots taken with `animations: "disabled"` and `caret: "hide"`, after `document.fonts.ready`, by one worker.

Desktop:

- Fixture data only, with fixed names. Scenario states the TUI also shows use a docs-only fixture state (see [Adding a scenario](#adding-a-scenario)). The fixture's "DEMO DATA" banner is hidden in the image through the screenshot's `style` option, because it tells a developer the screen is not a live deck, which is not something a docs reader needs.
- The clock is frozen with `page.clock.setFixedTime` at 2026-09-01T12:00:00Z before the page loads. The fixture computes ages as `Date.now()` minus a fixed number of minutes and the overview prints them relative to `Date.now()`, so every age reads the same on every run.
- The pointer is parked at the bottom-left corner of the viewport before the shot, so no hover tooltip from the last click is in the picture.

TUI:

- A fixed 180×40 terminal and a fixed palette (`Palette::default()` in `xtask/screenshots/src/terminal_html.rs`), 14px font, line height 1.2. At that line height DejaVu Sans Mono's box-drawing glyphs join up vertically. 180 columns is what the dashboard's card column, a third of the width once panes are open, needs to show every card's name, status and prompt without truncating them.
- Synthetic hook events are sent one at a time, each confirmed on screen before the next, because the daemon handles each hook connection on its own task and would otherwise apply them in no fixed order. Card order and state are then the same every run.
- The panes run stand-ins that print fixed transcripts, so a pane's content is the same on every run. `dashboard` focuses the pane it shows explicitly rather than relying on which pane was opened last.
- The TUI's clock cannot be frozen from outside the binary, and a card's `Last:` age cannot read older than its pane, so `dashboard`'s ages are seconds and every status event is stamped on a whole second, after the last pane opened: at a chosen capture second minus the agent's age. Every label then rolls over at the same instant, and during the capture second they read exactly the agents' ages. The readiness check names those labels, so the frame is taken inside that second and never outside it. Until then the stamps are in the future and the cards read `Last: 0s`; the wait is the largest age plus about two seconds, sampled from one clock reading that also dates each agent's `session_start`. An attempt that misses the second, because staging ran past it or because no frame inside it had every status dot lit, is retried by building the **whole scene again in a fresh sandbox**, up to `DASHBOARD_ATTEMPTS` (3) times; the last attempt does not give up early and fails with the harness's timeout panic and the final grid. An attempt counts as missed once the wall clock reaches the capture second plus two, leaving a frame drawn late in the second time to arrive. It does not re-stamp the existing cards, because the events that move a card's `Last:` also add to what the card shows: a second `tool_start` draws a second tool line, and a second event carrying the prompt a second prompt line, so a re-stamped scene would not be the same image. Measured on 2026-09-26 with the first attempt forced to miss: the second attempt wrote HTML byte-identical to an unforced run's.
- `orchestration` uses the same pane-addressed hook events, whole-second ages and fresh-sandbox retry for its planner and builder cards. Its capture second is 18 seconds after staging so the activation banner's 15-second lifetime has expired; the readiness check requires both working cards, their prompts and tools, exact `Last: 2s` and `Last: 3s` labels, and no activation banner or `No agent` placeholders.
- Idle and waiting cards blink their status dot by drawing a space in its place. Nothing else on the dashboard draws a `●` (the transcripts are written without one), so the readiness check requires exactly one per agent, which means every dot is lit; because it runs against the frame that is written, the image never shows half a blink.
- The command-mode banner over the focused pane (`COMMAND MODE — Ctrl+D to type`) clears on its own after a moment, and the readiness check requires it gone. The pane stays dimmed, as command mode draws it.
- The hardware cursor is not drawn.

## Known limits

- **Byte-identical output is established on one machine, not across machines.** The PNG depends on the fonts installed (DejaVu Sans Mono for the TUI; the desktop's stack is `Geist, "IBM Plex Sans", "Avenir Next", system-ui` and `"JetBrains Mono"` for code, which falls back to whatever the host has) and on the host's font rendering. A regeneration on a different machine can therefore differ in pixels while showing the same content. Commit images from one machine where you can, and treat a diff made up only of anti-aliasing changes as noise rather than as a UI change.
- **The desktop images are the web build, not a Tauri window.** It is the same UI for docs purposes; a native-window route is #953.
- **The TUI images are the vt100 model of the screen, not a terminal emulator's rendering.** Colours, bold, dim, italic, underline, inverse and wide characters are carried over; blinking text, the cursor and images inside a pane are not.
- **The TUI half is Unix-only** (see Prerequisites).
- **No real agent.** The TUI scenes run stand-in commands in their panes and describe them with synthetic hook events, and the desktop ones come from the fixture, so a screen that needs a real agent's own output is not reachable. Adding one would also need the redaction step above.
- **`dashboard`'s `Last:` ages are seconds, not hours**, for the reason under [How the output is made deterministic](#how-the-output-is-made-deterministic).
- **A missed capture second costs a whole scene.** The retry rebuilds the sandbox and reopens every pane (about ten seconds on a quiet box) rather than re-stamping the cards, for the reason under [How the output is made deterministic](#how-the-output-is-made-deterministic). Three misses in a row fail the run; rerun it.
- **The button bar still offers `[Toggle Layout Ctrl+T]`**, because the binary draws it. The scenes never press it; removing it is PRD #312's job.
