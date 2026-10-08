#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! L2 regressions for spawn-time prompt confirmation. The synthetic scenario
//! covers both a one-write swallow and a two-stage boot that destroys both
//! payload attempts; the real scenario repeats the reported three-dispatch
//! Claude Code startup race with interactive Haiku agents.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Output};
use std::time::Duration;
use std::{collections::BTreeSet, collections::HashMap};

use common::TuiDeck;
use dot_agent_deck::event::{SESSION_START_ORIGIN_METADATA_KEY, WRAPPER_FORK_SESSION_START_ORIGIN};
use dot_agent_deck::prompt_delivery::AUTOMATIC_PROMPT_DEADLINE;
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

/// The pane the `dispatch` CLI is run from, started with NO command — the
/// user's shell.
///
/// Issue #1602: a `--single` unit now starts with its dispatcher's own command,
/// so a `cat` caller (what this used to open) gave every unit `cat` instead of
/// the `default_command` these tests configure, and the stand-ins and the real
/// Claude never ran. A dispatcher that only opens a shell is the one a unit
/// does not copy, so it starts the deck's `default_command` as before. The
/// Command field is cleared first because the form seeds it from that
/// `default_command`.
fn open_shell_caller_pane(deck: &TuiDeck) -> String {
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(b"caller");
    deck.send_keys(b"\t");
    deck.send_keys(&[0x7f; 128]);
    let (col, row) = deck.wait_for_in_grid("[Submit]");
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

/// The opening words of the completion instruction PRD #220 Phase 2 (#1081)
/// appends to every `--single` dispatch prompt (`dispatch::dispatch_prompt`),
/// pinned here so [`confirms_dispatch_seed`] says "the caller's task, then the
/// daemon's OWN block" rather than "the caller's task, then anything".
const COMPLETION_INSTRUCTION_OPENING: &str = "When this work is finished";

/// Whether `reported` — a pane's last `UserPromptSubmit`, as the hook recorded
/// it — is the dispatch payload built around `seed`.
///
/// This used to be `== Some(seed)`, and that was right for exactly as long as a
/// `--single` dispatch's payload WAS the caller's task. #1081 appends the
/// `work-done` completion instruction to it, deliberately after the task
/// ("appended rather than prepended so the caller's own task stays the first
/// thing the agent reads"), and `dot_agent_deck::hook` records only the first
/// [`dot_agent_deck::prompt_delivery::USER_PROMPT_MAX_LEN`] bytes of what the
/// agent submitted, with a `…` where it cut. So exact equality against the seed
/// cannot hold for ANY dispatch any more, and the question it was really asking
/// is a prefix one: did this pane submit THIS caller's task, at the head of the
/// block the daemon composed around it?
///
/// Deliberately not `starts_with(seed)` alone: that would also accept a pane
/// that submitted the seed followed by anything at all, including the
/// accumulation shape `prompt_delivery::prompt_submission_accumulated` exists
/// to refuse. The daemon's own matcher stays the authority on whether a
/// delivery confirmed — the `logged` / `none_abandoned` assertions in both
/// tests read its verdict out of the delivery log — and this answers the
/// separate, user-altitude question the tests are named for.
fn confirms_dispatch_seed(reported: &str, seed: &str) -> bool {
    // Issue #1182's other half: a real Claude Code pane reports a multi-line
    // payload wrapped in its paste envelope rather than verbatim, so unwrap it
    // with the PRODUCT's own helper — one definition of that envelope's shape,
    // shared by the matcher this test exercises and by the test.
    let reported = dot_agent_deck::prompt_delivery::paste_envelope_payload(reported)
        .map_or(reported, |(payload, _)| payload);
    match reported.strip_prefix(seed) {
        // The payload is the task alone: nothing appended, which is what an
        // orchestration dispatch still submits.
        Some(rest) if rest.trim().is_empty() => true,
        Some(rest) => rest
            .trim_start()
            .starts_with(COMPLETION_INSTRUCTION_OPENING),
        None => false,
    }
}

/// Whether the pane dispatched as `name` has confirmed the seed `prompt`.
fn seed_is_confirmed(deck: &TuiDeck, name: &str, prompt: &str) -> bool {
    confirmed_prompt(deck, name).is_some_and(|reported| confirms_dispatch_seed(&reported, prompt))
}

fn prompt_attempt_log(deck: &TuiDeck, name: &str) -> String {
    std::fs::read_to_string(dispatch_worktree_of(deck, name).join("prompt-attempts.log"))
        .unwrap_or_else(|_| "<no attempt log>".to_string())
}

fn swallowed_submission_count(deck: &TuiDeck, name: &str, prompt: &str) -> usize {
    prompt_attempt_log(deck, name)
        .lines()
        .filter(|line| *line == format!("swallowed|{prompt}"))
        .count()
}

fn dispatch_pane_id(deck: &TuiDeck, name: &str) -> Option<String> {
    let display_name = format!("dispatch-{name}");
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.display_name.as_deref() == Some(display_name.as_str()))
        .and_then(|record| record.pane_id_env)
}

fn pane_delivery_log_lines<'a>(log: &'a str, pane_id: &str) -> Vec<&'a str> {
    log.lines()
        .filter(|line| line.contains(pane_id))
        .filter(|line| {
            line.contains("prompt written to pane; provisional")
                || line.contains("prompt delivery unconfirmed; re-submitting")
                || line.contains("prompt delivery confirmed by the agent")
                || line.contains("prompt delivery unconfirmed at the deadline; abandoning")
        })
        .collect()
}

fn payload_write_attempt(line: &str) -> Option<u32> {
    if !line.contains("prompt written to pane; provisional") {
        return None;
    }
    let (_, suffix) = line.split_once("attempt=")?;
    let end = suffix
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(suffix.len());
    suffix[..end].parse().ok()
}

fn delivery_diagnostics(deck: &TuiDeck, cases: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (name, prompt) in cases {
        let attempts = prompt_attempt_log(deck, name);
        let first_submission_swallowed = attempts
            .lines()
            .any(|line| line == format!("swallowed|{prompt}"));
        out.push_str(&format!(
            "\n{name}: expected={prompt:?}, confirmed_reported={:?}, first_submission_swallowed={first_submission_swallowed}, attempt_log={attempts:?}",
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

/// The POSIX-sh prelude every stand-in in this file opens with.
///
/// `read_submission` reads ONE WHOLE pane submission into `$submission` and
/// returns 1 at EOF — which is what an agent TUI does, and what reading one
/// LINE at a time only accidentally was. [`dot_agent_deck::pane_input`]'s
/// encoder wraps any MULTI-LINE payload in `ESC[200~`/`ESC[201~`, and the deck
/// submits it with a separate CR that the pane's line discipline delivers as
/// the final line's terminator — so a paste is one input however many lines it
/// spans. Every dispatch payload WAS single-line until PRD #220 Phase 2
/// (#1081) appended the `work-done` completion instruction to every `--single`
/// dispatch; from then on a line-at-a-time stand-in shredded one payload into
/// eight "submissions", none of them the text the daemon wrote and none of
/// them able to confirm it. That is issue #1182: both tests here spent their
/// whole retry budget and were abandoned at the deadline, on a product that
/// was delivering correctly.
///
/// `json_escape` renders `$1` as the body of a JSON string. A dispatch payload
/// legitimately carries `"`, `\` and newlines — the appended instruction has
/// all three — and an unescaped one is invalid JSON the hook would drop. The
/// trailing `\n` that the final `s/$/\\n/` leaves on the last line is exactly
/// what the daemon's `normalize_for_match` strips off a reported prompt before
/// comparing, so it cannot make a genuine submission fail to confirm.
const STAND_IN_SH_PRELUDE: &str = r#"
PASTE_OPEN=$(printf '\033[200~')
PASTE_CLOSE=$(printf '\033[201~')

read_submission() {
  submission=''
  pasting=0
  first=1
  while IFS= read -r chunk; do
    if [ "$first" -eq 1 ]; then
      first=0
      case "$chunk" in
        "$PASTE_OPEN"*)
          pasting=1
          chunk=${chunk#"$PASTE_OPEN"}
          ;;
      esac
      submission=$chunk
    else
      submission="$submission
$chunk"
    fi
    if [ "$pasting" -eq 0 ]; then
      return 0
    fi
    case "$submission" in
      *"$PASTE_CLOSE")
        submission=${submission%"$PASTE_CLOSE"}
        return 0
        ;;
    esac
  done
  return 1
}

json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/$/\\n/' | tr -d '\n'
}
"#;

/// The file, in each dispatch worktree, where a [`write_swallowing_agent`]
/// stand-in records how far it got with each `SessionStart` it posts:
/// `posting|<session prefix>` just before it shells out to
/// `dot-agent-deck hook`, then `hook-exited|<session prefix>|<status>` once that
/// subprocess returns.
///
/// Issue #531: the daemon's `Received event` line says whether an announcement
/// ARRIVED, and this trail says what the stand-in did, so a missing one can be
/// told apart as "never reached its announcement", "stuck in the hook
/// subprocess" or "the hook returned and nothing arrived". The last one needs
/// this file because the exit status alone cannot say it: `hook::handle_hook`
/// discards `send_to_socket`'s result and returns success either way.
const STAND_IN_READINESS_LOG: &str = "stand-in-readiness.log";

/// The POSIX-sh lines a [`write_swallowing_agent`] stand-in posts a
/// `SessionStart` with — session id `<session_prefix>-<pane id>`, plus
/// `metadata` when it is non-empty — bracketed by the
/// [`STAND_IN_READINESS_LOG`] trail, exiting `exit_code` if the hook fails.
/// `bin` is already shell-quoted.
fn session_start_sh(bin: &str, session_prefix: &str, metadata: &str, exit_code: u8) -> String {
    format!(
        "printf 'posting|{session_prefix}\\n' >> {STAND_IN_READINESS_LOG}\n\
         printf '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"{session_prefix}-%s\"{metadata}}}' \"$DOT_AGENT_DECK_PANE_ID\" | {bin} hook --agent claude-code >/dev/null 2>&1\n\
         hook_status=$?\n\
         printf 'hook-exited|{session_prefix}|%s\\n' \"$hook_status\" >> {STAND_IN_READINESS_LOG}\n\
         [ \"$hook_status\" -eq 0 ] || exit {exit_code}\n"
    )
}

/// The session-id prefix of the FIRST `SessionStart` the stand-in dispatched as
/// `name` posts — the one that depends on nothing but the stand-in having
/// started. For every pane but one that is its genuine `seed-` start. The
/// two-stage pane posts its genuine start only from stage two, which it
/// reaches only after swallowing two payload writes, so that start is
/// downstream of the delivery path under test; its launcher-origin
/// `launcher-` start is the announcement that owes nothing to delivery.
fn first_announcement_prefix(name: &str) -> &'static str {
    if name.contains("two-write-flush") {
        "launcher"
    } else {
        "seed"
    }
}

fn stand_in_readiness_trail(deck: &TuiDeck, name: &str) -> String {
    std::fs::read_to_string(dispatch_worktree_of(deck, name).join(STAND_IN_READINESS_LOG))
        .unwrap_or_else(|_| "<no readiness trail>".to_string())
}

/// The daemon's `Received event` lines for a `SessionStart`, verbatim.
fn session_start_log_lines(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|line| line.contains("Received event") && line.contains("event_type=SessionStart"))
        .collect()
}

/// The daemon's lines for a delivery it has STOPPED trying — the ones that
/// mean an announcement arriving afterwards can no longer arm a retry.
const TERMINAL_DELIVERY_MARKERS: [&str; 3] = [
    "delivery cannot be confirmed by this agent, not retrying",
    "prompt delivery stopped without confirmation",
    "prompt delivery unconfirmed at the deadline; abandoning",
];

/// Where one stand-in's first announcement stands, read from the daemon log in
/// order.
#[derive(Debug, PartialEq)]
enum Announcement {
    /// Logged before any terminal delivery line for the pane.
    InTime,
    /// Not logged, and the daemon is still trying to deliver.
    Pending,
    /// The daemon stopped trying before it logged one, so no retry was armed.
    TooLate,
}

fn announcement(log: &str, pane_id: &str, session_prefix: &str) -> Announcement {
    // The trailing space keeps pane `3` from matching `33`, and the quotes do
    // the same for the delivery lines' `pane_id="…"` field.
    let session_field = format!("session_id={session_prefix}-{pane_id} ");
    let pane_field = format!("pane_id=\"{pane_id}\"");
    for line in log.lines() {
        if line.contains("Received event")
            && line.contains("event_type=SessionStart")
            && line.contains(&session_field)
        {
            return Announcement::InTime;
        }
        if line.contains(&pane_field)
            && TERMINAL_DELIVERY_MARKERS
                .iter()
                .any(|marker| line.contains(marker))
        {
            return Announcement::TooLate;
        }
    }
    Announcement::Pending
}

/// One line of evidence per case whose first announcement is not
/// [`Announcement::InTime`], with that state; empty once every stand-in has
/// announced itself in time to arm a retry.
fn unannounced_stand_ins(
    deck: &TuiDeck,
    log: &str,
    cases: &[(&str, &str)],
) -> Vec<(Announcement, String)> {
    let records = common::agent_records_on(deck.attach_socket_path());
    cases
        .iter()
        .filter_map(|(name, _)| {
            let display_name = format!("dispatch-{name}");
            let pane_id = records
                .iter()
                .find(|record| record.display_name.as_deref() == Some(display_name.as_str()))
                .and_then(|record| record.pane_id_env.clone());
            let prefix = first_announcement_prefix(name);
            let state = pane_id
                .as_deref()
                .map_or(Announcement::Pending, |id| announcement(log, id, prefix));
            (state != Announcement::InTime).then(|| {
                let evidence = format!(
                    "{name}: {state:?}, expected session_id={prefix}-<pane id>, pane_id={pane_id:?}, stand-in trail={:?}",
                    stand_in_readiness_trail(deck, name)
                );
                (state, evidence)
            })
        })
        .collect()
}

/// The stand-in is named `claude` on purpose: the deck resolves
/// [`AgentType::from_command`] over the command IT chose to exec, so this is
/// the ordinary production shape (`default_command = "claude …"`) rather than
/// an anonymous script the deck can vouch for nothing about. Issue #570's fix
/// keys on exactly that spawn-time record, and `scheduler/dispatch/016` holds
/// the other side — a pane spawned with no known type still refuses a
/// post-write producer claim.
fn write_swallowing_agent(workdir: &Path) -> PathBuf {
    let path = workdir.join("claude");
    let stage_two = workdir.join("claude-stage-two");
    let bin = shell_quote(env!("CARGO_BIN_EXE_dot-agent-deck"));
    let genuine_start = session_start_sh(&bin, "seed", "", 97);
    let launcher_start = session_start_sh(
        &bin,
        "launcher",
        &format!(
            ",\"metadata\":{{\"{SESSION_START_ORIGIN_METADATA_KEY}\":\"{WRAPPER_FORK_SESSION_START_ORIGIN}\"}}"
        ),
        96,
    );
    let stage_two_body = format!(
        "#!/bin/sh\n{STAND_IN_SH_PRELUDE}\
         {genuine_start}\
         while read_submission; do\n\
           printf 'confirmed|%s\\n' \"$submission\" >> prompt-attempts.log\n\
           printf '{{\"hook_event_name\":\"UserPromptSubmit\",\"session_id\":\"seed-%s\",\"prompt\":\"%s\"}}' \"$DOT_AGENT_DECK_PANE_ID\" \"$(json_escape \"$submission\")\" | {bin} hook --agent claude-code >/dev/null 2>&1 || exit 98\n\
         done\n"
    );
    std::fs::write(&stage_two, stage_two_body).expect("write second-stage stand-in");
    let body = format!(
        "#!/bin/sh\n{STAND_IN_SH_PRELUDE}\
         case \"$DOT_AGENT_DECK_PANE_ID\" in\n\
           *two-write-flush*)\n\
             {launcher_start}\
             read_submission || exit 0\n\
             printf 'swallowed|%s\\n' \"$submission\" >> prompt-attempts.log\n\
             printf '{{\"hook_event_name\":\"PreToolUse\",\"session_id\":\"launcher-%s\",\"tool_name\":\"Bootstrap\"}}' \"$DOT_AGENT_DECK_PANE_ID\" | {bin} hook --agent claude-code >/dev/null 2>&1 || exit 99\n\
             read_submission || exit 0\n\
             printf 'swallowed|%s\\n' \"$submission\" >> prompt-attempts.log\n\
             exec {stage_two}\n\
             ;;\n\
         esac\n\
         case \"$DOT_AGENT_DECK_PANE_ID\" in\n\
           *late-claim*) sleep {LATE_CLAIM_SESSION_START_DELAY_SECS} ;;\n\
         esac\n\
         {genuine_start}\
         sleep 1\n\
         read_submission || exit 0\n\
         printf 'swallowed|%s\\n' \"$submission\" >> prompt-attempts.log\n\
         while read_submission; do\n\
           printf 'confirmed|%s\\n' \"$submission\" >> prompt-attempts.log\n\
           printf '{{\"hook_event_name\":\"UserPromptSubmit\",\"session_id\":\"seed-%s\",\"prompt\":\"%s\"}}' \"$DOT_AGENT_DECK_PANE_ID\" \"$(json_escape \"$submission\")\" | {bin} hook --agent claude-code >/dev/null 2>&1 || exit 98\n\
         done\n",
        stage_two = shell_quote(&stage_two.to_string_lossy()),
    );
    std::fs::write(&path, body).expect("write swallowing stand-in");
    use std::os::unix::fs::PermissionsExt;
    for executable in [&path, &stage_two] {
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
            .expect("chmod swallowing stand-in");
    }
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

/// Scenario: Launch five concurrent single-agent dispatches through hook-emitting stand-ins: four swallow one seed, while a two-stage launcher declares a wrapper handoff, destroys both payload writes, then starts a genuine Claude-shaped reader. The daemon must first log each stand-in's first announcement (the two-stage pane's launcher-origin start, everyone else's genuine one) before it stops trying to deliver to that pane, and a missing or too-late one fails as a named harness precondition rather than as a delivery verdict; then every pane must durably confirm the dispatch payload built around its own seed — the caller's task at its head, then the daemon's appended completion instruction; the two-stage pane must record two swallowed copies, receive the payload on attempt 3 exactly once, and never be abandoned.
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
        // Issue #1077: `start_dispatch` runs the real `dispatch` CLI from the
        // TEST process while naming the caller's pane, so it carries no
        // per-spawn hook capability token and the daemon refuses the message.
        // That refusal is SILENT to the caller — `Dispatch` is one of the two
        // fire-and-forget verbs `provenance_refusal_reply` deliberately answers
        // nothing for — so the CLI still exits 0 and the units simply never
        // spawn. The gate is working; the test process is not that pane. #1077
        // added this opt-in for exactly this harness pattern and applied it to
        // the files it knew about, and missed this one because nothing runs
        // lane 2 (CLAUDE.md rule 5).
        .impersonating_pane_signals()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    common::commit_fixture_repo(deck.workdir());
    let caller_pane = open_shell_caller_pane(&deck);

    // The first three announce themselves BEFORE the write — the control, and
    // the nearest thing to the fourth that should still work. `seed-late-claim`
    // is issue #570: same command, same swallow, same everything, except its
    // producer identifies itself only after the prompt is already in the pane.
    // The fifth is issue #666's measured alpha shape: the launcher-origin start
    // supplies standing, consumes both attempts while emitting only
    // non-generational capability evidence between them, then stage two posts
    // the genuine start; only an armed third payload can recover once its
    // reader becomes usable.
    let cases = [
        ("seed-alpha", "Confirm synthetic seed alpha-7f31"),
        ("seed-beta", "Confirm synthetic seed beta-8c42"),
        ("seed-gamma", "Confirm synthetic seed gamma-9d53"),
        ("seed-late-claim", "Confirm synthetic seed late-claim-1a05"),
        (
            "seed-two-write-flush",
            "Confirm synthetic seed two-write-flush-2b16",
        ),
    ];
    let worktrees: Vec<PathBuf> = cases
        .iter()
        .map(|(name, _)| dispatch_worktree_of(&deck, name))
        .collect();
    let _guards = SiblingWorktreeGuards(worktrees.clone());
    let outputs = dispatch_concurrently(&deck, &caller_pane, &cases);
    assert_dispatch_commands_succeeded(&cases, &outputs);

    // Issue #531: each retry this test is about is armed by a stand-in's
    // announcement. A pane whose announcement never reaches the daemon gets
    // one write and no retry — the daemon logs it as "delivery cannot be
    // confirmed by this agent, not retrying" — and the confirmation assertion
    // below then reported it as `confirmed=false` after its whole wait, which
    // blames the delivery path for a stand-in that never started. So establish
    // that precondition first, under its own name, for the one announcement
    // per pane that owes nothing to delivery (`first_announcement_prefix`),
    // and only count it when the daemon logged it BEFORE it stopped trying to
    // deliver to that pane: one that arrives afterwards can no longer arm
    // anything. A pane the daemon gives up on without one ends the wait at
    // once rather than at the bound. The bound is a backstop past the daemon's
    // own delivery deadline, far more than a passing run needs: the slowest
    // first announcer, `seed-late-claim`, sleeps
    // LATE_CLAIM_SESSION_START_DELAY_SECS first, and in a measured run the
    // last genuine start was logged 6.4 s after the first.
    let log_path = deck.workdir().join(log_name);
    let read_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let readiness_bound = AUTOMATIC_PROMPT_DEADLINE + Duration::from_secs(15);
    common::wait_until(readiness_bound, || {
        let gaps = unannounced_stand_ins(&deck, &read_log(), &cases);
        gaps.is_empty()
            || gaps
                .iter()
                .any(|(state, _)| *state == Announcement::TooLate)
    });
    let log = read_log();
    let unannounced = unannounced_stand_ins(&deck, &log, &cases);
    if !unannounced.is_empty() {
        panic!(
            "PRECONDITION, not a delivery verdict: these stand-ins did not announce themselves \
             in time to arm a retry, so the delivery assertions would fail them for a reason \
             that is not the delivery path's. `TooLate` is a pane the daemon stopped trying to \
             deliver to before it logged the announcement; `Pending` is one with neither by \
             {readiness_bound:?}. In the trail, ending at `posting` is a stand-in stuck in its \
             `dot-agent-deck hook` subprocess; `hook-exited|…|0` with no daemon line is a hook \
             that returned without its event arriving; no trail is a stand-in that never \
             reached its announcement.\n{}\nSessionStart lines the daemon did log:\n{}\n\
             delivery lifecycle:\n{}",
            unannounced
                .iter()
                .map(|(_, evidence)| evidence.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            session_start_log_lines(&log).join("\n"),
            delivery_log_evidence(&log)
        );
    }

    // The two-write pane is expected to run to the production deadline on
    // pre-fix code. Wait beyond it so RED diagnostics include the terminal
    // abandonment rather than only an in-flight missing confirmation.
    let confirmed = common::wait_until(Duration::from_secs(75), || {
        cases
            .iter()
            .all(|(name, prompt)| seed_is_confirmed(&deck, name, prompt))
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

    let two_stage_name = "seed-two-write-flush";
    let two_stage_prompt = "Confirm synthetic seed two-write-flush-2b16";
    let two_stage_attempts = prompt_attempt_log(&deck, two_stage_name);
    let two_stage_pane_id = dispatch_pane_id(&deck, two_stage_name);
    let two_stage_log_lines = two_stage_pane_id
        .as_deref()
        .map(|pane_id| pane_delivery_log_lines(&log, pane_id))
        .unwrap_or_default();
    let two_stage_written_on_attempt_three = two_stage_log_lines.iter().any(|line| {
        line.contains("prompt written to pane; provisional") && line.contains("attempt=3")
    });
    let two_stage_abandoned = two_stage_log_lines
        .iter()
        .any(|line| line.contains("prompt delivery unconfirmed at the deadline; abandoning"));
    let two_stage_recovered = swallowed_submission_count(&deck, two_stage_name, two_stage_prompt)
        == 2
        && two_stage_attempts
            .lines()
            .filter(|line| *line == format!("confirmed|{two_stage_prompt}"))
            .count()
            == 1
        && seed_is_confirmed(&deck, two_stage_name, two_stage_prompt)
        && two_stage_written_on_attempt_three
        && !two_stage_abandoned;

    assert!(
        confirmed && retried && logged && two_stage_recovered,
        "all concurrently booting panes must recover swallowed PTY payloads until UserPromptSubmit confirms the seed; the two-stage launcher must swallow exactly two payloads, receive one payload on attempt 3, and avoid abandonment. confirmed={confirmed}, retried={retried}, logged={logged}, two_stage_recovered={two_stage_recovered}, two_stage_written_on_attempt_three={two_stage_written_on_attempt_three}, two_stage_abandoned={two_stage_abandoned}, two_stage_pane_id={two_stage_pane_id:?}, two_stage_attempts={two_stage_attempts:?}, two_stage_log_lines={two_stage_log_lines:?}, states_by_delivery={states_by_delivery:?}{}\nlog tail:\n{}",
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
        "#!/bin/sh\n{STAND_IN_SH_PRELUDE}\
         printf '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"bootstrap-%s\",\"metadata\":{{\"{SESSION_START_ORIGIN_METADATA_KEY}\":\"{WRAPPER_FORK_SESSION_START_ORIGIN}\"}}}}' \"$DOT_AGENT_DECK_PANE_ID\" | {binary} hook --agent claude-code >/dev/null 2>&1 || exit 97\n\
         read_submission || exit 98\n\
         printf 'swallowed|%s\\n' \"$submission\" >> prompt-attempts.log\n\
         printf '{{\"hook_event_name\":\"PreToolUse\",\"session_id\":\"bootstrap-%s\",\"tool_name\":\"Bootstrap\"}}' \"$DOT_AGENT_DECK_PANE_ID\" | {binary} hook --agent claude-code >/dev/null 2>&1 || exit 99\n\
         read_submission || exit 100\n\
         printf 'swallowed|%s\\n' \"$submission\" >> prompt-attempts.log\n\
         exec {REAL_AGENT_COMMAND}\n"
    );
    std::fs::write(&wrapper, body).expect("write real-Claude bootstrap launcher");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("chmod real-Claude bootstrap launcher");
    wrapper
}

/// Scenario: Launch three real interactive Haiku dispatches through bootstrap launchers that declare a wrapper handoff, consume both payload attempts, then exec Claude. After each native Claude start, a later attempt must recover the sentinel-bearing seed and have Claude confirm it through UserPromptSubmit — which reports a pasted payload inside its `<pasted_content id="…">` envelope rather than verbatim — without deadline abandonment; failures print per-pane attempt and delivery evidence.
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
        // Issue #1077, same reason as `dispatch/014` above: the `dispatch` CLI
        // is invoked from the test process naming the caller's pane, so it
        // holds no hook capability token and the daemon refuses it silently.
        .impersonating_pane_signals()
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

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
    common::commit_fixture_repo(deck.workdir());

    let names: Vec<&str> = cases.iter().map(|(name, _)| *name).collect();
    let trust_paths = trust_paths_for_worktrees(&deck, &names);
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and project trust");
    let caller_pane = open_shell_caller_pane(&deck);
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
            .all(|(name, prompt)| seed_is_confirmed(&deck, name, prompt))
    });
    let all_two_payload_attempts_swallowed = cases
        .iter()
        .all(|(name, prompt)| swallowed_submission_count(&deck, name, prompt) == 2);
    let log = std::fs::read_to_string(deck.workdir().join(log_name)).unwrap_or_default();
    let pane_log_evidence: Vec<(&str, Option<String>, Vec<&str>)> = cases
        .iter()
        .map(|(name, _)| {
            let pane_id = dispatch_pane_id(&deck, name);
            let lines = pane_id
                .as_deref()
                .map(|pane_id| pane_delivery_log_lines(&log, pane_id))
                .unwrap_or_default();
            (*name, pane_id, lines)
        })
        .collect();
    let all_post_boot_payloads_written = pane_log_evidence.iter().all(|(_, _, lines)| {
        lines
            .iter()
            .filter_map(|line| payload_write_attempt(line))
            .any(|attempt| attempt > 2)
    });
    let none_abandoned = pane_log_evidence.iter().all(|(_, _, lines)| {
        !lines
            .iter()
            .any(|line| line.contains("prompt delivery unconfirmed at the deadline; abandoning"))
    });
    assert!(
        all_two_payload_attempts_swallowed
            && all_confirmed
            && all_post_boot_payloads_written
            && none_abandoned,
        "every bootstrap launcher must swallow both payload attempts, then every real interactive Claude pane must receive the seed payload on an attempt after attempt 2 and genuinely submit it without deadline abandonment; a healthy Idle pane with no matching UserPromptSubmit is an undelivered seed. all_two_payload_attempts_swallowed={all_two_payload_attempts_swallowed}, all_confirmed={all_confirmed}, all_post_boot_payloads_written={all_post_boot_payloads_written}, none_abandoned={none_abandoned}, pane_log_evidence={pane_log_evidence:?}{}\nDelivery log:\n{}\nFinal grid:\n{}",
        delivery_diagnostics(&deck, &cases),
        delivery_log_evidence(&log),
        deck.snapshot_grid()
    );
}

/// Issue #1616: the dispatched unit's pane, rendered from the daemon's own
/// scrollback the way the agent last painted it.
fn rendered_pane(deck: &TuiDeck, agent_id: &str) -> String {
    let mut parser = vt100::Parser::new(60, 200, 0);
    parser.process(&common::pane_snapshot_on(
        deck.attach_socket_path(),
        agent_id,
    ));
    parser.screen().contents()
}

/// Issue #1616: the daemon-side record of the unit dispatched as `name`.
fn dispatched_record(deck: &TuiDeck, name: &str) -> Option<dot_agent_deck::agent_pty::AgentRecord> {
    let display_name = format!("dispatch-{name}");
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.display_name.as_deref() == Some(display_name.as_str()))
}

/// Scenario: Dispatch one real interactive Haiku agent with a task file written with Windows line endings (CRLF, a blank line, a trailing line break) that asks it to print a sentinel file's contents; the deck must submit that task exactly once — confirmed by Claude on the first write, never typed into the pane again — and the agent must print the contents. Then send the same pane a follow-up whose two lines are separated by a bare carriage return, through the submit path the desktop app and the TUI use; it must start a turn of its own and the agent must print the second sentinel's contents, rather than sitting in the composer.
#[spec("scheduler/dispatch/028")]
#[test]
fn dispatch_028_prompts_with_cr_line_breaks_submit_exactly_once_on_real_claude() {
    skip_unless!(common::check_claude_available());

    // `Write` as well as `Bash`: a `--single` unit is told to write its report
    // with its file-writing tool, and a permission prompt would hold the first
    // turn open, leaving the follow-up no idle composer to land in.
    let config = write_default_command_config(&format!("{REAL_AGENT_COMMAND} Write"));
    let log_name = "prompt-delivery.log";
    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_CONFIG",
            config.path().join("config.toml").to_string_lossy(),
        )
        .with_env("DOT_AGENT_DECK_LOG", log_name)
        .with_env("PATH", path_with_binary_dir())
        // The `dispatch` CLI runs from the test process naming the caller's
        // pane, as in `dispatch/014` and `/015` (issue #1077).
        .impersonating_pane_signals()
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    // The contents are what the agent has to print, and appear in neither
    // prompt, so seeing them proves the agent read the file rather than
    // echoed its task.
    let first_sentinel = "line-break-sentinel-a-5d21.txt";
    let first_contents = "crlf-task-contents-91f3";
    let second_sentinel = "line-break-sentinel-b-6e32.txt";
    let second_contents = "bare-cr-followup-contents-4c7a";
    std::fs::write(
        deck.workdir().join(first_sentinel),
        format!("{first_contents}\n"),
    )
    .expect("write first sentinel");
    std::fs::write(
        deck.workdir().join(second_sentinel),
        format!("{second_contents}\n"),
    )
    .expect("write second sentinel");
    common::commit_fixture_repo(deck.workdir());

    let name = "line-breaks";
    let trust_paths = trust_paths_for_worktrees(&deck, &[name]);
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and project trust");
    let caller_pane = open_shell_caller_pane(&deck);
    let _guards = SiblingWorktreeGuards(vec![dispatch_worktree_of(&deck, name)]);

    // A task file saved by an editor that writes CRLF. `--task-file` reads it
    // verbatim, so every line break reaches the deck as `\r\n`.
    let task = format!(
        "This task file was saved with Windows line endings.\r\n\r\nUse Bash to run cat {first_sentinel} and print exactly what it contains, then wait.\r\n"
    );
    let staging = common::harness_tempdir().expect("task file staging dir");
    let task_file = staging.path().join("task.md");
    std::fs::write(&task_file, &task).expect("write CRLF task file");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["dispatch", name, "--single", "--task-file"])
        .arg(&task_file)
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("run dispatch");
    assert!(
        output.status.success(),
        "dispatch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let read_log = || std::fs::read_to_string(deck.workdir().join(log_name)).unwrap_or_default();
    let record = || dispatched_record(&deck, name);
    assert!(
        common::wait_until(Duration::from_secs(90), || record()
            .and_then(|r| r.live)
            .and_then(|live| live.last_user_prompt)
            .is_some_and(|prompt| prompt.contains(first_sentinel))),
        "the CRLF task never started a turn in the dispatched pane.\nDelivery log:\n{}\nPane:\n{}",
        delivery_log_evidence(&read_log()),
        record()
            .map(|r| rendered_pane(&deck, &r.id))
            .unwrap_or_default()
    );
    let agent_id = record().expect("dispatched record seen above").id;
    let pane_id = record()
        .and_then(|r| r.pane_id_env)
        .expect("dispatched pane id");
    let printed_first = common::wait_for_pane_text_on(
        deck.attach_socket_path(),
        &agent_id,
        first_contents,
        Duration::from_secs(90),
    );
    // The retry backoff's floor for Claude Code is 2 s, so by the time the
    // agent has run a command and printed its output a delivery the daemon
    // did not recognise as submitted has already been typed in again.
    let log = read_log();
    let lines = pane_delivery_log_lines(&log, &pane_id);
    let confirmed = lines
        .iter()
        .any(|line| line.contains("prompt delivery confirmed by the agent"));
    let resubmitted = lines
        .iter()
        .any(|line| line.contains("prompt delivery unconfirmed; re-submitting"));
    let payload_writes = lines
        .iter()
        .filter(|line| payload_write_attempt(line).is_some())
        .count();
    assert!(
        printed_first && confirmed && !resubmitted && payload_writes == 1,
        "a CRLF task must be submitted exactly once: confirmed by the agent's own report of it on the first write, and never typed into the pane again. printed_first={printed_first}, confirmed={confirmed}, resubmitted={resubmitted}, payload_writes={payload_writes}\nDelivery log for {pane_id}:\n{}\nPane:\n{}",
        lines.join("\n"),
        rendered_pane(&deck, &agent_id)
    );

    // Let the first turn finish so the follow-up lands in an idle composer, as
    // a person sending it from the desktop app would see it: the card reads
    // Idle (Claude's `Stop` hook) and stays Idle a moment, since text typed
    // while Claude is still running its turn-end hooks is not what this checks.
    let idle = || {
        record()
            .and_then(|r| r.live)
            .is_some_and(|live| live.status == dot_agent_deck::state::SessionStatus::Idle)
    };
    let settled_idle = common::wait_until(Duration::from_secs(120), || {
        idle() && {
            std::thread::sleep(Duration::from_secs(3));
            idle()
        }
    });
    assert!(
        settled_idle,
        "the first turn never ended, so the follow-up has no idle composer to land in.\nPane:\n{}",
        rendered_pane(&deck, &agent_id)
    );
    let follow_up = format!(
        "One more check, sent with an old Mac line break.\rUse Bash to run cat {second_sentinel} and print exactly what it contains."
    );
    // Named the way the TUI names it: a pane with a live conversation refuses
    // an unnamed write as `stale` and answers with the conversation's id, which
    // the retry then names (issues #608, #621).
    let submit = |session: Option<&str>| {
        common::write_and_submit_with_identity_on(
            deck.attach_socket_path(),
            &pane_id,
            &follow_up,
            &agent_id,
            session,
        )
        .expect("write-and-submit the follow-up")
    };
    let mut response = submit(None);
    if response.send_result == Some(dot_agent_deck::event::SendResult::Stale)
        && let Some(session) = response.current_session_id.clone()
    {
        response = submit(Some(&session));
    }
    assert_eq!(
        response.send_result,
        Some(dot_agent_deck::event::SendResult::Applied),
        "the follow-up was not written: {response:?}"
    );
    let follow_up_started = common::wait_until(Duration::from_secs(60), || {
        record()
            .and_then(|r| r.live)
            .and_then(|live| live.last_user_prompt)
            .is_some_and(|prompt| prompt.contains(second_sentinel))
    });
    let printed_second = follow_up_started
        && common::wait_for_pane_text_on(
            deck.attach_socket_path(),
            &agent_id,
            second_contents,
            Duration::from_secs(90),
        );
    assert!(
        follow_up_started && printed_second,
        "a prompt whose lines are separated by a bare CR must be submitted, not left in the agent's composer. follow_up_started={follow_up_started}, printed_second={printed_second}\nPane:\n{}",
        rendered_pane(&deck, &agent_id)
    );
}
