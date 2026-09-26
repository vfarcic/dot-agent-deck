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

It runs two stages:

1. **TUI capture.** `cargo nextest run --features e2e --test e2e_docs_screenshots --run-ignored only` with an exact filter for the selected scenarios. Each capture drives the real binary in the L2 PTY harness (`tests/common/mod.rs`), inside the harness's isolated sandbox: its own `HOME`, sockets, state dir and lazily spawned daemon, so it never attaches to your running deck. It launches the deck with `without_agent_credentials()`, so no agent credential is in its environment even when one is ambient on your machine (on Linux the capture reads `/proc/<pid>/environ` back to prove it), puts the scene on screen with synthetic hook events (no real agent), then writes the vt100 frame, every cell with its character, colours and attributes, as `<scenario>-tui.html` under `target/docs-screenshots/tui-html/`. That directory is emptied at the start of every run, so a stale HTML file is never rasterized.
2. **Rasterize.** Playwright runs `desktop/playwright.screenshots.config.ts` in Chromium. It screenshots each desktop scenario off the production web build (`vite build`, then `vite preview` on port 4183) and loads each TUI HTML file and screenshots its `#terminal` element. Both clients' PNGs come out of one Chromium with one set of settings. The web build runs only when the selection includes a desktop scenario: the command sets `DAD_DOCS_SCREENSHOTS_WEB=0` otherwise and the config then starts no web server, so `--client tui` never waits on a `vite build`. Running the config by hand without that variable builds.

### Why the screenshot code cannot run by accident

- The TUI captures are `#[ignore]`d, so `cargo test-e2e` and CI's `e2e-deterministic` job skip them. Each one also panics unless `DAD_DOCS_SCREENSHOTS_TUI_HTML` is set, so even `--run-ignored all` writes nothing. Only `cargo docs-screenshots` sets it.
- The desktop captures live under `desktop/screenshots/` with a `*.shot.ts` suffix, and only `playwright.screenshots.config.ts` looks there. `pnpm test:browser` uses `playwright.config.ts`, whose `testDir` is `./e2e`.

## Adding a scenario

1. Add an entry to `SCENARIOS` in `xtask/screenshots/src/scenarios.rs`: a kebab-case `name`, a one-line `description`, and the `clients` it is captured from.
2. For the TUI, add an `#[ignore]`d test named `docs_screenshot_<name>` (with `-` spelled `_`) to `tests/e2e_docs_screenshots.rs`. Launch with the file's `launch()`, drive the deck to the state you want with `send`, `send_keys` and the harness's waits, and finish with `capture(&deck, "<name>", |grid| …)`. The closure is the readiness check: it runs under the parser lock against the same frame that gets written, so make it name everything the image must show.
3. For the desktop, add a `desktopScenario("<name>", async (page) => { … })` call to `desktop/screenshots/desktop.shot.ts`. Load a fixture state (`/?fixture=1&state=…`; the states are listed in `desktop/src/data/fixture.ts`), navigate, and end on a state wait such as `expect(locator).toBeVisible()`. Never wait on a timer.
4. Run `cargo docs-screenshots --scenario <name>`, look at the images, and commit them.

`cargo test-fast` checks the registry against both capture files: a scenario registered without a capture, or a capture that is not registered, fails `every_tui_scenario_has_exactly_one_capture_and_vice_versa` or its desktop twin.

A feature both clients have should get the **same** scenario name on both, so the docs can show the two images as TUI | Desktop tabs, and the two images should depict **the same state**. `dashboard` is the worked example: `DASHBOARD_AGENTS` in `tests/e2e_docs_screenshots.rs` and `docsAgents` in `desktop/src/data/fixture.ts` (the fixture's `docs` state, `/?fixture=1&state=docs`) carry the same names, agent types, working directory (`/home/dev/storefront`), prompts, active tools and ages, and every desktop agent has an uptime so that column is not blank. Change the two lists together.

Give a docs scenario its own fixture state rather than reusing or editing a shared one: `connected` is what the desktop unit and snapshot tests are written against, and it carries demo-run paths such as `/dev/active/dot-agent-deck-gui` that do not belong in docs.

Statuses do not have the same vocabulary in both clients, so give each desktop agent the status live mode would show for the TUI's state, from `DAEMON_STATUS` in `desktop/src/lib/bridge.ts`:

| TUI card status (the hook event `dashboard` sends for it) | daemon status | desktop status |
| --- | --- | --- |
| Working (`tool_start`) | `working` | `running` (RUNNING) |
| Needs Input (`waiting_for_input`) | `waiting_for_input` | `waiting` (WAITING) |
| Idle (`idle`) | `idle` | `waiting` (WAITING) |

The desktop folds Idle and Needs Input into one status, so the two images of `dashboard` read 2 working / 1 waiting / 1 idle in the TUI and 2 running / 2 waiting on the desktop. That is the same state, not a mismatch.

**An agent-backed scenario must redact before it writes anything.** Today's scenes run no agent, so no frame can hold a secret. A scenario added later that runs a real agent in a pane must replace sensitive cells (credentials, tokens, real home paths, anything from the agent's environment) before it writes the HTML and the PNGs, because both are committed and published.

## How the output is made deterministic

Measured on 2026-09-26: two consecutive full runs on one Linux machine produced byte-identical PNGs for all four images (identical `sha256sum` and `cmp`), and a third run into `docs/img/` matched them too. That is the claim this section supports; the determinism across different machines is covered under the limits below.

Common to both clients:

- One engine, Chromium, the build `@playwright/test` pins. WebKit stays in the test tier, where it earns its place because the app ships on it.
- A fixed viewport of 1280×800 CSS pixels, `deviceScaleFactor: 2`, the `en-US` locale, the `UTC` timezone, `colorScheme: "dark"` and `reducedMotion: "reduce"`.
- Screenshots taken with `animations: "disabled"` and `caret: "hide"`, after `document.fonts.ready`, by one worker.

Desktop:

- Fixture data only, with fixed names. Scenario states the TUI also shows use a docs-only fixture state (see [Adding a scenario](#adding-a-scenario)). The fixture's "DEMO DATA" banner is hidden in the image through the screenshot's `style` option, because it tells a developer the screen is not a live deck, which is not something a docs reader needs.
- The clock is frozen with `page.clock.setFixedTime` at 2026-09-01T12:00:00Z before the page loads. The fixture computes ages as `Date.now()` minus a fixed number of minutes and the overview prints them relative to `Date.now()`, so every age reads the same on every run.
- The pointer is parked at the bottom-left corner of the viewport before the shot, so no hover tooltip from the last click is in the picture.

TUI:

- A fixed 120×32 terminal and a fixed palette (`Palette::default()` in `xtask/screenshots/src/terminal_html.rs`), 14px font, line height 1.2. At that line height DejaVu Sans Mono's box-drawing glyphs join up vertically.
- Synthetic hook events are sent one at a time, each confirmed on screen before the next, because the daemon handles each hook connection on its own task and would otherwise apply them in no fixed order. Card order and state are then the same every run.
- The TUI's clock cannot be frozen from outside the binary, so every agent's age is a whole number of hours and minutes: the card prints `1h 5m` from an hour up and `5m 12s` below it, so only hour-scale ages survive the seconds a capture takes. The events are stamped at the start of a minute and the readiness check names the exact `Last:` labels, so a capture that ever outlived the ~59 seconds before a label rolls over times out rather than writing a wrong image.
- Idle and waiting cards blink their status dot by drawing a space in its place. Nothing else on the dashboard draws a `●`, so the readiness check requires exactly one per agent, which means every dot is lit; because it runs against the frame that is written, the image never shows half a blink.
- The hardware cursor is not drawn.

## Known limits

- **Byte-identical output is established on one machine, not across machines.** The PNG depends on the fonts installed (DejaVu Sans Mono for the TUI; the desktop's stack is `Geist, "IBM Plex Sans", "Avenir Next", system-ui` and `"JetBrains Mono"` for code, which falls back to whatever the host has) and on the host's font rendering. A regeneration on a different machine can therefore differ in pixels while showing the same content. Commit images from one machine where you can, and treat a diff made up only of anti-aliasing changes as noise rather than as a UI change.
- **The desktop images are the web build, not a Tauri window.** It is the same UI for docs purposes; a native-window route is #953.
- **The TUI images are the vt100 model of the screen, not a terminal emulator's rendering.** Colours, bold, dim, italic, underline, inverse and wide characters are carried over; blinking text, the cursor and images inside a pane are not.
- **The TUI half is Unix-only** (see Prerequisites).
- **Only daemon-free states.** The TUI scenes are built from synthetic hook events and the desktop ones from the fixture, so a screen that needs a real agent running in a pane is not reachable yet. Adding one would mean a stand-in command under `with_continue_session`, as the mouse tests do, which the harness already supports.
