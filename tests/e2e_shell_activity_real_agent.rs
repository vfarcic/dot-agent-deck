#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

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

/// The command lines of `pids`, read directly rather than through
/// [`process_table`] (issue #862).
///
/// The production sampler reads the argv only of a root's **session-boundary**
/// descendants, because a Bash-tool call's whole subtree inherits that call's
/// session and re-reading all of it every 500 ms is what made the poll stall
/// under load. The processes these tests need to *identify* — the `ping` running
/// under the tool shell, the agent's own MCP servers — sit below that boundary,
/// so their `command_line` comes back `NotSampled` by design.
///
/// This is a test diagnostic, so it pays the cost the poll refuses to: one `ps`
/// naming exactly the pids the assertion is about. Any failure yields an empty
/// map rather than a panic — the caller's assertion reports it with better
/// context than a panic here could.
fn command_lines_of(pids: &[i32]) -> std::collections::HashMap<i32, String> {
    if pids.is_empty() {
        return std::collections::HashMap::new();
    }
    let list = pids
        .iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = std::process::Command::new("ps")
        .args(["-w", "-w", "-o", "pid=,args=", "-p"])
        .arg(list)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return std::collections::HashMap::new();
    };
    let mut map = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let Some((pid, rest)) = line.split_once(char::is_whitespace) else {
            // A pid with no argv at all is still a real answer.
            if let Ok(pid) = line.parse::<i32>() {
                map.insert(pid, String::new());
            }
            continue;
        };
        if let Ok(pid) = pid.parse::<i32>() {
            map.insert(pid, rest.trim_start().to_string());
        }
    }
    map
}

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

fn background_live_fixture(suffix: &str) -> (TuiDeck, common::EventSub, String) {
    let deck = TuiDeck::builder()
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", Duration::from_secs(30)));
    let cwd = deck.workdir().to_path_buf();
    let mut paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        paths.push(canonical.to_string_lossy().into_owned());
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &paths).unwrap();
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(suffix.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash Agent").as_bytes());
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    assert!(
        deck.wait_for_grid_string_within("? for shortcuts", Duration::from_secs(120)),
        "interactive Claude not ready:\n{}",
        deck.snapshot_grid()
    );
    let id = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|r| {
            r.display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(suffix))
        })
        .unwrap()
        .id;
    (deck, events, id)
}

/// Scenario: Start real interactive Haiku Claude through Ctrl+N with trust/onboarding seeded, then ask for one Bash run_in_background ping writing a unique sentinel. After its native Stop the rendered card stays Idle with one background command; kill only that command found in this test's descendant tree and watch the indicator disappear.
#[spec("status/background/007")]
#[test]
fn background_007_real_background_command_clears_after_scoped_kill() {
    skip_unless!(common::check_claude_available());
    const SENTINEL_BG: &str = "background-007-ping-d960348b.txt";
    let (deck, events, agent_id) = background_live_fixture("background-007-haiku");
    let prompt = format!(
        "Use Bash exactly once as your first action, with run_in_background=true, to execute exactly: ping -c 180 127.0.0.1 > {SENTINEL_BG} 2>&1. Do not use timeout. Do not call TaskOutput, wait, inspect or stop the command. Immediately finish your turn with the single word STARTED after Bash returns the background task id."
    );
    let submitted = deck.submit_claude_prompt(&events, &agent_id, &prompt, SENTINEL_BG);
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(&agent_id)
                && event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
                && event.timestamp >= submitted.timestamp
        },
        Duration::from_secs(90),
    );
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(&agent_id)
                && event.event_type == EventType::Idle
                && event.timestamp >= submitted.timestamp
        },
        Duration::from_secs(90),
    );
    assert!(
        common::wait_for_path(&deck.workdir().join(SENTINEL_BG), Duration::from_secs(10)),
        "real command must write the sentinel"
    );
    deck.send_keys(b"\x04");
    deck.wait_for_string("Dir:");
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(20), |grid| grid.contains("Idle")
            && grid.contains("Background: 1 command")
            && !grid.contains("Working")),
        "ended turn and live command must both be visible:\n{}",
        deck.snapshot_grid()
    );

    let root = std::process::id() as i32;
    let table = process_table(&[root]).expect("Unix process sample");
    let own_pids: Vec<_> = descendants(&table, root)
        .into_iter()
        .map(|row| row.pid)
        .collect();
    let argv = command_lines_of(&own_pids);
    let shell = descendants(&table, root)
        .into_iter()
        .find(|row| {
            row.session_leader
                && !row.has_controlling_tty
                && argv
                    .get(&row.pid)
                    .is_some_and(|line| line.contains(SENTINEL_BG))
        })
        .expect("a detached command in this test's own process tree carries the unique sentinel");
    let ping = command_lines_of(
        &descendants(&table, shell.pid)
            .into_iter()
            .map(|row| row.pid)
            .collect::<Vec<_>>(),
    )
    .into_iter()
    .find(|(_, line)| {
        line.split_whitespace()
            .next()
            .is_some_and(|executable| executable.ends_with("ping"))
    })
    .expect("the background shell has a real live ping")
    .0;
    dot_agent_deck::platform::proc::force_kill_pid(ping as u32).expect("kill this test's own ping");
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| grid.contains("Idle")
            && !grid.contains("Background:")),
        "indicator did not clear after command ended:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Ask real interactive Haiku Claude to launch one Haiku subagent doing three slow Bash steps against a unique sentinel, then end the main turn immediately. The rendered card reads Idle with one background subagent (and may also count its commands), and loses the indicator only after that paired subagent's terminal hook and completion sentinel.
#[spec("status/background/008")]
#[test]
fn background_008_real_background_subagent_clears_on_paired_stop() {
    skip_unless!(common::check_claude_available());
    const SENTINEL_BG: &str = "background-008-subagent-72841dca.txt";
    let (deck, events, agent_id) = background_live_fixture("background-008-haiku");
    let prompt = format!(
        "Your first and only tool call must be Agent, launching exactly one general-purpose subagent with model=haiku and run_in_background=true. Give it this exact directive: 'Use Bash for exactly three sequential steps: (1) printf started > {SENTINEL_BG}; ping -c 20 127.0.0.1 > /dev/null (2) ping -c 20 127.0.0.1 > /dev/null (3) ping -c 20 127.0.0.1 > /dev/null; printf done > {SENTINEL_BG}.done. Use timeout=90000 and run_in_background=false for each Bash call. Do not delegate. Then finish.' After Agent returns, immediately finish your own turn with STARTED. Do not wait, inspect files, call TaskOutput or launch anything else."
    );
    let submitted = deck.submit_claude_prompt(&events, &agent_id, &prompt, SENTINEL_BG);
    let started = events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(&agent_id)
                && event.event_type == EventType::SubagentStart
                && event.timestamp >= submitted.timestamp
        },
        Duration::from_secs(90),
    );
    let subagent = started
        .metadata
        .get(dot_agent_deck::event::SUBAGENT_ID_METADATA_KEY)
        .expect("native SubagentStart carries its identity")
        .clone();
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(&agent_id)
                && event.event_type == EventType::Idle
                && !event.is_from_subagent()
                && event.timestamp >= submitted.timestamp
        },
        Duration::from_secs(90),
    );
    assert!(
        common::wait_for_path(&deck.workdir().join(SENTINEL_BG), Duration::from_secs(30)),
        "subagent must really do its sentinel work"
    );
    deck.send_keys(b"\x04");
    deck.wait_for_string("Dir:");
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(20), |grid| grid.contains("Idle")
            && grid
                .lines()
                .any(|line| line.contains("Background:") && line.contains("1 subagent"))
            && !grid.contains("Working")),
        "subagent part must survive unpaired internal stops:\n{}",
        deck.snapshot_grid()
    );
    // Ignore Claude's unpaired internal stops: only this start's id ends
    // the work being asserted, even when another stop arrives first.
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(&agent_id)
                && event.event_type == EventType::SubagentStop
                && event
                    .metadata
                    .get(dot_agent_deck::event::SUBAGENT_ID_METADATA_KEY)
                    == Some(&subagent)
        },
        Duration::from_secs(180),
    );
    assert!(
        common::wait_for_file_trimmed_eq(
            &deck.workdir().join(format!("{SENTINEL_BG}.done")),
            "done",
            Duration::from_secs(10)
        )
        .is_ok(),
        "subagent completion sentinel is required"
    );
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| grid.contains("Idle")
            && !grid.contains("Background:")),
        "paired SubagentStop must clear indicator:\n{}",
        deck.snapshot_grid()
    );
}

const PANE_NAME_SUFFIX: &str = "shell-activity-005-haiku";
/// Unique so the prompt unambiguously names one real file rather than
/// leaving the model to invent a command — the sentinel + directive-prompt
/// pairing CLAUDE.md rule 4 asks for so the assertion survives LLM
/// phrasing/tool variance.
const SENTINEL: &str = "shell-activity-005-sentinel-e4b7f1.txt";
const SENTINEL_CONTENT: &str = "SHELL_ACTIVITY_005_OK";

/// Scenario: launch a real interactive Haiku Claude agent through the normal Ctrl+N new-pane flow (per-folder trust pre-seeded, `--allowedTools Bash`), then type a directive prompt naming a uniquely-named sentinel fixture file that instructs the agent to run `ping -c 20 127.0.0.1 > /dev/null` as its one Bash tool call, and confirm Claude Code accepted it (its `UserPromptSubmit` hook) so a lost prompt fails as that rather than as a missing tool call. While that ~20s foreground command is in flight, assert — over a `SubscribeEvents` connection, never against the rendered badge — that the daemon's shell-activity monitor synthesizes a `ShellBusy` broadcast event for the pane.
#[spec("status/shell-activity/005")]
#[test]
fn shell_activity_005_real_claude_bash_child_trips_the_descendant_scan() {
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
        deck.wait_for_grid_string_within("No active agents", Duration::from_secs(30)),
        "startup race: the deck never rendered \"No active agents\" within 30s — this is the \
         known loaded-machine startup flake (harness-side), not a badge or shell-activity \
         assertion:\n{}",
        deck.snapshot_grid()
    );

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
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(PANE_NAME_SUFFIX.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash").as_bytes());
    let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
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
    let submitted = deck.submit_claude_prompt(&events, &agent_id, &prompt, SENTINEL);
    eprintln!(
        "shell-activity-005: prompt submitted (Thinking) at {:?}",
        submitted.timestamp
    );

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

/// Scenario: Launch real interactive Haiku Claude with seeded trust and allowedTools Bash, then ask for one default-settings ping lasting beyond the 120-second cap and writing a unique sentinel. Require its real Stop-mapped Idle hook (PRECONDITION NOT MET if absent), then assert the rendered badge reads Idle with Background: 1 command while the sentinel-tagged shell and its ping remain live.
#[spec("status/shell-activity/006")]
#[test]
fn shell_activity_006_real_claude_bash_call_crossing_the_cap_shows_idle_and_background() {
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
        deck.wait_for_grid_string_within("No active agents", Duration::from_secs(30)),
        "startup race: the deck never rendered \"No active agents\" within 30s — this is the \
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
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(PANE_NAME_SUFFIX_006.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash").as_bytes());
    let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
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
    let submitted = deck.submit_claude_prompt(&events, &agent_id, &prompt, SENTINEL_006);
    eprintln!(
        "shell-activity-006: prompt submitted (Thinking) at {:?}",
        submitted.timestamp
    );

    // Precondition: the real Bash tool call actually started, under default
    // settings — the native ToolStart hook event, not a fabricated one. A
    // timeout here is a harness flake (the model never issued the Bash tool
    // call at all), not a badge regression; the custom message says so
    // explicitly so a red run here is never misread as BADGE WRONG. The prompt
    // is known to have been submitted by this point (`submit_claude_prompt`
    // above), so a timeout here is the model, not a lost prompt — before issue
    // #701 the two were indistinguishable, with only a SessionStart in
    // `observed events` either way.
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
             within 120s of the prompt being submitted — the model never issued the Bash tool \
             call at all. This is a harness flake (prompt adherence), NOT a badge regression: \
             rerun rather than reading this as BADGE WRONG. Observed events: {:#?}",
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

    // The turn ended, while its command is still alive: both facts must be
    // presented together rather than conflated into a Working badge.
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid.contains("Idle")
            && grid.contains("Background: 1 command")),
        "after the real Stop the card must read Idle with Background: 1 command:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        !deck.snapshot_grid().contains("Working"),
        "the ended turn was incorrectly promoted to Working:\n{}",
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
    // Issue #862: the sample reads command lines only for detached descendants
    // of the roots it is given. This test's own pid is the right root: the
    // Bash-tool shell this assertion cares about `setsid`s off the agent's
    // terminal, so it is a detached descendant of it and gets its argv read,
    // while nothing outside this test's tree does. (The agent itself leads a
    // session on the pane's terminal, which since issue #1493 is not a
    // boundary, so its own argv is not read — this test does not need it.)
    let root_pid = std::process::id() as i32;
    let table = process_table(&[root_pid]).expect("process_table() must enumerate on unix");
    let shell_row = descendants(&table, root_pid)
        .into_iter()
        .find(|p| {
            p.command_line
                .read()
                .is_some_and(|argv| argv.contains(SENTINEL_006))
        })
        .unwrap_or_else(|| {
            panic!(
                "no live descendant of this test carries the sentinel {SENTINEL_006:?} in its \
                 argv at the sample point (right after the native Idle landed) — the Bash-tool \
                 call already finished, which would mean the {PING_COUNT_006}s ping finished \
                 early rather than genuinely testing the cap"
            )
        });
    // The `ping` lives BELOW the tool shell's session boundary, so the
    // production sampler deliberately never read its argv (issue #862) — read it
    // here, for exactly this shell's descendants.
    let ping_argv = command_lines_of(
        &descendants(&table, shell_row.pid)
            .into_iter()
            .map(|p| p.pid)
            .collect::<Vec<_>>(),
    );
    let ping_alive = ping_argv
        .values()
        .any(|argv| argv.contains("ping") && argv.contains("127.0.0.1"));
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
    // `wait_for_string`'s shared 10s bound is tight on a loaded machine (this
    // one routinely runs at load average 20+) and this startup race is
    // unrelated to anything this test asserts, so it gets a local, more
    // generous bound here rather than widening the shared constant every
    // other harness test relies on.
    assert!(
        deck.wait_for_grid_string_within("No active agents", Duration::from_secs(30)),
        "startup race: the deck never rendered \"No active agents\" within 30s — this is the \
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

    // The normal, user-driven new-pane flow — no synthetic hook, no
    // fabricated pane id, no hand-seeded SessionStart.
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(PANE_NAME_SUFFIX_007.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash").as_bytes());
    let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
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
    // Only a child seen in two samples at least `LASTING` apart counts. Claude
    // Code starts short-lived processes at boot (`ssh … git@github.com`,
    // `dpkg-query --search`, `git` fetches), and a precondition satisfied by one
    // of those died before the sample below, ~3 s and a dashboard switch later,
    // and failed the run about half the time.
    const LASTING: Duration = Duration::from_secs(5);
    let earlier_children: std::cell::RefCell<Option<(std::time::Instant, Vec<i32>)>> =
        std::cell::RefCell::new(None);
    let found_children = common::wait_until(Duration::from_secs(30), || {
        // Issue #862: this test's own pid as the sample's root — see the same
        // note in `status/shell-activity/006` above for why that is the root
        // that makes the deck's whole subtree's argv available here.
        let Some(table) = process_table(&[root_pid]) else {
            return false;
        };
        // Read every descendant's argv here rather than through the sampler,
        // which reads only session-boundary descendants of its root. The pane's
        // Claude leads a session on the pane's own terminal, and since issue
        // #1493 such a nested terminal is not a boundary, so the sampler no
        // longer reads its command line.
        let descendant_pids: Vec<i32> = descendants(&table, root_pid)
            .into_iter()
            .map(|p| p.pid)
            .collect();
        let Some(claude_row_pid) = command_lines_of(&descendant_pids)
            .into_iter()
            .find(|(_, argv)| argv.contains("claude") && argv.contains("--allowedTools"))
            .map(|(pid, _)| pid)
        else {
            return false;
        };
        // Same as in `006`: the agent's own children share its session, so they
        // are below its boundary and the sampler never read their argv.
        let child_pids: Vec<i32> = descendants(&table, claude_row_pid)
            .into_iter()
            .map(|p| p.pid)
            .collect();
        let now = std::time::Instant::now();
        let lasting: Vec<i32> = match earlier_children.borrow().as_ref() {
            Some((at, earlier)) if now.duration_since(*at) >= LASTING => child_pids
                .iter()
                .copied()
                .filter(|pid| earlier.contains(pid))
                .collect(),
            _ => Vec::new(),
        };
        if lasting.is_empty() {
            let mut earlier = earlier_children.borrow_mut();
            if earlier
                .as_ref()
                .is_none_or(|(at, _)| now.duration_since(*at) >= LASTING)
            {
                *earlier = Some((now, child_pids));
            }
            return false;
        }
        let children: Vec<String> = command_lines_of(&lasting).into_values().collect();
        if children.is_empty() {
            return false;
        }
        claude_pid_cell.set(Some(claude_row_pid));
        *children_argv_cell.borrow_mut() = children;
        true
    });
    // A precondition of the host, not an assertion about the deck: an agent
    // with no lasting children proves nothing about the no-false-positive
    // claim, and where Claude Code has no MCP server configured (and, on Linux,
    // no `caffeinate`) it often has none. `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1`
    // turns this skip into a failure.
    skip_unless!(if found_children {
        Ok(())
    } else {
        Err(
            "the real Claude pane grew no child that lasted 5 s within 30 s (no MCP server \
             configured, no caffeinate) — an agent with no children proves nothing about the \
             no-false-positive claim"
                .to_string(),
        )
    });
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
    // Issue #862: `claude_pid` is the root the classifier below is run against,
    // so it is the root this sample must be taken for — that is what makes the
    // command lines classification could consult present, rather than relying on
    // some other root's set happening to cover them. `root_pid` is named too so
    // the walk from it still resolves. The diagnostic listings do not depend on
    // either: they read the argv they need through `command_lines_of`.
    let table =
        process_table(&[root_pid, claude_pid]).expect("process_table() must enumerate on unix");
    let still_alive: Vec<String> = command_lines_of(
        &descendants(&table, claude_pid)
            .into_iter()
            .map(|p| p.pid)
            .collect::<Vec<_>>(),
    )
    .into_values()
    .collect();
    skip_unless!(if still_alive.is_empty() {
        Err(
            "the real Claude agent's children died out between the liveness check and the \
             sample — this run proves nothing about the no-false-positive claim"
                .to_string(),
        )
    } else {
        Ok(())
    });
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
