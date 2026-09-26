#![cfg(all(feature = "e2e", unix))]

//! Issue #1322 — the TUI half of the docs screenshots. Tooling, not a test.
//!
//! Each `docs_screenshot_<scenario>` function drives the REAL binary in the L2
//! PTY harness — an isolated sandbox with its own HOME, sockets, state dir and
//! lazily-spawned daemon, so it never sees the developer's running deck — puts
//! a fixed scene on screen with synthetic hook events (no real agent, no
//! credential), and writes the vt100 frame as `<scenario>-tui.html` via
//! `xtask_screenshots::terminal_html`. `cargo docs-screenshots` then
//! rasterizes the HTML to `docs/img/<scenario>-tui.png` in Playwright's
//! Chromium, next to the desktop images.
//!
//! Every function is `#[ignore]`d, so `cargo test-e2e` never runs one, and
//! refuses to run without `DAD_DOCS_SCREENSHOTS_TUI_HTML`, so even a
//! `--run-ignored all` writes nothing. Only `cargo docs-screenshots` sets it.
//!
//! The scenario names must match `xtask/screenshots/src/scenarios.rs`; a unit
//! test there fails when they drift. `docs/develop/docs-screenshots.md` is the
//! maintainer page.

mod common;

use std::path::PathBuf;

use chrono::{Duration as ChronoDuration, Utc};
use common::{TuiDeck, write_hook_line};
use xtask_screenshots::TUI_HTML_DIR_ENV;
use xtask_screenshots::terminal_html::{RenderOptions, render_page};

/// The terminal every TUI screenshot is taken at. Fixed, so the image size and
/// the layout (two card columns at this width) never depend on the host.
const COLS: u16 = 120;
const ROWS: u16 = 32;

fn html_dir() -> PathBuf {
    let dir = std::env::var_os(TUI_HTML_DIR_ENV).unwrap_or_else(|| {
        panic!(
            "{TUI_HTML_DIR_ENV} is not set: this is the docs-screenshot generator, \
             run it with `cargo docs-screenshots`"
        )
    });
    PathBuf::from(dir)
}

/// Launch the deck for a capture. No agent credential reaches it: these
/// scenes need no real agent, and their frames are written out as HTML and
/// PNGs, so an ambient `ANTHROPIC_API_KEY` has no business in the process
/// that draws them. On Linux the launched environment is read back to prove
/// it, since the harness would otherwise pass that key through by default.
fn launch() -> TuiDeck {
    let deck = TuiDeck::builder()
        .with_pty_size(COLS, ROWS)
        .without_success_recording()
        .without_agent_credentials()
        .launch_with_fixture("minimal");
    #[cfg(target_os = "linux")]
    {
        let pid = deck
            .child_pid()
            .expect("the PTY backend reports the deck's pid");
        let environ = std::fs::read(format!("/proc/{pid}/environ"))
            .unwrap_or_else(|e| panic!("read the deck's environment: {e}"));
        for entry in environ.split(|b| *b == 0) {
            for key in common::AGENT_CREDENTIAL_ENV {
                assert!(
                    !entry.starts_with(format!("{key}=").as_bytes()),
                    "{key} reached the capture's deck process"
                );
            }
        }
    }
    deck
}

/// Wait until `ready` holds for the frame, render that same frame, and write
/// it as `<scenario>-tui.html`.
fn capture(deck: &TuiDeck, scenario: &str, ready: impl Fn(&str) -> bool) {
    let page = deck.capture_screen_when(scenario, |screen| {
        ready(&screen.contents()).then(|| {
            render_page(
                screen,
                &format!("{scenario} (TUI)"),
                &RenderOptions::default(),
            )
        })
    });
    let dir = html_dir();
    std::fs::create_dir_all(&dir).expect("create the TUI HTML dir");
    let path = dir.join(format!("{scenario}-tui.html"));
    std::fs::write(&path, page).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// One agent on the dashboard, described by the hook events that put it there.
struct Agent {
    session: &'static str,
    agent_type: &'static str,
    name: &'static str,
    cwd: &'static str,
    prompt: &'static str,
    /// The event that sets the card's status, and the tool it names if any.
    status_event: &'static str,
    tool: Option<(&'static str, &'static str)>,
    /// How long ago its last event was. At least an hour on purpose: the card
    /// prints elapsed time as `1h 5m` from an hour up and as `5m 12s` below
    /// it, so only hour-scale ages read the same across the seconds a capture
    /// takes.
    quiet_for_minutes: i64,
}

/// The four agents the `dashboard` scenario shows. The desktop fixture's `docs`
/// state (`docsAgents` in `desktop/src/data/fixture.ts`) carries the same
/// names, agent types, directory, prompts, tools and ages, and the desktop
/// status each of these hook states maps to, so the TUI and desktop images of
/// this scenario depict one state. Change the two together.
const DASHBOARD_AGENTS: &[Agent] = &[
    Agent {
        session: "docs-plan",
        agent_type: "claude_code",
        name: "Plan / architecture",
        cwd: "/home/dev/storefront",
        prompt: "Map the checkout flow and propose a retry design.",
        status_event: "idle",
        tool: None,
        quiet_for_minutes: 135,
    },
    Agent {
        session: "docs-impl",
        agent_type: "codex",
        name: "Desktop implementation",
        cwd: "/home/dev/storefront",
        prompt: "Add the retry action to the checkout view.",
        status_event: "tool_start",
        tool: Some(("Edit", "src/components/RetryPayment.tsx")),
        quiet_for_minutes: 65,
    },
    Agent {
        session: "docs-review",
        agent_type: "claude_code",
        name: "Contract review",
        cwd: "/home/dev/storefront",
        prompt: "Check the payment API for breaking changes.",
        status_event: "tool_start",
        tool: Some(("Bash", "cargo test checkout_retry")),
        quiet_for_minutes: 70,
    },
    Agent {
        session: "docs-verify",
        agent_type: "open_code",
        name: "User-path verification",
        cwd: "/home/dev/storefront",
        prompt: "Walk the checkout path and report failures.",
        status_event: "waiting_for_input",
        tool: None,
        quiet_for_minutes: 80,
    },
];

/// The card's `Last:` text for an age in minutes, as `format_elapsed` in
/// `src/ui.rs` prints it at hour scale.
fn elapsed_label(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if m == 0 {
        format!("{h}h")
    } else {
        format!("{h}h {m}m")
    }
}

fn send(deck: &TuiDeck, event: serde_json::Value) {
    write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write a hook event to the sandbox's hook socket");
}

/// Scenario: Launch the deck in a sandbox and give it four synthetic agents
/// with fixed names, prompts, tools and hour-scale ages through hook events,
/// one event at a time and each confirmed on screen before the next, so card
/// order and state are the same every run. Once every card shows its name, its
/// tool and its `Last:` age, and every status dot is lit, the frame is written
/// as `dashboard-tui.html`.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_dashboard() {
    html_dir();
    let deck = launch();
    deck.wait_for_string("No active sessions");

    // Taken once, after launch, and every age measured back from it. Each age
    // is then a whole number of minutes plus the few seconds the capture takes,
    // so every `Nh Mm` label holds for the ~59 seconds before its next roll —
    // and the `ready` check below names the exact labels, so a capture that
    // somehow outlived that window times out rather than writing a wrong one.
    let base = Utc::now() - ChronoDuration::seconds(1);
    for agent in DASHBOARD_AGENTS {
        let at = base - ChronoDuration::minutes(agent.quiet_for_minutes);
        let timestamp = at.to_rfc3339();
        send(
            &deck,
            serde_json::json!({
                "session_id": agent.session,
                "agent_type": agent.agent_type,
                "event_type": "session_start",
                "timestamp": timestamp,
                "cwd": agent.cwd,
                "metadata": { "display_name": agent.name },
            }),
        );
        deck.wait_for_string(agent.name);
        let mut event = serde_json::json!({
            "session_id": agent.session,
            "agent_type": agent.agent_type,
            "event_type": agent.status_event,
            "timestamp": timestamp,
            "cwd": agent.cwd,
            "user_prompt": agent.prompt,
        });
        if let Some((tool, detail)) = agent.tool {
            event["tool_name"] = tool.into();
            event["tool_detail"] = detail.into();
        }
        send(&deck, event);
        deck.wait_for_string(agent.prompt);
    }

    let lasts: Vec<String> = DASHBOARD_AGENTS
        .iter()
        .map(|a| format!("Last: {}", elapsed_label(a.quiet_for_minutes)))
        .collect();
    capture(&deck, "dashboard", |grid| {
        DASHBOARD_AGENTS.iter().all(|a| {
            grid.contains(a.name) && a.tool.is_none_or(|(_, detail)| grid.contains(detail))
        }) && lasts.iter().all(|l| grid.contains(l.as_str()))
            // Idle and waiting cards blink their status dot by drawing a space
            // in its place (`flash_dot` in `src/ui.rs`), and nothing else on
            // this screen draws a `●`, so exactly one per card means every dot
            // is lit and the image never shows a half-blink. A future `●`
            // elsewhere on the dashboard makes this time out, never pass early.
            && grid.matches('●').count() == DASHBOARD_AGENTS.len()
    });
}

/// Scenario: Launch the deck in a sandbox with no agents and write the
/// dashboard's empty state — the `No active sessions` hint and the command-mode
/// button bar — as `dashboard-empty-tui.html`.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_dashboard_empty() {
    html_dir();
    let deck = launch();
    capture(&deck, "dashboard-empty", |grid| {
        grid.contains("No active sessions") && grid.contains("[New Pane Ctrl+N]")
    });
}
