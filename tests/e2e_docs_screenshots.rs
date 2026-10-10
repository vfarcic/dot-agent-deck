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

use std::os::unix::fs::PermissionsExt;
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
    launch_fixture_with("minimal", customize)
}

fn launch_fixture_with(
    fixture: &str,
    customize: impl FnOnce(TuiDeckBuilder) -> TuiDeckBuilder,
) -> TuiDeck {
    let deck = customize(
        TuiDeck::builder()
            .with_pty_size(COLS, ROWS)
            .without_success_recording()
            .without_agent_credentials(),
    )
    .launch_with_fixture(fixture);
    #[cfg(target_os = "linux")]
    {
        let pid = deck
            .child_pid()
            .expect("the PTY backend reports the deck's pid");
        // portable-pty's `pre_exec` closes every fd above 2, std's
        // close-on-exec status pipe included, so `spawn` can return while
        // the child is still the fork of this test process and its environ
        // is this process's own, ambient `ANTHROPIC_API_KEY` and all. Read it
        // only once the pid runs another binary and has an environment: an
        // environ read between the exec's mm switch and its argument setup
        // is empty, which would pass the check below without proving it.
        let test_exe = std::env::current_exe().expect("this test's executable");
        let read_environ = || std::fs::read(format!("/proc/{pid}/environ"));
        let execed = common::wait_until(Duration::from_secs(10), || {
            std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe != test_exe)
                && read_environ().is_ok_and(|env| !env.is_empty())
        });
        assert!(
            execed,
            "the deck process (pid {pid}) never exec'd its binary"
        );
        let environ = read_environ().unwrap_or_else(|e| panic!("read the deck's environment: {e}"));
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
    let captured = capture_unless(deck, scenario, ready, || false);
    assert!(
        captured,
        "a capture that never gives up returned without one"
    );
}

/// [`capture`], except that it stops waiting and returns `false`, writing
/// nothing, once `give_up` holds before `ready` does. `ready` is checked
/// first, so a frame that is ready is written even as `give_up` turns true.
/// Returns `true` once the frame is written; the harness's timeout still
/// panics with the final grid if neither ever holds.
fn capture_unless(
    deck: &TuiDeck,
    scenario: &str,
    ready: impl Fn(&str) -> bool,
    give_up: impl Fn() -> bool,
) -> bool {
    let page = deck.capture_screen_when(scenario, |screen| {
        if ready(&screen.contents()) {
            Some(Some(render_page(
                screen,
                &format!("{scenario} (TUI)"),
                &RenderOptions::default(),
            )))
        } else {
            give_up().then_some(None)
        }
    });
    let Some(mut page) = page else {
        return false;
    };
    if scenario == "new-agent" {
        // The form shows the harness's random temp path. Replace only its
        // visible Dir field, before the HTML becomes a published PNG, while
        // keeping the same cell width and the rest of the real frame intact.
        let start = page
            .find("Dir: /")
            .expect("New Agent form has an absolute Dir field");
        let end = start
            + page[start..]
                .find("</span>")
                .expect("Dir field ends in a rendered span");
        let width = page[start..end].chars().count();
        let display = "Dir: /home/dev/demo-project";
        assert!(
            display.len() <= width,
            "Dir field is too narrow for the docs path"
        );
        page.replace_range(start..end, &format!("{display:<width$}"));
    }
    let dir = html_dir();
    std::fs::create_dir_all(&dir).expect("create the TUI HTML dir");
    let path = dir.join(format!("{scenario}-tui.html"));
    std::fs::write(&path, page).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    true
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
const DOCS_PROJECT_DIR: &str = "demo-project";

/// The script every stand-in pane runs, the same text for every agent: it
/// takes the file to record its environment's variable names in as `$1` and
/// the transcript to print as `$2`, so no path is ever spliced into it. It
/// records the NAMES and never a value, so the capture can prove no agent
/// credential reached the panes: awk's `ENVIRON` is built from the process's
/// environment entries, each split on its first `=`, and the loop prints only
/// the keys, so a value — newlines included — is never written anywhere. Then
/// it prints the transcript and stays up so the pane stays open.
const STAND_IN_SCRIPT: &str = "awk 'BEGIN { for (name in ENVIRON) print name }' > \"$1\"\n\
                               cat \"$2\"\n\
                               exec sleep 600\n";

/// `s` as one POSIX shell word: wrapped in single quotes, with each `'` in it
/// spelled `'\''` (close the quote, an escaped `'`, reopen). Nothing is
/// special inside single quotes, so any path — spaces, `$`, backticks,
/// apostrophes — reaches the command as the one argument it is.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn shell_word(path: &Path) -> String {
    shell_quote(
        path.to_str()
            .unwrap_or_else(|| panic!("{} is not UTF-8", path.display())),
    )
}

/// Write the stand-in script and each agent's transcript under `dir`, and
/// return the command each agent's pane runs: [`STAND_IN_SCRIPT`] with that
/// agent's env-names file and transcript as its arguments, every path quoted
/// with [`shell_quote`]. The harness temp root is configurable
/// (`DAD_E2E_TMPDIR`), so the paths are not ours to assume shell-safe.
fn write_stand_ins(dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).expect("create the stand-in dir");
    let script = dir.join("stand-in.sh");
    std::fs::write(&script, STAND_IN_SCRIPT).expect("write the stand-in script");
    DASHBOARD_AGENTS
        .iter()
        .map(|agent| {
            let transcript = dir.join(format!("{}.txt", agent.session));
            std::fs::write(&transcript, agent.transcript).expect("write a transcript");
            format!(
                "sh {} {} {}",
                shell_word(&script),
                shell_word(&env_names_path(dir, agent)),
                shell_word(&transcript),
            )
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
    // The button bar also says "New Agent"; wait for the form's bordered
    // title so the keystrokes below cannot land on the picker.
    deck.wait_for_string("┌ New Agent");
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
    deck.wait_for_string("[New Agent Ctrl+N]");
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

/// How many times `docs_screenshot_dashboard` builds its scene before it gives
/// up. See [`DashboardScene::capture_at`] for what a missed attempt is.
const DASHBOARD_ATTEMPTS: usize = 3;

/// The `dashboard` scene, staged and waiting for its capture second.
struct DashboardScene {
    deck: TuiDeck,
    /// The whole second, as a Unix timestamp, during which every card's
    /// `Last:` label reads exactly its agent's `quiet_for_secs`. The frame can
    /// only be taken inside it, so a staging that ran past it, or a capture
    /// that never saw every status dot lit inside it, is a missed attempt.
    capture_at: i64,
}

/// Scenario: Launch the deck in a sandbox and open four panes through the
/// new-pane form, each running a stand-in that prints a fixed agent-like
/// transcript. Focus the working agent's pane and go back to the dashboard,
/// then give each agent its fixed type, prompt, tool and status through hook
/// events addressed to its pane, one at a time and each confirmed by the
/// daemon before the next. Once every card shows its name, tool and exact
/// `Last:` age with every status dot lit, and the focused pane shows its
/// transcript beside the card column, write the frame as `dashboard-tui.html`;
/// if that second passes first, build the whole scene again in a fresh
/// sandbox, up to three times.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_dashboard() {
    html_dir();
    let lasts: Vec<String> = DASHBOARD_AGENTS
        .iter()
        .map(|a| format!("Last: {}s ", a.quiet_for_secs))
        .collect();
    let ready = |grid: &str| {
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
    };
    // A missed capture second is retried by building the whole scene again,
    // never by re-stamping the cards: the only events that move a card's
    // `Last:` also add to what it shows (a second `tool_start` is a second
    // tool line, a second prompt a second prompt line), so a re-stamped scene
    // would not be the same image. The last attempt does not give up, so it
    // fails with the harness's timeout panic and the final grid.
    for attempt in 1..=DASHBOARD_ATTEMPTS {
        let scene = stage_dashboard();
        let last_attempt = attempt == DASHBOARD_ATTEMPTS;
        // The labels roll over at `capture_at + 1`. A frame drawn inside the
        // second can still be the latest one shortly after it ends, so give
        // it until `capture_at + 2` before calling the attempt missed.
        let missed = || !last_attempt && Utc::now().timestamp() >= scene.capture_at + 2;
        if capture_unless(&scene.deck, "dashboard", ready, missed) {
            return;
        }
        eprintln!(
            "docs_screenshot_dashboard: attempt {attempt} of {DASHBOARD_ATTEMPTS} \
             missed its capture second; building the scene again"
        );
    }
    unreachable!("the last attempt either captures or panics");
}

/// Stage the `dashboard` scene in a fresh sandbox: open the four panes, focus
/// [`FOCUSED_AGENT`]'s, and send each agent's stamped events.
fn stage_dashboard() -> DashboardScene {
    // The events come from this process, which is not the pane and so cannot
    // present the per-pane capability the daemon gives each pane's own agent.
    let deck = launch_with(|builder| {
        builder
            .with_launch_subdir(LAUNCH_DIR)
            .impersonating_pane_signals()
    });
    deck.wait_for_string("No active agents");
    let stand_ins = deck.workdir().join("docs-stand-ins");
    let commands = write_stand_ins(&stand_ins);

    let mut last_command = "";
    for (agent, command) in DASHBOARD_AGENTS.iter().zip(&commands) {
        open_pane(&deck, agent, command, last_command);
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
    deck.wait_for_string("[New Agent Ctrl+N]");

    // The cards' `Last:` ages. A card's last activity is a high-water mark
    // that starts at its pane's spawn, so every status event is stamped after
    // the last spawn: at `capture_at` minus the agent's age, on a whole second.
    // With every stamp on a whole second, the four labels roll over together,
    // once a second, and during the second that starts at `capture_at` they
    // read exactly `quiet_for_secs` each. The `ready` check names those
    // labels, so the frame is always taken in that second, and a capture that
    // missed it is retried or times out rather than writing a wrong image.
    // Until then the stamps are in the future and the cards read `Last: 0s`.
    // One clock sample feeds both the capture second and the uptimes.
    let max_quiet = DASHBOARD_AGENTS
        .iter()
        .map(|a| a.quiet_for_secs)
        .max()
        .unwrap_or(0);
    let now = Utc::now();
    let capture_at = now.timestamp() + 2 + max_quiet;
    for agent in DASHBOARD_AGENTS {
        let record = wait_for_record(&deck, agent.name, |_| true);
        let pane_id = record
            .pane_id_env
            .clone()
            .unwrap_or_else(|| panic!("{}'s pane has no pane id", agent.name));
        let started = (now - ChronoDuration::minutes(agent.up_for_minutes)).to_rfc3339();
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
    DashboardScene { deck, capture_at }
}

/// `shell_quote` keeps every path one argument, whatever it contains. Runs
/// the quoted words through `sh` rather than comparing strings, so it checks
/// what the pane's shell will actually see.
#[test]
fn shell_quote_keeps_hostile_paths_one_word() {
    let hostile = [
        "/plain/path",
        "/with space/and\ttab",
        "/it's/an apostrophe",
        "/'leading and trailing'",
        "/''",
        "/$(touch pwned)/`id`/$HOME/;rm -rf x",
        "/new\nline",
    ];
    let words: Vec<String> = hostile.iter().map(|s| shell_quote(s)).collect();
    let script = format!("printf '%s\\0' {}", words.join(" "));
    let out = std::process::Command::new("sh")
        .args(["-c", &script])
        .output()
        .expect("run sh");
    assert!(out.status.success(), "sh failed: {out:?}");
    let args: Vec<&[u8]> = out
        .stdout
        .strip_suffix(b"\0")
        .expect("NUL-terminated")
        .split(|b| *b == 0)
        .collect();
    let expected: Vec<&[u8]> = hostile.iter().map(|s| s.as_bytes()).collect();
    assert_eq!(args, expected);
}

/// Scenario: Launch the deck in a sandbox with no agents and write the
/// dashboard's empty state — the `No active agents` hint and the command-mode
/// button bar — as `dashboard-empty-tui.html`.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_dashboard_empty() {
    html_dir();
    let deck = launch();
    capture(&deck, "dashboard-empty", |grid| {
        grid.contains("No active agents") && grid.contains("[New Agent Ctrl+N]")
    });
}

/// Scenario: Open a stand-in implementation agent on a fixture GitHub branch
/// whose strict offline gh stub reports open PR #1234 awaiting review. Capture
/// the dashboard only when its card badge and fixed pane transcript are visible.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_pull_request_badge() {
    html_dir();
    let scratch = common::harness_tempdir().expect("PR screenshot scratch");
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, r#"#!/bin/sh
[ "$1" = pr ] && [ "$2" = list ] || exit 91
shift 2
head= state= repo= fields=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --head) shift; head="$1" ;;
        --state) shift; state="$1" ;;
        --repo) shift; repo="$1" ;;
        --json) shift; fields="$1" ;;
        *) exit 92 ;;
    esac
    shift
done
[ "$head" = feat/pr-badge ] && [ "$repo" = test-org/test-repo ] && [ "$state" = all ] || exit 93
for field in number state isDraft reviewDecision url headRefName; do
    case ",$fields," in
        *",$field,"*) ;;
        *) exit 94 ;;
    esac
done
printf '%s\n' '[{"number":1234,"headRefName":"feat/pr-badge","state":"OPEN","isDraft":false,"reviewDecision":"REVIEW_REQUIRED","url":"https://github.com/test-org/test-repo/pull/1234"}]'
"#).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let deck = launch_with(|builder| {
        builder
            .with_launch_subdir(LAUNCH_DIR)
            .impersonating_pane_signals()
            .with_env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .with_env("DOT_AGENT_DECK_PR_REFRESH_SECS", "1")
    });
    deck.wait_for_string("No active agents");
    let repo = deck.workdir().join(LAUNCH_DIR);
    for args in [
        vec!["init", "-b", "main"],
        vec!["commit", "--allow-empty", "-m", "fixture"],
        vec![
            "remote",
            "add",
            "origin",
            "https://github.com/test-org/test-repo.git",
        ],
        vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
        vec![
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
        vec!["checkout", "-b", "feat/pr-badge"],
    ] {
        let output = common::fixture_git(&repo, scratch.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stand_ins = deck.workdir().join("docs-stand-ins");
    let commands = write_stand_ins(&stand_ins);
    let agent = &DASHBOARD_AGENTS[1];
    open_pane(&deck, agent, &commands[1], "");
    let names = std::fs::read_to_string(env_names_path(&stand_ins, agent)).unwrap();
    for key in common::AGENT_CREDENTIAL_ENV {
        assert!(
            !names.lines().any(|name| name == key),
            "{key} reached the screenshot pane"
        );
    }
    let record = wait_for_record(&deck, agent.name, |_| true);
    let pane_id = record.pane_id_env.expect("screenshot pane id");
    send(
        &deck,
        serde_json::json!({
            "session_id": agent.session,
            "agent_type": agent.agent_type,
            "event_type": "session_start",
            "timestamp": (Utc::now() - ChronoDuration::minutes(agent.up_for_minutes)).to_rfc3339(),
            "cwd": repo,
            "pane_id": pane_id,
            "agent_id": record.id,
            "metadata": { "display_name": agent.name },
        }),
    );
    wait_for_record(&deck, agent.name, |r| {
        r.live.as_ref().is_some_and(|live| {
            live.agent_type.as_ref().map(|kind| serde_json::json!(kind))
                == Some(serde_json::Value::from(agent.agent_type))
        })
    });
    send(
        &deck,
        serde_json::json!({
            "session_id": agent.session,
            "agent_type": agent.agent_type,
            "event_type": "tool_start",
            "timestamp": (Utc::now() + ChronoDuration::seconds(30)).to_rfc3339(),
            "cwd": repo,
            "pane_id": pane_id,
            "agent_id": record.id,
            "user_prompt": agent.prompt,
            "tool_name": "Edit",
            "tool_detail": "src/components/RetryPayment.tsx",
        }),
    );
    capture(&deck, "pull-request-badge", |grid| {
        grid.contains(agent.name)
            && grid.contains(agent.prompt)
            && grid.contains("#1234 ⊙ ◐")
            && grid.contains("Last: 0s ")
            && grid.contains("Working")
            && !grid.contains("Ctrl+D to type")
            && FOCUSED_PANE_LINES.iter().all(|line| grid.contains(line))
    });
}

/// Scenario: Choose the sandbox project's directory through Ctrl+N and show
/// the New Agent form before starting any command.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_new_agent() {
    html_dir();
    let deck = launch_fixture_with("docs-screenshots", |builder| {
        builder.with_launch_subdir(DOCS_PROJECT_DIR)
    });
    deck.wait_for_string("No active agents");
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    capture(&deck, "new-agent", |grid| {
        grid.contains("┌ New Agent") && grid.contains(DOCS_PROJECT_DIR) && grid.contains("No mode")
    });
}

/// Scenario: Activate the project's demo-loop orchestration with two stand-in
/// roles, then address synthetic working events to their real pane IDs. Capture
/// its active tab only when both role cards show their work and fixed ages.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_orchestration() {
    html_dir();
    for attempt in 1..=DASHBOARD_ATTEMPTS {
        let scene = stage_orchestration();
        let ready = |grid: &str| {
            grid.contains("demo-loop [×]")
                && grid.contains("planner")
                && grid.contains("builder")
                && grid.contains("Planning the checkout retry flow")
                && grid.contains("Plan the checkout retry flow.")
                && grid.contains("Implement the checkout retry flow.")
                && grid.contains("src/checkout/flow.ts")
                && grid.contains("src/checkout/RetryPayment.tsx")
                && grid.contains("Last: 2s ")
                && grid.contains("Last: 3s ")
                && grid.matches("Working").count() == 2
                && grid.matches('●').count() == 2
                && !grid.contains("No agent")
                && !grid.contains("Launch an agent to get started")
                && !grid.contains("Activated orchestration")
        };
        let last_attempt = attempt == DASHBOARD_ATTEMPTS;
        let missed = || !last_attempt && Utc::now().timestamp() >= scene.capture_at + 2;
        if capture_unless(&scene.deck, "orchestration", ready, missed) {
            return;
        }
        eprintln!(
            "docs_screenshot_orchestration: attempt {attempt} of {DASHBOARD_ATTEMPTS} \
             missed its capture second; building the scene again"
        );
    }
    unreachable!("the last attempt either captures or panics");
}

/// A fresh orchestration whose role hooks are stamped for one capture second.
fn stage_orchestration() -> DashboardScene {
    let deck = launch_fixture_with("docs-screenshots", |builder| {
        builder
            .with_launch_subdir(DOCS_PROJECT_DIR)
            .impersonating_pane_signals()
    });
    deck.wait_for_string("No active agents");
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_string("Orch: demo-loop");
    deck.send_keys(b"\r");
    deck.send_keys(&vec![0x7f; "demo-project-orchestrator-1".len()]);
    deck.send_keys(b"demo-loop");
    deck.send_keys(b"\r");
    wait_for_record(&deck, "planner", |_| true);
    wait_for_record(&deck, "builder", |_| true);

    // A role card's last activity starts at its pane's spawn. Stamp both
    // status events after both panes exist, on whole seconds, so their Last:
    // labels roll over together and the ready check captures exactly one
    // second. Rebuild the scene if that second is missed.
    let now = Utc::now();
    // The activation banner has a 15-second TTL. Its lifetime starts before
    // this clock sample, so 18 seconds leaves room for its next redraw to
    // clear it before the capture second.
    let capture_at = now.timestamp() + 18;
    for (role, agent_type, session, prompt, tool, detail, quiet_for_secs) in [
        (
            "planner",
            "claude_code",
            "docs-orch-planner",
            "Plan the checkout retry flow.",
            "Read",
            "src/checkout/flow.ts",
            2,
        ),
        (
            "builder",
            "codex",
            "docs-orch-builder",
            "Implement the checkout retry flow.",
            "Edit",
            "src/checkout/RetryPayment.tsx",
            3,
        ),
    ] {
        let record = wait_for_record(&deck, role, |_| true);
        let pane_id = record
            .pane_id_env
            .clone()
            .unwrap_or_else(|| panic!("{role}'s pane has no pane id"));
        send(
            &deck,
            serde_json::json!({
                "session_id": session,
                "agent_type": agent_type,
                "event_type": "session_start",
                "timestamp": (now - ChronoDuration::minutes(30)).to_rfc3339(),
                "cwd": "/home/dev/demo-project",
                "pane_id": pane_id,
                "agent_id": record.id,
                "metadata": { "display_name": role },
            }),
        );
        let expected_type = serde_json::Value::from(agent_type);
        wait_for_record(&deck, role, |r| {
            r.live.as_ref().is_some_and(|live| {
                live.agent_type.as_ref().map(|t| serde_json::json!(t))
                    == Some(expected_type.clone())
            })
        });
        send(
            &deck,
            serde_json::json!({
                "session_id": session,
                "agent_type": agent_type,
                "event_type": "tool_start",
                "timestamp": chrono::DateTime::from_timestamp(capture_at - quiet_for_secs, 0)
                    .expect("a representable instant")
                    .to_rfc3339(),
                "cwd": "/home/dev/demo-project",
                "pane_id": pane_id,
                "agent_id": record.id,
                "user_prompt": prompt,
                "tool_name": tool,
                "tool_detail": detail,
            }),
        );
        wait_for_record(&deck, role, |r| {
            r.live
                .as_ref()
                .is_some_and(|live| live.last_user_prompt.as_deref() == Some(prompt))
        });
    }
    DashboardScene { deck, capture_at }
}

/// Scenario: Open the Schedules manager over an empty dashboard with one
/// disabled nightly-triage task. Its longer name leaves a visible gap between
/// the NAME and STATUS headers while keeping the next-fire field stable.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_schedules() {
    html_dir();
    let scratch = common::harness_tempdir().expect("schedules scratch directory");
    let schedules = scratch.path().join("schedules.toml");
    std::fs::write(
        &schedules,
        "[[scheduled_tasks]]\nname = \"nightly-triage\"\ncron = \"0 9 * * *\"\nworking_dir = \"/home/dev/storefront\"\ncommand = \"cat\"\nprompt = \"Summarize checkout changes.\"\nenabled = false\n",
    )
    .expect("write docs schedule");
    let deck = launch_with(|builder| {
        builder.with_env("DOT_AGENT_DECK_SCHEDULES", schedules.to_string_lossy())
    });
    deck.wait_for_string("No active agents");
    deck.send_keys(b"S");
    capture(&deck, "schedules", |grid| {
        grid.lines()
            .any(|line| line.contains("NAME ") && line.contains("STATUS"))
            && grid.contains("NEXT FIRE")
            && grid.contains("nightly-triage")
            && grid.contains("disabled")
    });
}

/// Scenario: Open the dashboard's question-mark help overlay so the keyboard
/// shortcuts and their plain-English actions are visible.
#[test]
#[ignore = "docs-screenshot generator: run it with `cargo docs-screenshots`"]
fn docs_screenshot_help() {
    html_dir();
    let deck = launch();
    deck.wait_for_string("No active agents");
    deck.send_keys(b"?");
    capture(&deck, "help", |grid| {
        grid.contains("Create new agent") && grid.contains("┌ Help")
    });
}
