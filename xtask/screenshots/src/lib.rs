//! Docs screenshots of both clients, generated from code (issue #1322).
//!
//! - [`terminal_html`] renders a `vt100::Screen` — a frame of the real TUI as
//!   the L2 harness holds it — to a standalone HTML page.
//! - [`scenarios`] is the registry of named screenshots and the naming
//!   convention that ties each one to its capture code.
//!
//! The `xtask-screenshots` binary (`cargo docs-screenshots`) runs the captures.
//! `docs/develop/docs-screenshots.md` is the maintainer page.

pub mod scenarios;
pub mod terminal_html;

/// The environment variable the TUI capture writes its HTML into. The capture
/// refuses to run without it, which is what keeps a stray
/// `--run-ignored all` from writing anywhere at all.
pub const TUI_HTML_DIR_ENV: &str = "DAD_DOCS_SCREENSHOTS_TUI_HTML";

/// The environment variable naming the directory the PNGs land in.
pub const OUT_DIR_ENV: &str = "DAD_DOCS_SCREENSHOTS_OUT";

/// The environment variable telling `desktop/playwright.screenshots.config.ts`
/// whether this run needs the web build. `cargo docs-screenshots` sets it to
/// `0` when no desktop scenario is selected, so a `--client tui` run does not
/// pay for a `vite build` whose output nothing loads. Any other value, or
/// none, builds, which keeps a hand-run of the config correct by default.
pub const WEB_BUILD_ENV: &str = "DAD_DOCS_SCREENSHOTS_WEB";

/// The environment variable carrying the localhost port the desktop leg's
/// `vite preview` binds and Playwright's `baseURL` points at.
/// `cargo docs-screenshots` picks a free one per invocation that serves the web
/// build (and sets none for a terminal-only run, which starts no server), so
/// two concurrent runs on one machine never share a server; the config keeps
/// `--strictPort`, so a run that loses the race for its port fails rather than
/// screenshotting somebody else's bundle. Unset — a hand-run of the config —
/// uses 4183.
pub const PORT_ENV: &str = "DAD_DOCS_SCREENSHOTS_PORT";

/// The environment variable naming this invocation's private scratch directory
/// (`target/docs-screenshots/run-<pid>`). The config puts the web build it
/// serves and Playwright's own output there, so a concurrent run's `vite build`
/// cannot empty the bundle this run is serving. Unset — a hand-run of the
/// config — keeps vite's default `dist/` and a `test-results/` under
/// `desktop/`.
pub const RUN_DIR_ENV: &str = "DAD_DOCS_SCREENSHOTS_RUN_DIR";
