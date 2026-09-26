#![cfg(all(feature = "e2e", unix))]

//! Issue #1322 — the TUI half of the docs screenshots. Tooling, not a test.
//!
//! Each `docs_screenshot_<scenario>` function drives the REAL binary in the L2
//! PTY harness — an isolated sandbox with its own HOME, sockets and state dir,
//! so it never sees the developer's running deck — puts a fixed scene on screen
//! (stand-in commands in real panes and synthetic hook events: no real agent,
//! no credential), and writes the vt100 frame as `<scenario>-tui.html` via
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use common::{TuiDeck, TuiDeckBuilder, write_hook_line};
use dot_agent_deck::agent_pty::AgentRecord;
use xtask_screenshots::TUI_HTML_DIR_ENV;
use xtask_screenshots::terminal_html::{RenderOptions, render_page};

/// The terminal every TUI screenshot is taken at. Fixed, so the image size and
/// the layout never depend on the host. Wide enough that the dashboard's card
/// column (a third of the width once panes are open) shows every card's full
/// name, status and prompt without truncating them.
const COLS: u16 = 180;
const ROWS: u16 = 40;

fn html_dir() -> PathBuf {
    let dir = std::env::var_os(TUI_HTML_DIR_ENV).unwrap_or_else(|| {
        panic!(
            "{TUI_HTML_DIR_ENV} is not set: this is the docs-screenshot generator, \
             run it with `cargo docs-screenshots`"
        )
    });
    PathBuf::from(dir)
}

/// Launch the deck for a capture with its own lazily-spawned daemon.
fn launch() -> TuiDeck {
    launch_with(|builder| builder)
}

/// Launch the deck for a capture, with `customize` applied to the builder. No
/// agent credential reaches it: these scenes need no real agent, and their
/// frames are written out as HTML and PNGs, so an ambient `ANTHROPIC_API_KEY`
/// has no business in the process that draws them. On Linux the launched
/// environment is read back to prove it, since the harness would otherwise
/// pass that key through by default.
fn launch_with(customize: impl FnOnce(TuiDeckBuilder) -> TuiDeckBuilder) -> TuiDeck {
    let deck = customize(
        TuiDeck::builder()
            .with_pty_size(COLS, ROWS)
            .without_success_recording()
            .without_agent_credentials(),
    )
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

/// One agent on the dashboard: the pane it runs in and the hook events that
/// describe it.
struct Agent {
    session: &'static str,
    agent_type: &'static str,
    name: &'static str,
    cwd: &'static str,
    prompt: &'static str,
    /// The event that sets the card's status, and the tool it names if any.
    status_event: &'static str,
    tool: Option<(&'static str, &'static str)>,
    /// How long ago the agent started, in minutes: its `session_start` is
    /// stamped this far back. No TUI card shows it (a card's clock starts with
    /// its pane, see `quiet_for_secs`), but it is the agent's uptime in the
    /// depicted state, and the desktop fixture's `DOCS_UP_MINUTES` feeds its
    /// Uptime column from this value.
    up_for_minutes: i64,
    /// How long ago its last event was, in whole seconds: the card's `Last:`
    /// label. Seconds rather than the hour-scale ages a hook-only card can show
    /// because a card's last activity never reads older than its pane, and the
    /// pane is opened during the capture (the deck mints the pane's card at
    /// spawn and keeps `last_activity` a high-water mark, `AppState::apply_event`
    /// in `src/state.rs`). How those labels stay the same on every run is in
    /// `docs_screenshot_dashboard`.
    quiet_for_secs: i64,
    /// What the stand-in in the agent's pane prints: a fixed, agent-like
    /// transcript with no timestamps, pids or host paths in it, so the pane
    /// reads the same on every run. Plain ANSI SGR for colour. It must not
    /// contain `●`, which the readiness check counts on the cards.
    transcript: &'static str,
}

/// The four agents the `dashboard` scenario shows. The desktop fixture's `docs`
/// state (`docsAgents` in `desktop/src/data/fixture.ts`) carries the same
/// names, agent types, directory, prompts, tools and ages, and the desktop
/// status each of these hook states maps to, so the TUI and desktop images of
/// this scenario depict one state. Change the two together. The transcripts are
/// the TUI's alone: the desktop overview shows no pane content.
const DASHBOARD_AGENTS: &[Agent] = &[
    Agent {
        session: "docs-plan",
        agent_type: "claude_code",
        name: "Plan / architecture",
        cwd: "/home/dev/storefront",
        prompt: "Map the checkout flow and propose a retry design.",
        status_event: "idle",
        tool: None,
        up_for_minutes: 135,
        quiet_for_secs: 24,
        transcript: concat!(
            "\x1b[1m> Map the checkout flow and propose a retry design.\x1b[0m\r\n",
            "\r\n",
            "  \x1b[36mRead\x1b[0m src/checkout/flow.ts\r\n",
            "  \x1b[36mRead\x1b[0m src/api/payments.ts\r\n",
            "\r\n",
            "  Checkout submits the payment once and treats any failure as final.\r\n",
            "  Proposed retry design:\r\n",
            "\r\n",
            "  1. Send an idempotency key with every payment attempt.\r\n",
            "  2. Retry timeouts and 5xx responses up to three times, with backoff.\r\n",
            "  3. Offer a Retry action on a decline instead of an error page.\r\n",
        ),
    },
    Agent {
        session: "docs-impl",
        agent_type: "codex",
        name: "Desktop implementation",
        cwd: "/home/dev/storefront",
        prompt: "Add the retry action to the checkout view.",
        status_event: "tool_start",
        tool: Some(("Edit", "src/components/RetryPayment.tsx")),
        up_for_minutes: 65,
        quiet_for_secs: 2,
        transcript: concat!(
            "\x1b[1m> Add the retry action to the checkout view.\x1b[0m\r\n",
            "\r\n",
            "\x1b[36m\u{2022}\x1b[0m \x1b[1mExplored\x1b[0m\r\n",
            "  \x1b[2m\u{2514}\x1b[0m Read src/components/CheckoutView.tsx\r\n",
            "    Read src/api/payments.ts\r\n",
            "    Search retryPayment in src\r\n",
            "\r\n",
            "\x1b[36m\u{2022}\x1b[0m The payments API already exposes retryPayment(orderId) with an\r\n",
            "  idempotency key, so the view only needs an action that calls it and\r\n",
            "  reports the outcome.\r\n",
            "\r\n",
            "\x1b[36m\u{2022}\x1b[0m \x1b[1mEditing\x1b[0m src/components/RetryPayment.tsx\r\n",
            "  \x1b[2m 1\x1b[0m \x1b[32m+import { useState } from \"react\";\x1b[0m\r\n",
            "  \x1b[2m 2\x1b[0m \x1b[32m+import { retryPayment } from \"../api/payments\";\x1b[0m\r\n",
            "  \x1b[2m 3\x1b[0m \x1b[32m+\x1b[0m\r\n",
            "  \x1b[2m 4\x1b[0m \x1b[32m+export function RetryPayment({ orderId, onDone }: Props) {\x1b[0m\r\n",
            "  \x1b[2m 5\x1b[0m \x1b[32m+  const [pending, setPending] = useState(false);\x1b[0m\r\n",
            "  \x1b[2m 6\x1b[0m \x1b[32m+\x1b[0m\r\n",
            "  \x1b[2m 7\x1b[0m \x1b[32m+  async function retry() {\x1b[0m\r\n",
            "  \x1b[2m 8\x1b[0m \x1b[32m+    setPending(true);\x1b[0m\r\n",
            "  \x1b[2m 9\x1b[0m \x1b[32m+    await retryPayment(orderId);\x1b[0m\r\n",
            "  \x1b[2m10\x1b[0m \x1b[32m+    setPending(false);\x1b[0m\r\n",
            "  \x1b[2m11\x1b[0m \x1b[32m+    onDone();\x1b[0m\r\n",
            "  \x1b[2m12\x1b[0m \x1b[32m+  }\x1b[0m\r\n",
            "\r\n",
            "\x1b[2m\u{2026} working\x1b[0m\r\n",
        ),
    },
    Agent {
        session: "docs-review",
        agent_type: "claude_code",
        name: "Contract review",
        cwd: "/home/dev/storefront",
        prompt: "Check the payment API for breaking changes.",
        status_event: "tool_start",
        tool: Some(("Bash", "cargo test checkout_retry")),
        up_for_minutes: 70,
        quiet_for_secs: 6,
        transcript: concat!(
            "\x1b[1m> Check the payment API for breaking changes.\x1b[0m\r\n",
            "\r\n",
            "  \x1b[36mRead\x1b[0m src/api/payments.ts\r\n",
            "  \x1b[36mRead\x1b[0m docs/api/payments.md\r\n",
            "\r\n",
            "  retryPayment is new and additive; no existing field changed type.\r\n",
            "  Running the retry tests before signing off.\r\n",
            "\r\n",
            "  \x1b[36mBash\x1b[0m cargo test checkout_retry\r\n",
        ),
    },
    Agent {
        session: "docs-verify",
        agent_type: "open_code",
        name: "User-path verification",
        cwd: "/home/dev/storefront",
        prompt: "Walk the checkout path and report failures.",
        status_event: "waiting_for_input",
        tool: None,
        up_for_minutes: 80,
        quiet_for_secs: 13,
        transcript: concat!(
            "\x1b[1m> Walk the checkout path and report failures.\x1b[0m\r\n",
            "\r\n",
            "  Cart, address and shipping steps pass.\r\n",
            "  A declined card still ends on the generic error page.\r\n",
            "\r\n",
            "  \x1b[33mRe-run the payment step once the retry action lands? (y/n)\x1b[0m\r\n",
        ),
    },
];

/// The agent whose pane the `dashboard` image shows: the one at work, with a
/// diff going into its pane.
const FOCUSED_AGENT: &str = "Desktop implementation";

/// Lines of [`FOCUSED_AGENT`]'s transcript that only its pane shows, so the
/// readiness check can tell that pane is drawn and complete.
const FOCUSED_PANE_LINES: &[&str] = &[
    "Read src/components/CheckoutView.tsx",
    "+    await retryPayment(orderId);",
    "\u{2026} working",
];

/// The directory the deck is launched from, and so the one the new-pane form
/// offers and the panes run in. Named like the hook events' `cwd` so the
/// cards' `Dir:` reads the same whichever of the two it shows.
const LAUNCH_DIR: &str = "storefront";

/// Write each agent's transcript and the script its pane runs under `dir`, and
/// return the script path per agent. The script records the NAMES of the
/// variables in its environment (never a value), so the capture can prove no
/// agent credential reached the panes, prints the transcript, and then stays
/// up so the pane stays open.
fn write_stand_ins(dir: &Path) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).expect("create the stand-in dir");
    DASHBOARD_AGENTS
        .iter()
        .map(|agent| {
            let transcript = dir.join(format!("{}.txt", agent.session));
            std::fs::write(&transcript, agent.transcript).expect("write a transcript");
            let script = dir.join(format!("{}.sh", agent.session));
            std::fs::write(
                &script,
                format!(
                    "env | cut -d= -f1 > '{names}'\ncat '{transcript}'\nexec sleep 600\n",
                    names = env_names_path(dir, agent).display(),
                    transcript = transcript.display(),
                ),
            )
            .expect("write a stand-in script");
            script
        })
        .collect()
}

fn env_names_path(dir: &Path, agent: &Agent) -> PathBuf {
    dir.join(format!("{}.env-names", agent.session))
}

/// Open `agent`'s pane the way a user does: `Ctrl+N`, confirm the launch
/// directory, then the new-pane form's Name and Command. The form keeps the
/// directory's name in Name and the last command in Command, so each is
/// cleared first; `last_command` is what the previous pane was given. Returns
/// once the pane shows its transcript and the deck is back on the dashboard.
fn open_pane(deck: &TuiDeck, agent: &Agent, command: &str, last_command: &str) {
    const BACKSPACE: u8 = 0x7f;
    deck.send_keys(b"\x0e"); // Ctrl+N -> directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // confirm the launch directory -> the form
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t"); // Mode -> Name
    deck.send_keys(&vec![BACKSPACE; LAUNCH_DIR.chars().count()]);
    deck.send_keys(agent.name.as_bytes());
    deck.send_keys(b"\t"); // Name -> Command
    deck.send_keys(&vec![BACKSPACE; last_command.chars().count()]);
    deck.send_keys(command.as_bytes());
    deck.send_keys(b"\r"); // submit; the new pane takes focus
    // The transcript's first line is its prompt, which no card shows yet.
    deck.wait_for_string(&format!("> {}", agent.prompt));
    deck.send_keys(b"\x04"); // Ctrl+D -> back to the dashboard
    deck.wait_for_string("[New Pane Ctrl+N]");
}

/// The daemon's record of the pane named `name`, once it has one that
/// satisfies `ready`. The daemon handles every hook connection on its own
/// task, so the capture confirms each event here before sending the next.
fn wait_for_record(
    deck: &TuiDeck,
    name: &str,
    ready: impl Fn(&AgentRecord) -> bool,
) -> AgentRecord {
    let find = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|r| r.display_name.as_deref() == Some(name) && ready(r))
    };
    common::wait_until(Duration::from_secs(10), || find().is_some());
    find().unwrap_or_else(|| {
        panic!(
            "the daemon never reached the expected state for {name}: {:#?}",
            common::agent_records_on(deck.attach_socket_path())
        )
    })
}

fn send(deck: &TuiDeck, event: serde_json::Value) {
    write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write a hook event to the sandbox's hook socket");
}

/// Scenario: Launch the deck in a sandbox and open four panes through the
/// new-pane form, each running a stand-in that prints a fixed agent-like
/// transcript. Focus the working agent's pane and go back to the dashboard,
/// then give each agent its fixed type, prompt, tool and status through hook
/// events addressed to its pane, one at a time and each confirmed by the
/// daemon before the next. Once every card shows its name, tool and exact
/// `Last:` age with every status dot lit, and the focused pane shows its
/// transcript beside the card column, write the frame as `dashboard-tui.html`.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_dashboard() {
    html_dir();
    // The events come from this process, which is not the pane and so cannot
    // present the per-pane capability the daemon gives each pane's own agent.
    let deck = launch_with(|builder| {
        builder
            .with_launch_subdir(LAUNCH_DIR)
            .impersonating_pane_signals()
    });
    deck.wait_for_string("No active sessions");
    let stand_ins = deck.workdir().join("docs-stand-ins");
    let scripts = write_stand_ins(&stand_ins);

    let mut last_command = String::new();
    for (agent, script) in DASHBOARD_AGENTS.iter().zip(&scripts) {
        let command = format!("sh '{}'", script.display());
        open_pane(&deck, agent, &command, &last_command);
        last_command = command;
    }

    // Every stand-in wrote its environment's names before printing anything,
    // and none of them may carry an agent credential.
    for agent in DASHBOARD_AGENTS {
        let path = env_names_path(&stand_ins, agent);
        let names = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for key in common::AGENT_CREDENTIAL_ENV {
            assert!(
                !names.lines().any(|name| name == key),
                "{key} reached {}'s pane",
                agent.name
            );
        }
    }

    // The newest pane has focus. Select the focused agent's card (cards are
    // in the order the panes were opened), focus its pane with Enter, and go
    // back to the dashboard, where that pane stays the one shown.
    let newest = DASHBOARD_AGENTS.len() - 1;
    let focused = DASHBOARD_AGENTS
        .iter()
        .position(|a| a.name == FOCUSED_AGENT)
        .expect("FOCUSED_AGENT is one of DASHBOARD_AGENTS");
    deck.send_keys(&vec![b'k'; newest - focused]);
    deck.send_keys(b"\r");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");

    // The cards' `Last:` ages. A card's last activity is a high-water mark
    // that starts at its pane's spawn, so every status event is stamped after
    // the last spawn: at `capture_at` minus the agent's age, on a whole second.
    // With every stamp on a whole second, the four labels roll over together,
    // once a second, and during the second that starts at `capture_at` they
    // read exactly `quiet_for_secs` each. The `ready` check names those
    // labels, so the frame is always taken in that second, and a capture that
    // somehow missed it times out rather than writing a wrong image. Until
    // then the stamps are in the future and the cards read `Last: 0s`.
    let max_quiet = DASHBOARD_AGENTS
        .iter()
        .map(|a| a.quiet_for_secs)
        .max()
        .unwrap_or(0);
    let capture_at = Utc::now().timestamp() + 2 + max_quiet;
    let base = Utc::now();
    for agent in DASHBOARD_AGENTS {
        let record = wait_for_record(&deck, agent.name, |_| true);
        let pane_id = record
            .pane_id_env
            .clone()
            .unwrap_or_else(|| panic!("{}'s pane has no pane id", agent.name));
        let started = (base - ChronoDuration::minutes(agent.up_for_minutes)).to_rfc3339();
        let last = chrono::DateTime::from_timestamp(capture_at - agent.quiet_for_secs, 0)
            .expect("a representable instant")
            .to_rfc3339();
        send(
            &deck,
            serde_json::json!({
                "session_id": agent.session,
                "agent_type": agent.agent_type,
                "event_type": "session_start",
                "timestamp": started,
                "cwd": agent.cwd,
                "pane_id": pane_id,
                "agent_id": record.id,
                "metadata": { "display_name": agent.name },
            }),
        );
        let agent_type = serde_json::Value::from(agent.agent_type);
        wait_for_record(&deck, agent.name, |r| {
            r.live.as_ref().is_some_and(|live| {
                live.agent_type.as_ref().map(|t| serde_json::json!(t)) == Some(agent_type.clone())
            })
        });
        let mut event = serde_json::json!({
            "session_id": agent.session,
            "agent_type": agent.agent_type,
            "event_type": agent.status_event,
            "timestamp": last,
            "cwd": agent.cwd,
            "pane_id": pane_id,
            "agent_id": record.id,
            "user_prompt": agent.prompt,
        });
        if let Some((tool, detail)) = agent.tool {
            event["tool_name"] = tool.into();
            event["tool_detail"] = detail.into();
        }
        send(&deck, event);
        wait_for_record(&deck, agent.name, |r| {
            r.live
                .as_ref()
                .is_some_and(|live| live.last_user_prompt.as_deref() == Some(agent.prompt))
        });
    }

    let lasts: Vec<String> = DASHBOARD_AGENTS
        .iter()
        .map(|a| format!("Last: {}s ", a.quiet_for_secs))
        .collect();
    capture(&deck, "dashboard", |grid| {
        DASHBOARD_AGENTS.iter().all(|a| {
            grid.contains(a.name)
                && grid.contains(a.prompt)
                && a.tool.is_none_or(|(_, detail)| grid.contains(detail))
        }) && lasts.iter().all(|l| grid.contains(l.as_str()))
            && FOCUSED_PANE_LINES.iter().all(|l| grid.contains(l))
            && !grid.contains("COMMAND MODE")
            // Idle and waiting cards blink their status dot by drawing a space
            // in its place (`flash_dot` in `src/ui.rs`), and nothing else on
            // this screen draws a `●` (the transcripts are written without
            // one), so exactly one per card means every dot is lit and the
            // image never shows a half-blink. A future `●` elsewhere on the
            // dashboard makes this time out, never pass early.
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
