#![cfg(all(feature = "e2e", unix))]

//! L2 regressions for spawn-time prompt confirmation. The synthetic scenario
//! deterministically swallows each pane's first PTY submission and confirms
//! only a later retry; the real scenario repeats the reported three-dispatch
//! Claude Code startup race with interactive Haiku agents.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Output};
use std::time::Duration;
use std::{collections::BTreeSet, collections::HashMap};

use common::TuiDeck;
use dot_agent_deck::event::{SESSION_START_ORIGIN_METADATA_KEY, WRAPPER_FORK_SESSION_START_ORIGIN};
use spec::spec;

const REAL_AGENT_COMMAND: &str = "claude --model claude-haiku-4-5-20251001 --allowedTools Bash";

struct SiblingWorktreeGuards(Vec<PathBuf>);

impl Drop for SiblingWorktreeGuards {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bindir = Path::new(bin).parent().expect("binary path has a parent");
    format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn commit_fixture_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    };
    run(&["config", "user.email", "deck-test@example.com"]);
    run(&["config", "user.name", "Deck Test"]);
    run(&["add", "-A"]);
    run(&["commit", "-qm", "fixture baseline"]);
}

fn dispatch_worktree_of(deck: &TuiDeck, name: &str) -> PathBuf {
    deck.workdir()
        .parent()
        .expect("fixture dir has a parent")
        .join(format!(
            "{}-dispatch-{name}",
            deck.workdir()
                .file_name()
                .expect("fixture dir has a name")
                .to_string_lossy()
        ))
}

fn open_cat_caller_pane(deck: &TuiDeck) -> String {
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(b"caller");
    deck.send_keys(b"\t");
    deck.send_keys(&[0x7f; 128]);
    deck.send_keys(b"cat");
    let (col, row) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render Submit");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");

    let find_caller = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find_map(|record| record.pane_id_env.filter(|_| record.cwd.is_some()))
    };
    assert!(
        common::wait_until(Duration::from_secs(60), || find_caller().is_some()),
        "no registered caller pane appeared; records={:?}\ngrid:\n{}",
        common::agent_records_on(deck.attach_socket_path()),
        deck.snapshot_grid()
    );
    find_caller().expect("caller checked above")
}

fn start_dispatch(deck: &TuiDeck, caller_pane: &str, name: &str, prompt: &str) -> Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["dispatch", name, "--task", prompt, "--single"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", caller_pane)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("start dispatch {name}: {error}"))
}

fn dispatch_concurrently(deck: &TuiDeck, caller_pane: &str, cases: &[(&str, &str)]) -> Vec<Output> {
    let children: Vec<Child> = cases
        .iter()
        .map(|(name, prompt)| start_dispatch(deck, caller_pane, name, prompt))
        .collect();
    children
        .into_iter()
        .map(|child| child.wait_with_output().expect("wait for dispatch CLI"))
        .collect()
}

fn assert_dispatch_commands_succeeded(cases: &[(&str, &str)], outputs: &[Output]) {
    for ((name, _), output) in cases.iter().zip(outputs) {
        assert!(
            output.status.success(),
            "dispatch {name} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn confirmed_prompt(deck: &TuiDeck, name: &str) -> Option<String> {
    let display_name = format!("dispatch-{name}");
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.display_name.as_deref() == Some(display_name.as_str()))
        .and_then(|record| record.live)
        .and_then(|live| live.last_user_prompt)
}

fn prompt_attempt_log(deck: &TuiDeck, name: &str) -> String {
    std::fs::read_to_string(dispatch_worktree_of(deck, name).join("prompt-attempts.log"))
        .unwrap_or_else(|_| "<no attempt log>".to_string())
}

fn first_submission_was_swallowed(deck: &TuiDeck, name: &str, prompt: &str) -> bool {
    prompt_attempt_log(deck, name)
        .lines()
        .any(|line| line == format!("swallowed|{prompt}"))
}

fn delivery_diagnostics(deck: &TuiDeck, cases: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (name, prompt) in cases {
        let attempts = prompt_attempt_log(deck, name);
        let first_submission_swallowed = attempts
            .lines()
            .any(|line| line == format!("swallowed|{prompt}"));
        out.push_str(&format!(
            "\n{name}: expected={prompt:?}, confirmed_exact={:?}, first_submission_swallowed={first_submission_swallowed}, attempt_log={attempts:?}",
            confirmed_prompt(deck, name)
        ));
    }
    out
}

/// Every delivery-lifecycle line the daemon logged, verbatim and in order.
///
/// Issue #664: `scheduler/dispatch/015`'s failure used to read only as
/// `confirmed_exact=None`, which is indistinguishable between "the retry path
/// is broken" (the regression the test exists to catch) and "the daemon
/// ABANDONED this delivery because nothing confirmed it inside the 60 s
/// production `AUTOMATIC_PROMPT_DEADLINE`" (a starved machine, or budget spent
/// somewhere it could not be recovered from).
/// Those need different responses and the panic could not tell them apart, so
/// the lines that name the difference — `abandoning`, `not re-submitting`, and
/// the per-attempt trail leading to them, each carrying its own `delivery_id`
/// and attempt count — are printed with the assertion instead of having to be
/// reconstructed afterwards.
fn delivery_log_evidence(log: &str) -> String {
    const MARKERS: [&str; 5] = [
        "prompt written to pane; provisional",
        "prompt delivery unconfirmed; re-submitting",
        "prompt delivery confirmed by the agent",
        "prompt delivery unconfirmed at the deadline; abandoning",
        "prompt delivery stopped without confirmation",
    ];
    let lines: Vec<&str> = log
        .lines()
        .filter(|line| MARKERS.iter().any(|marker| line.contains(marker)))
        .collect();
    if lines.is_empty() {
        "<no delivery lifecycle lines in the deck log>".to_string()
    } else {
        lines.join("\n")
    }
}

fn delivery_log_states(log: &str) -> HashMap<String, BTreeSet<&'static str>> {
    let mut states: HashMap<String, BTreeSet<&'static str>> = HashMap::new();
    for line in log.lines() {
        let state = if line.contains("prompt written to pane; provisional") {
            "written"
        } else if line.contains("prompt delivery unconfirmed; re-submitting") {
            "unconfirmed"
        } else if line.contains("prompt delivery confirmed by the agent") {
            "confirmed"
        } else {
            continue;
        };
        let Some(after_marker) = line.split_once("delivery_id=").map(|(_, after)| after) else {
            continue;
        };
        let delivery_id = if let Some(quoted) = after_marker.strip_prefix('"') {
            quoted.split_once('"').map(|(id, _)| id)
        } else {
            after_marker.split_whitespace().next()
        };
        if let Some(delivery_id) = delivery_id {
            states
                .entry(delivery_id.trim_end_matches(',').to_string())
                .or_default()
                .insert(state);
        }
    }
    states
}

/// How long the deck's readiness gate waits for a `SessionStart` before writing
/// the prompt anyway, pinned short so the fallback path is reached in seconds
/// rather than the production 30 s.
const READINESS_GATE_MS: u64 = 3_000;

/// How long the late-claim stand-in withholds its `SessionStart`, comfortably
/// past [`READINESS_GATE_MS`] so the claim is unambiguously post-write.
const LATE_CLAIM_SESSION_START_DELAY_SECS: u64 = 6;

/// The stand-in is named `claude` on purpose: the deck resolves
/// [`AgentType::from_command`] over the command IT chose to exec, so this is
/// the ordinary production shape (`default_command = "claude …"`) rather than
/// an anonymous script the deck can vouch for nothing about. Issue #570's fix
/// keys on exactly that spawn-time record, and `scheduler/dispatch/016` holds
/// the other side — a pane spawned with no known type still refuses a
/// post-write producer claim.
fn write_swallowing_agent(workdir: &Path) -> PathBuf {
    let path = workdir.join("claude");
    let bin = shell_quote(env!("CARGO_BIN_EXE_dot-agent-deck"));
    let body = format!(
        "#!/bin/sh\n\
         case \"$DOT_AGENT_DECK_PANE_ID\" in\n\
           *late-claim*) sleep {LATE_CLAIM_SESSION_START_DELAY_SECS} ;;\n\
         esac\n\
         printf '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"seed-%s\"}}' \"$DOT_AGENT_DECK_PANE_ID\" | {bin} hook --agent claude-code >/dev/null 2>&1 || exit 97\n\
         sleep 1\n\
         IFS= read -r swallowed || exit 0\n\
         printf 'swallowed|%s\\n' \"$swallowed\" >> prompt-attempts.log\n\
         while IFS= read -r submitted; do\n\
           printf 'confirmed|%s\\n' \"$submitted\" >> prompt-attempts.log\n\
           printf '{{\"hook_event_name\":\"UserPromptSubmit\",\"session_id\":\"seed-%s\",\"prompt\":\"%s\"}}' \"$DOT_AGENT_DECK_PANE_ID\" \"$submitted\" | {bin} hook --agent claude-code >/dev/null 2>&1 || exit 98\n\
         done\n"
    );
    std::fs::write(&path, body).expect("write swallowing stand-in");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod swallowing stand-in");
    path
}

fn write_default_command_config(command: &str) -> tempfile::TempDir {
    let dir = common::harness_tempdir().expect("config tempdir");
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    std::fs::write(
        dir.path().join("config.toml"),
        format!("default_command = \"{escaped}\"\n"),
    )
    .expect("write dispatch config");
    dir
}

/// Scenario: Launch an attached deck whose single-agent command posts SessionStart, delays its input reader, and deliberately swallows the first submitted line, then issue four dispatch --single calls concurrently. Three panes announce themselves before the prompt is written; the fourth withholds its SessionStart until after the readiness gate has expired and the prompt is already in the pane, so its producer claims a reporting agent only afterwards. Every pane must receive a backoff retry, emit a matching UserPromptSubmit hook for that retry, retain a durable confirmation, and produce written/unconfirmed/confirmed logs under its own distinct delivery id.
#[spec("scheduler/dispatch/014")]
#[test]
fn dispatch_014_concurrent_swallowed_seeds_retry_until_confirmed() {
    let staging = common::harness_tempdir().expect("stand-in staging dir");
    let stand_in = write_swallowing_agent(staging.path());
    let config = write_default_command_config(&stand_in.to_string_lossy());
    let log_name = "prompt-delivery.log";
    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_CONFIG",
            config.path().join("config.toml").to_string_lossy(),
        )
        .with_env("DOT_AGENT_DECK_LOG", log_name)
        // Issue #570: the reported failure is a `SessionStart` that missed the
        // readiness gate by 37 ms. Shortening the gate makes "the producer
        // identified itself only after the write" a deterministic input
        // instead of a race nobody could reproduce on demand.
        .with_env(
            "DOT_AGENT_DECK_SESSION_START_WAIT_MS",
            READINESS_GATE_MS.to_string(),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());
    let caller_pane = open_cat_caller_pane(&deck);

    // The first three announce themselves BEFORE the write — the control, and
    // the nearest thing to the fourth that should still work. `seed-late-claim`
    // is issue #570: same command, same swallow, same everything, except its
    // producer identifies itself only after the prompt is already in the pane.
    let cases = [
        ("seed-alpha", "Confirm synthetic seed alpha-7f31"),
        ("seed-beta", "Confirm synthetic seed beta-8c42"),
        ("seed-gamma", "Confirm synthetic seed gamma-9d53"),
        ("seed-late-claim", "Confirm synthetic seed late-claim-1a05"),
    ];
    let worktrees: Vec<PathBuf> = cases
        .iter()
        .map(|(name, _)| dispatch_worktree_of(&deck, name))
        .collect();
    let _guards = SiblingWorktreeGuards(worktrees.clone());
    let outputs = dispatch_concurrently(&deck, &caller_pane, &cases);
    assert_dispatch_commands_succeeded(&cases, &outputs);

    // The late-claim pane cannot confirm before its withheld `SessionStart`
    // plus a retry round trip, so the budget covers that rather than the
    // three-pane wait it replaced.
    let confirmed = common::wait_until(Duration::from_secs(45), || {
        cases
            .iter()
            .all(|(name, prompt)| confirmed_prompt(&deck, name).as_deref() == Some(*prompt))
    });
    let retried = cases.iter().all(|(name, prompt)| {
        let attempts =
            std::fs::read_to_string(dispatch_worktree_of(&deck, name).join("prompt-attempts.log"))
                .unwrap_or_default();
        attempts.contains(&format!("swallowed|{prompt}"))
            && attempts.contains(&format!("confirmed|{prompt}"))
    });
    let log = std::fs::read_to_string(deck.workdir().join(log_name)).unwrap_or_default();
    let states_by_delivery = delivery_log_states(&log);
    let required_states = BTreeSet::from(["written", "unconfirmed", "confirmed"]);
    let logged = states_by_delivery.len() == cases.len()
        && states_by_delivery
            .values()
            .all(|states| states == &required_states);

    assert!(
        confirmed && retried && logged,
        "all concurrently booting panes must retry a swallowed first PTY write until UserPromptSubmit confirms the seed, and each distinct delivery id must log written/unconfirmed/confirmed state. confirmed={confirmed}, retried={retried}, logged={logged}, states_by_delivery={states_by_delivery:?}{}\nlog tail:\n{}",
        delivery_diagnostics(&deck, &cases),
        log.lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn trust_paths_for_worktrees(deck: &TuiDeck, names: &[&str]) -> Vec<String> {
    let mut paths: Vec<String> = names
        .iter()
        .map(|name| {
            dispatch_worktree_of(deck, name)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if let Ok(parent) = deck
        .workdir()
        .parent()
        .expect("fixture dir has a parent")
        .canonicalize()
    {
        let stem = deck
            .workdir()
            .file_name()
            .expect("fixture dir has a name")
            .to_string_lossy();
        for name in names {
            let canonical_shape = parent
                .join(format!("{stem}-dispatch-{name}"))
                .to_string_lossy()
                .into_owned();
            if !paths.contains(&canonical_shape) {
                paths.push(canonical_shape);
            }
        }
    }
    paths
}

fn write_bootstrap_swallowing_real_claude(workdir: &Path) -> PathBuf {
    let wrapper = workdir.join("bootstrap-swallowing-real-claude.sh");
    let binary = shell_quote(env!("CARGO_BIN_EXE_dot-agent-deck"));
    let body = format!(
        "#!/bin/sh\n\
         printf '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"bootstrap-%s\",\"metadata\":{{\"{SESSION_START_ORIGIN_METADATA_KEY}\":\"{WRAPPER_FORK_SESSION_START_ORIGIN}\"}}}}' \"$DOT_AGENT_DECK_PANE_ID\" | {binary} hook --agent claude-code >/dev/null 2>&1 || exit 97\n\
         IFS= read -r swallowed || exit 98\n\
         printf 'swallowed|%s\\n' \"$swallowed\" >> prompt-attempts.log\n\
         exec {REAL_AGENT_COMMAND}\n"
    );
    std::fs::write(&wrapper, body).expect("write real-Claude bootstrap launcher");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("chmod real-Claude bootstrap launcher");
    wrapper
}

/// Scenario: Launch an attached deck with isolated Claude credentials and a bootstrap launcher that identifies its SessionStart as launcher-origin, consumes the first seed during boot, then execs real interactive Haiku Claude in each of three predicted dispatch worktrees. Every first write must be recorded as swallowed, each real pane must later submit its distinct sentinel-bearing retry through Claude's native UserPromptSubmit hook, and any failure must print each pane's exact confirmation and attempt evidence plus the daemon's own delivery-lifecycle log lines.
#[spec("scheduler/dispatch/015")]
#[test]
fn dispatch_015_three_real_claude_seeds_are_genuinely_confirmed() {
    skip_unless!(common::check_claude_available());

    let staging = common::harness_tempdir().expect("real-Claude bootstrap staging dir");
    let launcher = write_bootstrap_swallowing_real_claude(staging.path());
    let config = write_default_command_config(&launcher.to_string_lossy());
    let log_name = "prompt-delivery.log";
    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_CONFIG",
            config.path().join("config.toml").to_string_lossy(),
        )
        // Issue #664: without a log the failure cannot say WHY a pane never
        // confirmed. `dispatch/014` above has always captured this; /015 —
        // the one whose panes race a real 60 s deadline — did not, so its
        // abandonment was invisible. See [`delivery_log_evidence`].
        .with_env("DOT_AGENT_DECK_LOG", log_name)
        // Issue #664: this scenario can NEVER satisfy the readiness gate before
        // the write, so leaving it at the production 30 s spent half the
        // delivery budget on a wait with no possible outcome. The gate skips a
        // `wrapper_fork`-origin `SessionStart` and holds out for the agent's
        // NATIVE one (`state::wait_for_session_start`), but the bootstrap
        // launcher only `exec`s Claude after the write it is blocked reading —
        // so Claude cannot emit that native event until the gate has already
        // given up. Measured: the gate timed out at 30.1 s and the whole
        // delivery was abandoned 29.9 s later, the two halves of one 60 s
        // `AUTOMATIC_PROMPT_DEADLINE` captured before the wait. Pinning it here
        // — exactly as `dispatch/014` does, and to the same constant — returns
        // that half to the retry window the real agent actually gets, which is
        // what production spends it on when a native `SessionStart` releases
        // the gate in milliseconds. It changes no deadline and no assertion.
        .with_env(
            "DOT_AGENT_DECK_SESSION_START_WAIT_MS",
            READINESS_GATE_MS.to_string(),
        )
        .with_env("PATH", path_with_binary_dir())
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let cases = [
        (
            "real-seed-alpha",
            "Use Bash to verify seed-confirm-alpha-7f31.txt exists in the current directory then print its exact filename and wait",
        ),
        (
            "real-seed-beta",
            "Use Bash to verify seed-confirm-beta-8c42.txt exists in the current directory then print its exact filename and wait",
        ),
        (
            "real-seed-gamma",
            "Use Bash to verify seed-confirm-gamma-9d53.txt exists in the current directory then print its exact filename and wait",
        ),
    ];
    for (_, prompt) in &cases {
        let sentinel = prompt
            .split_whitespace()
            .find(|word| word.starts_with("seed-confirm-") && word.ends_with(".txt"))
            .expect("prompt carries a sentinel filename");
        std::fs::write(
            deck.workdir().join(sentinel),
            "dispatch seed confirmation\n",
        )
        .expect("write real-agent sentinel");
    }
    commit_fixture_repo(deck.workdir());

    let names: Vec<&str> = cases.iter().map(|(name, _)| *name).collect();
    let trust_paths = trust_paths_for_worktrees(&deck, &names);
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and project trust");
    let caller_pane = open_cat_caller_pane(&deck);
    let worktrees: Vec<PathBuf> = names
        .iter()
        .map(|name| dispatch_worktree_of(&deck, name))
        .collect();
    let _guards = SiblingWorktreeGuards(worktrees);

    let outputs = dispatch_concurrently(&deck, &caller_pane, &cases);
    let failed_commands: Vec<String> = cases
        .iter()
        .zip(&outputs)
        .filter(|(_, output)| !output.status.success())
        .map(|((name, _), output)| {
            format!(
                "{name}: status={} stdout={:?} stderr={:?}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .collect();
    assert!(
        failed_commands.is_empty(),
        "real dispatch commands failed: {failed_commands:#?}{}\nFinal grid:\n{}",
        delivery_diagnostics(&deck, &cases),
        deck.snapshot_grid()
    );

    let all_confirmed = common::wait_until(Duration::from_secs(150), || {
        cases
            .iter()
            .all(|(name, prompt)| confirmed_prompt(&deck, name).as_deref() == Some(*prompt))
    });
    let all_first_attempts_swallowed = cases
        .iter()
        .all(|(name, prompt)| first_submission_was_swallowed(&deck, name, prompt));
    let log = std::fs::read_to_string(deck.workdir().join(log_name)).unwrap_or_default();
    assert!(
        all_first_attempts_swallowed && all_confirmed,
        "every bootstrap launcher must swallow its first PTY submission and every real interactive Claude pane must genuinely submit a retried sentinel-bearing seed; a healthy Idle pane with no matching UserPromptSubmit is an undelivered seed. all_first_attempts_swallowed={all_first_attempts_swallowed}, all_confirmed={all_confirmed}{}\nDelivery log:\n{}\nFinal grid:\n{}",
        delivery_diagnostics(&deck, &cases),
        delivery_log_evidence(&log),
        deck.snapshot_grid()
    );
}
