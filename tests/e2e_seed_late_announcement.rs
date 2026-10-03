#![cfg(all(feature = "e2e", unix))]

//! L2 regression for issues #1005 / #1006 — a spawn-time seed must not be
//! written into a pane before the agent has announced itself.
//!
//! **Lane 1 on purpose.** `prompt/new-pane/016` is the PTY-attached real-Claude
//! test for the same user-visible scenario and it is lane 2, so it runs on no
//! runner anywhere; issue #1006 measured that it ALSO passes with the bug
//! present, because its `claude` boots fast enough to win the race the test
//! exists to exercise. This file carries no `e2e-live` term, so
//! `e2e-deterministic` runs it on every PR, and it reaches the failing ordering
//! by construction rather than by luck — the stand-in's announcement latency is
//! a parameter (`common::write_late_announcing_paste_agent`).
//!
//! The assertion is on what the agent ACTED UPON, never on the seed's text being
//! on screen: the deck writes the bytes BEFORE the delivery goes wrong, so a PTY
//! buffers them and the text survives the failure. What does not survive is the
//! ordering, which the stand-in reports directly.
//!
//! `prompt/pane-input/044` (issue #621) shares the fixture and the form driver:
//! a seed whose agent DID announce itself, to the daemon, while the deck's own
//! event stream missed it.

mod common;

use std::time::{Duration, Instant};

use common::TuiDeck;
use spec::spec;

/// The first line of the built-in dispatcher seed, submitted as bracketed paste.
const SEED_MARKER: &str = "You are an ordinary assistant";

/// Where the stand-in records its ordering evidence, relative to the pane's cwd.
const AGENT_LOG: &str = "late-announce.log";

/// How long the stand-in withholds its genuine `SessionStart`.
///
/// Comfortably past the 500 ms `SPAWN_TIME_READINESS_BUFFER` that the broken
/// gate writes on, and comfortably short of the 10 s `timeout_ready` slow path —
/// which would deliver the seed for an unrelated reason and make this test green
/// on a broken product.
const ANNOUNCE_AFTER_SECS: u64 = 3;

/// Drive the production New Agent form to spawn a dispatcher running `command`.
fn spawn_seeded_dispatcher(deck: &TuiDeck, command: &str) {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C"); // saturate at dispatcher
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name (default) → Command
    // PRD #196 pre-fills the Command field with the last command spawned, so
    // clear it before typing (the form has no clear-line key).
    deck.send_keys(&[0x7fu8; 64]);
    deck.send_keys(command.as_bytes());
    deck.send_keys(b"\r"); // submit
}

/// Lines the stand-in recorded, or a placeholder when it wrote nothing at all.
fn agent_log(deck: &TuiDeck) -> String {
    std::fs::read_to_string(deck.workdir().join(AGENT_LOG))
        .unwrap_or_else(|_| "<the stand-in wrote no log>".to_string())
}

/// Whether any live card on this deck reports the seed as its last submitted
/// prompt — the user-altitude "the agent acted on it" fact, read from the
/// daemon's own registry rather than from the screen.
fn seed_is_in_a_card_prompt_history(deck: &TuiDeck) -> bool {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .filter_map(|record| record.live)
        .filter_map(|live| live.last_user_prompt)
        .any(|prompt| prompt.contains(SEED_MARKER))
}

/// Scenario: Spawn the built-in dispatcher with a stand-in agent that declares its type at exec but
/// withholds its genuine `SessionStart` for three seconds, then reports whether
/// anything was already queued in its PTY when it announced. Nothing may have
/// been written before that announcement; afterwards the seed must arrive
/// exactly once and be reported back as submitted, so it appears in the card's
/// prompt history.
#[spec("prompt/new-pane/017")]
#[test]
fn new_pane_017_a_spawn_time_seed_waits_for_the_agent_to_announce_itself() {
    let deck = TuiDeck::launch_with_fixture("dispatcher-seed");
    deck.wait_for_string("No active agents");
    let stand_in =
        common::write_late_announcing_paste_agent(deck.workdir(), AGENT_LOG, ANNOUNCE_AFTER_SECS);
    let command = format!(
        "./{}",
        stand_in
            .file_name()
            .expect("stand-in has a name")
            .to_string_lossy()
    );

    spawn_seeded_dispatcher(&deck, &command);

    // The stand-in has announced itself and published its verdict on what was
    // already in the pane. Generous, because the wait covers dispatcher spawn,
    // the typed launch line, the fixed delay and two hook round trips.
    const ANNOUNCE_WAIT: Duration = Duration::from_secs(45);
    assert!(
        common::wait_until(ANNOUNCE_WAIT, || {
            let log = agent_log(&deck);
            log.contains(common::LATE_ANNOUNCE_CLEAN)
                || log.contains(common::LATE_ANNOUNCE_PREMATURE)
                || log.contains("probe-failed|")
        }),
        "the stand-in never reached its announcement within {}s — the fixture \
         itself did not run, so nothing below would mean anything.\nagent log:\n{}\n\
         grid:\n{}",
        ANNOUNCE_WAIT.as_secs(),
        agent_log(&deck),
        deck.snapshot_grid()
    );

    // THE ASSERTION. A resolved agent type is not an agent that can read: until
    // a producer announces a conversation, the pane's PTY must be untouched.
    // Read from the input queue itself, because the bytes of a premature write
    // are still delivered afterwards — the failure is invisible on screen.
    let log = agent_log(&deck);
    assert!(
        log.contains(common::LATE_ANNOUNCE_CLEAN),
        "the seed was already sitting in the pane's PTY when the agent announced \
         itself — written {}s early, into a program that had not started reading. \
         The probe reports `probe-failed|…` if it could not run at all, so read \
         the log before assuming a premature write.\nagent log:\n{log}",
        ANNOUNCE_AFTER_SECS
    );

    // ... and once the agent IS there, the seed arrives and is acted upon.
    const DELIVERY_WAIT: Duration = Duration::from_secs(45);
    assert!(
        common::wait_until(DELIVERY_WAIT, || {
            agent_log(&deck).contains(common::LATE_ANNOUNCE_RECEIVED)
        }),
        "the seed never reached the agent after it announced itself within {}s.\n\
         agent log:\n{}",
        DELIVERY_WAIT.as_secs(),
        agent_log(&deck)
    );
    assert!(
        common::wait_until(DELIVERY_WAIT, || seed_is_in_a_card_prompt_history(&deck)),
        "the agent never reported SUBMITTING the seed, so the card's prompt \
         history does not carry it — the payload reached the PTY but was not \
         acted upon.\nagent log:\n{}",
        agent_log(&deck)
    );

    // Exactly one payload, so the bounded replacement write (`MAX_PAYLOAD_SUBMISSIONS`)
    // is still available to whatever genuinely needs it. Sampled after the
    // confirmation above, which is the point at which the delivery finalizes.
    let log = agent_log(&deck);
    let copies = log
        .lines()
        .filter(|line| line.starts_with(common::LATE_ANNOUNCE_RECEIVED))
        .filter(|line| line.contains(SEED_MARKER))
        .count();
    assert_eq!(
        copies, 1,
        "the seed must be typed into the pane exactly once — a confirmed delivery \
         has no reason to spend the single bounded replacement payload.\n\
         agent log:\n{log}"
    );
}

/// Scenario: Spawn the built-in dispatcher with a stand-in that never sends a
/// `SessionStart`, and watch the first line of its seeded prompt. The pane must
/// receive one copy only after the 10-second readiness fallback and its buffer.
#[spec("prompt/new-pane/019")]
#[test]
fn new_pane_019_a_silent_agent_receives_one_seed_after_the_readiness_fallback() {
    let deck = TuiDeck::launch_with_fixture("dispatcher-seed");
    deck.wait_for_string("No active agents");
    let script = deck.workdir().join("claude");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         printf 'started\\n' >> silent-seed.log\n\
         while IFS= read -r line; do\n\
         \x20 printf 'received|%s\\n' \"$line\" >> silent-seed.log\n\
         done\n",
    )
    .expect("write silent stand-in");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("chmod silent stand-in");

    // This starts before submission and therefore before the seed's created_at.
    let submitted = Instant::now();
    spawn_seeded_dispatcher(&deck, "./claude");
    let log_path = deck.workdir().join("silent-seed.log");
    assert!(
        common::wait_for_file_substr_count(&log_path, "started", 1, Duration::from_secs(10)),
        "the silent stand-in must have started before timing seed delivery"
    );

    // A seed observed before this point escaped through a readiness path other
    // than the buffered fallback. The stand-in never announces a conversation.
    const FALLBACK_WITH_BUFFER: Duration = Duration::from_millis(10_500);
    assert!(
        common::wait_until(FALLBACK_WITH_BUFFER + Duration::from_secs(1), || {
            submitted.elapsed() >= FALLBACK_WITH_BUFFER
        }),
        "the readiness fallback observation window did not elapse"
    );
    let before_fallback = std::fs::read_to_string(&log_path).expect("stand-in log exists");
    assert!(
        !before_fallback.contains(SEED_MARKER),
        "the dispatcher seed arrived before the 10-second fallback plus readiness buffer.\nLog:\n{before_fallback}"
    );

    assert!(
        common::wait_for_file_substr_count(&log_path, SEED_MARKER, 1, Duration::from_secs(30)),
        "the silent stand-in did not receive the dispatcher seed through the readiness fallback.\nLog:\n{}",
        std::fs::read_to_string(&log_path).unwrap_or_default()
    );
    // Give a replacement attempt time to surface before counting copies.
    let delivered = Instant::now();
    assert!(
        common::wait_until(Duration::from_secs(4), || {
            delivered.elapsed() >= Duration::from_secs(3)
        }),
        "the replacement observation window did not elapse"
    );
    let log = std::fs::read_to_string(&log_path).expect("read stand-in log");
    let copies = log
        .lines()
        .filter(|line| line.contains(SEED_MARKER))
        .count();
    assert_eq!(copies, 1, "the seed must arrive exactly once.\nLog:\n{log}");
}

/// Scenario: Spawn the built-in dispatcher with a stand-in that announces its conversation at once, while the deck's own event stream misses that conversation's events (the reconnect window issue #621 describes, reproduced with the `e2e` build's drop seam). The daemon knows the conversation and the deck does not, so the deck's unnamed seed is refused; the seed must still reach the agent well before the 60-second delivery deadline.
#[spec("prompt/pane-input/044")]
#[test]
fn pane_input_044_a_seed_whose_session_start_the_deck_missed_is_still_delivered() {
    // The prefix the stand-in's genuine conversation id carries, and the one
    // the TUI's subscriber is told to drop. Every event of that conversation is
    // lost to the TUI — the start AND the daemon's `ShellIdle` that follows it,
    // either of which would teach the TUI the generation — while the daemon
    // applies them as normal.
    const DROPPED_PREFIX: &str = "dropped-start-";
    const LOG: &str = "dropped-start.log";
    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_E2E_DROP_SESSION_EVENTS", DROPPED_PREFIX)
        .launch_with_fixture("dispatcher-seed");
    deck.wait_for_string("No active agents");

    // A stand-in that announces exactly one conversation — no wrapper-fork
    // boot start beside it, which the deck WOULD see and bind, turning this
    // into a bound delivery against a different generation (a case the fix
    // deliberately does not redirect). It then records every line it reads.
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let quoted_bin = format!("'{}'", bin.replace('\'', r"'\''"));
    let script = deck.workdir().join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             printf '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"{DROPPED_PREFIX}%s\"}}' \
             \"$DOT_AGENT_DECK_PANE_ID\" | {quoted_bin} hook --agent claude-code >/dev/null 2>&1 \
             || {{ printf 'hook-rejected\\n' >> {LOG}; exit 97; }}\n\
             printf 'announced\\n' >> {LOG}\n\
             while IFS= read -r line; do\n\
             \x20 printf 'received|%s\\n' \"$line\" >> {LOG}\n\
             done\n"
        ),
    )
    .expect("write announcing stand-in");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("chmod announcing stand-in");

    let submitted = Instant::now();
    spawn_seeded_dispatcher(&deck, "./claude");
    let log_path = deck.workdir().join(LOG);
    let log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        common::wait_for_file_substr_count(&log_path, "announced", 1, Duration::from_secs(20)),
        "precondition: the stand-in must have announced its conversation to the daemon.\n\
         Log:\n{}\ngrid:\n{}",
        log(),
        deck.snapshot_grid()
    );

    // With nothing announced in the deck's view, the 10-second fallback is the
    // door, and its first write goes out naming no conversation. Bounded well
    // inside the 60-second deadline so an abandoned seed fails here rather than
    // reading as slow.
    const DELIVERY_WAIT: Duration = Duration::from_secs(40);
    let delivered = common::wait_for_file_substr_count(&log_path, SEED_MARKER, 1, DELIVERY_WAIT);
    assert!(
        delivered,
        "the seed never reached the agent within {}s of submitting the form, although the \
         daemon knew its conversation — the deck's unnamed writes were refused and nothing \
         told it which conversation to name.\nLog:\n{}\ngrid:\n{}",
        submitted.elapsed().as_secs(),
        log(),
        deck.snapshot_grid()
    );
}
