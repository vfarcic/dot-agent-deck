#![cfg(all(feature = "e2e", unix))]

//! L2 PTY-attached real-agent rot canary for PRD #386's descendant-scan
//! shell-activity signal (M6a, catalog `status/shell-activity/005`).
//!
//! Every earlier test in this PRD (`status/shell-activity/001`-`004`) proves
//! the mechanism against a synthetic or stand-in process — a spawned
//! `sleep`, a hand-`setsid()`'d `python3` child. None of them prove Claude
//! Code itself still `setsid`-detaches its real Bash-tool child, which is
//! the one fact the whole signal rests on (PRD #386 Risks: "Claude Code
//! ceasing to `setsid` its Bash-tool child — total false negative, and
//! silent"). This test spawns a genuine interactive Haiku Claude agent
//! through the normal new-pane flow (the same path a user drives), lets it
//! make one real Bash tool call that runs long enough to observe, and
//! asserts the daemon's `run_shell_activity_monitor` actually synthesizes a
//! `ShellBusy` broadcast event for that pane while the call is in flight.
//!
//! Hand-seeds nothing: no synthetic `SessionStart`, no fabricated `pane_id`
//! — the pane id comes from the real spawn path (`AgentType::from_command`
//! infers `ClaudeCode` from the typed command, and the daemon assigns
//! `pane_id_env` itself), exactly as #370's own test failed to do. The
//! assertion is on the broadcast `AgentEvent` observed over a
//! `SubscribeEvents` connection, never on the rendered `Working` badge —
//! the badge is already `Working` from `ToolStart` regardless of whether
//! this signal fires at all, so a badge-only assertion would pass with the
//! mechanism completely dead, which is precisely how #370 shipped green.
//!
//! `sleep` cannot be the long-command instrument: Claude Code blocks long
//! `sleep` at the tool layer and emits no `ToolStart` at all (PRD #386,
//! measured). `ping -c 20 127.0.0.1 > /dev/null` is real, non-blocked,
//! foreground work that runs ~19-20s — long enough to observe the daemon's
//! 500ms poll catch the rising edge well before the command finishes.
//!
//! Cost note (Decision 23): one Haiku-4.5 interactive turn, one Bash tool
//! call. Local-only (Decision 8) and real-agent (rule 5 exception (a)):
//! gated on the `e2e` feature so CI's `cargo test-fast` never compiles it,
//! and this file's real-agent tier has no CI credentials, so a local run is
//! the only way to exercise it at all.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::EventType;
use dot_agent_deck::platform::proc::{
    MEASURED_SHELL_TOOL_SHAPES, descendant_shell_activity, descendants, process_table,
};
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const PANE_NAME_SUFFIX: &str = "shell-activity-005-haiku";
/// Unique so the prompt unambiguously names one real file rather than
/// leaving the model to invent a command — the sentinel + directive-prompt
/// pairing CLAUDE.md rule 4 asks for so the assertion survives LLM
/// phrasing/tool variance.
const SENTINEL: &str = "shell-activity-005-sentinel-e4b7f1.txt";
const SENTINEL_CONTENT: &str = "SHELL_ACTIVITY_005_OK";

/// Scenario: launch a real interactive Haiku Claude agent through the normal Ctrl+N new-pane flow (per-folder trust pre-seeded, `--allowedTools Bash`), then type a directive prompt naming a uniquely-named sentinel fixture file that instructs the agent to run `ping -c 20 127.0.0.1 > /dev/null` as its one Bash tool call. While that ~20s foreground command is in flight, assert — over a `SubscribeEvents` connection, never against the rendered badge — that the daemon's shell-activity monitor synthesizes a `ShellBusy` broadcast event for the pane.
#[spec("status/shell-activity/005")]
#[test]
fn shell_activity_005_real_claude_bash_child_trips_the_descendant_scan() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    std::fs::write(deck.workdir().join(SENTINEL), SENTINEL_CONTENT)
        .expect("write shell-activity-005 sentinel fixture");

    let cwd = deck.workdir().to_path_buf();
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and per-folder trust");

    let events = deck.subscribe_events();

    // The normal, user-driven new-pane flow — no synthetic hook, no
    // fabricated pane id. `AgentType::from_command` infers `ClaudeCode`
    // from the typed command text, which is what makes the daemon's
    // per-pane argv-shape selection (`shell_tool_shape_key`) pick
    // `CLAUDE_BASH_TOOL_SHAPE` for this pane later.
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(PANE_NAME_SUFFIX.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash").as_bytes());
    let (submit_col, submit_row) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render [Submit]");
    deck.click(submit_col, submit_row);

    assert!(
        deck.wait_for_grid_string_within("? for shortcuts", Duration::from_secs(120)),
        "the genuine interactive Claude prompt never became ready:\n{}",
        deck.snapshot_grid()
    );

    let record = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(PANE_NAME_SUFFIX))
        })
        .unwrap_or_else(|| {
            panic!("the real Claude pane ending in {PANE_NAME_SUFFIX:?} must be registered")
        });
    let agent_id = record.id;
    let pane_id = record
        .pane_id_env
        .expect("a real spawned pane must carry a pane_id_env — the daemon assigns it at spawn");

    let prompt = format!(
        "There is a file named {SENTINEL} in the current directory. Use the Bash tool exactly \
         once to run this single command: ping -c 20 127.0.0.1 > /dev/null; cat {SENTINEL}. Do \
         not run any other command. After the tool call finishes, reply with only the exact \
         file contents and nothing else."
    );
    deck.send_keys(prompt.as_bytes());
    deck.send_keys(b"\r");

    // Precondition: the real Bash tool call actually started. This is the
    // native ToolStart hook event, not a fabricated one.
    let tool_start = events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
        },
        Duration::from_secs(120),
    );
    eprintln!(
        "shell-activity-005: ToolStart observed at {:?} for pane {pane_id:?}",
        tool_start.timestamp
    );

    // The load-bearing assertion: the daemon's descendant-scan poll must
    // synthesize a ShellBusy event for THIS pane while the ~20s ping is
    // still in flight. `ping` began running the moment ToolStart fired (the
    // whole Bash-tool shell is what Claude Code setsid-detaches, not just a
    // backgrounded sub-command), so a 15s window comfortably sits inside the
    // command's ~19-20s lifetime — if the monitor's 500ms poll can see the
    // detached descendant at all, it has several polls' worth of margin to
    // report it within this window. A miss here means either Claude Code
    // stopped setsid-detaching its Bash-tool child (the total-false-negative
    // risk this canary exists to catch) or the descendant scan itself is not
    // reaching this pane — never an artifact of the window being too tight.
    let shell_busy = events.wait_for(
        |event| {
            event.event_type == EventType::ShellBusy
                && event.pane_id.as_deref() == Some(pane_id.as_str())
        },
        Duration::from_secs(15),
    );
    eprintln!(
        "shell-activity-005: ShellBusy observed at {:?}, {}ms after ToolStart — the descendant \
         scan found Claude Code's real Bash-tool child in a POSIX session of its own",
        shell_busy.timestamp,
        (shell_busy.timestamp - tool_start.timestamp).num_milliseconds()
    );

    // Soft, reel-style confirmation that the observed ping really was the
    // one the prompt asked for — logged, not gating, since matching the
    // model's final free-text reply is inherently more phrasing-sensitive
    // than the two typed hook/synthetic events above.
    let saw_sentinel_reply =
        deck.wait_for_grid_string_within(SENTINEL_CONTENT, Duration::from_secs(30));
    eprintln!(
        "shell-activity-005: sentinel content echoed back in the pane = {saw_sentinel_reply}"
    );
}

// ---------------------------------------------------------------------------
// M6b, catalog `status/shell-activity/006` — the reported bug, reproduced as
// the user actually sees it: a real Bash call crossing Claude Code's 120s
// default timeout must keep the pane's rendered badge on `Working`, not the
// stale `Idle` the bare `Stop -> EventType::Idle` mapping would otherwise
// paint the instant the agent's turn ends at the cap.
// ---------------------------------------------------------------------------

const PANE_NAME_SUFFIX_006: &str = "shell-activity-006-haiku";
/// Redirect target for the ping call, not a pre-created fixture — its whole
/// purpose is to appear, uniquely, inside the Bash-tool shell's own argv (the
/// `eval '<user command>'` segment PRD #386 documents), so a later
/// `process_table()` sample can find exactly this invocation among this
/// test's own descendants and nothing else's.
const SENTINEL_006: &str = "shell-activity-006-sentinel-9d21e6.log";
/// `-c 200` (200 pings, ~200s wall clock) comfortably outlives the sample
/// point below by ~70s of margin, matching the PRD's own 200s control run.
const PING_COUNT_006: u32 = 200;
/// Measured `ToolStart` -> `Idle` gap for an identical 200s ping under the
/// 120s default Bash timeout (PRD #386 Problem Statement: `ToolStart`
/// 01:20:53.307, `Idle` 01:23:00.372 = +127.065s). Used only to size how long
/// this test is willing to wait for the real `Idle` below — the assertion
/// itself no longer depends on this number being exactly right, since it now
/// waits for the genuine event rather than sleeping to a computed instant.
const MEASURED_TOOL_START_TO_IDLE_SECS: u64 = 127;
/// Margin added on top of [`MEASURED_TOOL_START_TO_IDLE_SECS`] for the
/// `Idle`-wait bound below. Generous enough to absorb normal jitter around
/// the 120s cap without turning a run that will never see `Idle` (Claude
/// Code ended the capped call with `ToolEnd` and no `Stop`) into an
/// unreasonably long wait.
const IDLE_WAIT_MARGIN_SECS: u64 = 30;

/// Scenario: launch a real interactive Haiku Claude agent through the normal Ctrl+N new-pane flow, then send a directive prompt instructing it to run `ping -c 200 127.0.0.1 > <sentinel> 2>&1` as its one Bash tool call under default Bash settings (no `timeout` parameter, no `run_in_background`), reproducing the >120s-cap bug exactly. After the native `ToolStart` hook fires, wait for the real, native `Idle` event (mapped from Claude Code's own `Stop` hook, never fabricated) so a PASS can only mean the bug path was genuinely exercised — a run where Claude ends the capped call with `ToolEnd` and no `Stop` fails loudly with a `PRECONDITION NOT MET` message rather than silently passing. Once the real `Idle` lands, switch to the dashboard and assert the pane's rendered card badge still reads `Working`, and that a live process carrying the sentinel (and a live `ping` beneath it) is still running.
#[spec("status/shell-activity/006")]
#[test]
fn shell_activity_006_real_claude_bash_call_crossing_the_cap_keeps_the_badge_working() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    // `wait_for_string`'s shared 10s bound is tight on a loaded machine (this
    // one routinely runs at load average 20+) and this startup race is
    // unrelated to anything this test asserts, so it gets a local, more
    // generous bound here rather than widening the shared constant every
    // other harness test relies on.
    assert!(
        deck.wait_for_grid_string_within("No active sessions", Duration::from_secs(30)),
        "startup race: the deck never rendered \"No active sessions\" within 30s — this is the \
         known loaded-machine startup flake (harness-side), not a badge or shell-activity \
         assertion:\n{}",
        deck.snapshot_grid()
    );

    let cwd = deck.workdir().to_path_buf();
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and per-folder trust");

    let events = deck.subscribe_events();

    // The normal, user-driven new-pane flow — no synthetic hook, no
    // fabricated pane id.
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(PANE_NAME_SUFFIX_006.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash").as_bytes());
    let (submit_col, submit_row) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render [Submit]");
    deck.click(submit_col, submit_row);

    assert!(
        deck.wait_for_grid_string_within("? for shortcuts", Duration::from_secs(120)),
        "the genuine interactive Claude prompt never became ready:\n{}",
        deck.snapshot_grid()
    );

    let record = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(PANE_NAME_SUFFIX_006))
        })
        .unwrap_or_else(|| {
            panic!("the real Claude pane ending in {PANE_NAME_SUFFIX_006:?} must be registered")
        });
    let agent_id = record.id;

    let prompt = format!(
        "Call the Bash tool now, as your very first action, with no preceding text or \
         explanation. Use the Bash tool exactly once, with default settings — do not pass a \
         timeout parameter and do not set run_in_background — to run this single command: ping \
         -c {PING_COUNT_006} 127.0.0.1 > {SENTINEL_006} 2>&1. Do not run any other command and do \
         not alter this command in any way."
    );
    deck.send_keys(prompt.as_bytes());
    deck.send_keys(b"\r");

    // Precondition: the real Bash tool call actually started, under default
    // settings — the native ToolStart hook event, not a fabricated one. A
    // timeout here is a harness flake (the model never issued the Bash tool
    // call at all — only a SessionStart shows up in `observed events`), not a
    // badge regression; the custom message says so explicitly so a red run
    // here is never misread as BADGE WRONG.
    let tool_start = match events.try_wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
        },
        Duration::from_secs(120),
    ) {
        Some(ev) => ev,
        None => panic!(
            "PRECONDITION NOT MET: no ToolStart/Bash event observed for agent {agent_id:?} \
             within 120s — the model never issued the Bash tool call at all. This is a harness \
             flake (prompt adherence), NOT a badge regression: rerun rather than reading this as \
             BADGE WRONG. Observed events: {:#?}",
            events.snapshot()
        ),
    };
    eprintln!(
        "shell-activity-006: ToolStart observed at {:?}",
        tool_start.timestamp
    );

    // The precondition this test exists to gate on. Roughly a third of runs
    // reach the moment that actually exercises Defect A: the rest end with
    // Claude Code's `ToolEnd` and no `Stop`, in which case the card never
    // leaves `Working` and the badge assertion below would hold whether or
    // not the fix works at all. Waiting for the real, native `Idle` (mapped
    // from the agent's own `Stop` hook, never fabricated by this test) turns
    // the old implicit timing assumption (ToolStart + a fixed offset) into an
    // explicit causal one: this run only counts as a PASS of the bug path if
    // the Idle that Defect A's fix has to correct actually landed.
    let idle_wait_timeout =
        Duration::from_secs(MEASURED_TOOL_START_TO_IDLE_SECS + IDLE_WAIT_MARGIN_SECS);
    let idle_event = events.try_wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.event_type == EventType::Idle
        },
        idle_wait_timeout,
    );
    let Some(idle_event) = idle_event else {
        panic!(
            "PRECONDITION NOT MET: no native Stop-mapped Idle event observed for pane \
             {PANE_NAME_SUFFIX_006:?} within {idle_wait_timeout:?} of ToolStart — this run never \
             exercises the bug path (Claude Code most likely ended the capped Bash call with \
             ToolEnd and no Stop, which is common — roughly 2 runs in 3 by the tester's own \
             measurement). This is an inconclusive run, NOT a badge regression: rerun rather than \
             reading this as BADGE WRONG. Observed events: {:#?}",
            events.snapshot()
        );
    };
    eprintln!(
        "shell-activity-006: native Idle observed at {:?}, {}ms after ToolStart — this run \
         genuinely exercises the bug path",
        idle_event.timestamp,
        (idle_event.timestamp - tool_start.timestamp).num_milliseconds()
    );

    // Back to the dashboard: what gets sampled next must be the rendered
    // card badge a user actually glances at, not the pane's own terminal
    // content (which is where 005 correctly avoids looking, for the
    // opposite reason — here the badge IS the point). Sampled right after
    // the real Idle above landed — the same instant a broken monitor would
    // have painted the badge Idle.
    deck.send_keys(b"\x04");
    deck.wait_for_string("Dir:");

    // The load-bearing assertion: the badge must still read Working, not the
    // stale Idle a bare `Stop -> EventType::Idle` mapping would have painted
    // the instant the real Idle above landed — this IS the reported bug,
    // reproduced or fixed, as the user sees it.
    assert!(
        deck.wait_for_grid_string_within("Working", Duration::from_secs(5)),
        "the pane's badge did not read Working right after the native Idle landed — the \
         reported bug (a >120s Bash call reads Idle while the command is still running) \
         reproduced:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        !deck.snapshot_grid().contains("Idle"),
        "the pane's badge read Idle alongside Working right after the native Idle landed — an \
         inconsistent render:\n{}",
        deck.snapshot_grid()
    );

    // Also assert the command is genuinely still running at this moment — a
    // Working badge next to a finished command would be a different bug
    // passing as a fix. Scoped to this test's own process tree (walked from
    // this test binary's own pid, which portable_pty made the real parent of
    // the spawned deck binary, its lazily-spawned daemon, and the daemon's
    // own spawned agent — `setsid` changes session/group, never `ppid`) so a
    // concurrently running e2e test's own ping or MCP servers can never be
    // mistaken for this one's.
    let table = process_table().expect("process_table() must enumerate on unix");
    let root_pid = std::process::id() as i32;
    let shell_row = descendants(&table, root_pid)
        .into_iter()
        .find(|p| p.argv.contains(SENTINEL_006))
        .unwrap_or_else(|| {
            panic!(
                "no live descendant of this test carries the sentinel {SENTINEL_006:?} in its \
                 argv at the sample point (right after the native Idle landed) — the Bash-tool \
                 call already finished, which would mean the {PING_COUNT_006}s ping finished \
                 early rather than genuinely testing the cap"
            )
        });
    let ping_alive = descendants(&table, shell_row.pid)
        .into_iter()
        .any(|p| p.argv.contains("ping") && p.argv.contains("127.0.0.1"));
    assert!(
        ping_alive,
        "the sentinel-tagged Bash-tool shell (pid {}) is alive but has no live `ping` \
         descendant at the sample point — the command must genuinely still be running, not \
         merely its wrapper shell",
        shell_row.pid
    );
    eprintln!(
        "shell-activity-006: command confirmed still running at the sample point (shell pid {}, \
         a live `ping` descendant found beneath it)",
        shell_row.pid
    );
}

// ---------------------------------------------------------------------------
// M6c, catalog `status/shell-activity/007` — no false positive against a
// LIVE process table: a real idle agent, with its real MCP servers (and
// whatever else Claude Code keeps alive) as children, must not pin the pane
// at `Working`. The load-bearing assumption of the whole design — every
// measured confounder stays in the agent's own POSIX session — was measured
// once, on one machine, with one MCP configuration; this is the only test
// that checks it against a live process table rather than a captured one.
// ---------------------------------------------------------------------------

const PANE_NAME_SUFFIX_007: &str = "shell-activity-007-haiku";

/// Scenario: launch a real interactive Haiku Claude agent through the normal Ctrl+N new-pane flow and leave it at its idle prompt — no prompt of ours is ever sent. Poll the real process table until the agent genuinely has live children (its MCP servers, `caffeinate`, whatever Claude Code keeps alive), wait a margin past the daemon's 500ms shell-activity poll, then assert the dashboard's rendered badge reads `Idle`, that the children are still alive at that moment, and that the descendant-scan classifier itself — run directly against that live table — agrees the pane is not busy.
#[spec("status/shell-activity/007")]
#[test]
fn shell_activity_007_real_claude_idle_with_live_mcp_servers_stays_idle() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let cwd = deck.workdir().to_path_buf();
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and per-folder trust");

    // The normal, user-driven new-pane flow — no synthetic hook, no
    // fabricated pane id, no hand-seeded SessionStart.
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(PANE_NAME_SUFFIX_007.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash").as_bytes());
    let (submit_col, submit_row) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render [Submit]");
    deck.click(submit_col, submit_row);

    assert!(
        deck.wait_for_grid_string_within("? for shortcuts", Duration::from_secs(120)),
        "the genuine interactive Claude prompt never became ready:\n{}",
        deck.snapshot_grid()
    );

    // Never send a prompt: the agent sits at its own idle prompt, exactly as
    // a user who opened a pane and stepped away would leave it.

    let root_pid = std::process::id() as i32;

    // Make sure the agent really has live children before sampling — an
    // agent with none proves nothing here (PRD #386 M6c). MCP servers take a
    // few seconds to come up after the prompt itself turns interactive, so
    // poll rather than assume they are already there.
    // Decision 21: bounded polling only, never a raw sleep, inside an
    // `e2e_*.rs` body — `common::wait_until` owns the actual `sleep`. Interior
    // mutability lets the `Fn` closure stash what it found on the iteration
    // that finally succeeds.
    let claude_pid_cell: std::cell::Cell<Option<i32>> = std::cell::Cell::new(None);
    let children_argv_cell: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
    let found_children = common::wait_until(Duration::from_secs(30), || {
        let Some(table) = process_table() else {
            return false;
        };
        let Some(claude_row) = descendants(&table, root_pid)
            .into_iter()
            .find(|p| p.argv.contains("claude") && p.argv.contains("--allowedTools"))
        else {
            return false;
        };
        let children: Vec<String> = descendants(&table, claude_row.pid)
            .into_iter()
            .map(|p| p.argv.clone())
            .collect();
        if children.is_empty() {
            return false;
        }
        claude_pid_cell.set(Some(claude_row.pid));
        *children_argv_cell.borrow_mut() = children;
        true
    });
    assert!(
        found_children,
        "the real Claude pane never grew any live children (MCP servers, caffeinate) within \
         30s — an agent with no children proves nothing about the no-false-positive claim"
    );
    let claude_pid = claude_pid_cell
        .get()
        .expect("wait_until reported success so claude_pid_cell must be set");
    let children_argv = children_argv_cell.into_inner();
    eprintln!(
        "shell-activity-007: real Claude agent pid {claude_pid} has {} live children before the \
         sample: {children_argv:?}",
        children_argv.len()
    );

    // Margin past the daemon's 500ms shell-activity poll, so several poll
    // cycles have had a chance to run against this live, children-bearing
    // table before the sample.
    common::wait_until(Duration::from_secs(3), || false);

    deck.send_keys(b"\x04");
    deck.wait_for_string("Dir:");

    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(5)),
        "the pane's badge did not read Idle with live MCP servers/caffeinate as children — a \
         false positive that would pin the pane at Working forever, which the PRD's Risks \
         section calls worse than the stale Idle this mechanism replaces:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        !deck.snapshot_grid().contains("Working"),
        "the pane's badge read Working alongside Idle — an inconsistent render:\n{}",
        deck.snapshot_grid()
    );

    // Independent oracle, at (approximately) the same moment: re-sample the
    // real process table, confirm the children are STILL alive (not just
    // before the badge check), and run the descendant-scan classifier
    // directly against this live table — the M2 fixture claim proven against
    // a live process table rather than a captured one.
    let table = process_table().expect("process_table() must enumerate on unix");
    let still_alive: Vec<&str> = descendants(&table, claude_pid)
        .into_iter()
        .map(|p| p.argv.as_str())
        .collect();
    assert!(
        !still_alive.is_empty(),
        "the real Claude agent's children died out between the liveness check and the sample — \
         rerun; this run proves nothing about the no-false-positive claim"
    );
    eprintln!(
        "shell-activity-007: {} children still alive at the sample point: {still_alive:?}",
        still_alive.len()
    );
    assert_eq!(
        descendant_shell_activity(&table, claude_pid, MEASURED_SHELL_TOOL_SHAPES),
        Some(false),
        "the descendant-scan classifier, run directly against the live process table, must \
         classify this real idle agent as NOT busy — every one of its live children must share \
         its own POSIX session"
    );
}
