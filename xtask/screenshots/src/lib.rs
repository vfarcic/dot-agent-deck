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
