#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! Real Codex 0.160.0 through the wrapper and inbox hooks in a scratch HOME.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/codex_inbox_live.rs"]
mod live;
use live::{BUDGET, Codex};
use spec::spec;
use std::time::Duration;

/// Scenario: Leave interactive Codex idle in its deck pane and post a sentinel task. The deck types nothing into Codex: no doorbell rings, the task stays queued and Codex's card says `waiting for the agent to fetch it. Send it a prompt.` Then the test sends Codex a prompt in its pane, as a user would: its real hooks hand the task over at that turn, and Codex acknowledges it and creates the sentinel.
#[spec("inbox/codex/009")]
#[test]
fn codex_009_live_idle_is_held_and_a_prompt_delivers() {
    skip_unless!(common::check_codex_available());
    let codex = Codex::new(false);
    let name = "codex-idle-19cd36.txt";
    let id = codex.post(name);
    assert!(
        codex.deck.wait_for_grid_string_within(
            dot_agent_deck::inbox_display::WAKE_HOLD_CANNOT_TYPE,
            BUDGET
        ),
        "the card holds the task; {}",
        codex.evidence()
    );
    assert!(
        !codex.log().contains("doorbell rang"),
        "{}",
        codex.evidence()
    );
    inbox::state(&codex.deck, &codex.pane, &id, "queued");
    let mut input = codex.input("Check for new deck messages and do what they say.");
    codex.wait_screen("Check for new deck messages");
    input.write("\r");
    codex.done(&id, name);
    assert!(
        !codex.log().contains("doorbell rang"),
        "{}",
        codex.evidence()
    );
    // Its prompt hook or its Stop hook at that turn's end.
    assert!(
        codex.log().contains("DoorbellHook") || codex.log().contains("TurnEndHook"),
        "{}",
        codex.evidence()
    );
}

/// Scenario: Queue a task while real Codex displays its folder-trust selection. The dialog must stay on the same choice until the user accepts it; the deck then still types nothing into Codex, and once the test sends Codex a prompt, Codex must receive, act on and acknowledge the task.
#[spec("inbox/codex/010")]
#[test]
fn codex_010_live_folder_dialog_holds_until_answered() {
    skip_unless!(common::check_codex_available());
    let codex = Codex::new(true);
    let name = "codex-folder-6c1eda.txt";
    codex.wait_screen("› 1. Trust and continue");
    let id = codex.post(name);
    let selection = |screen: String| {
        screen
            .lines()
            .find(|line| line.trim_start().starts_with('›'))
            .map(|line| line.trim().to_owned())
    };
    let before = selection(codex.screen());
    assert_eq!(before.as_deref(), Some("› 1. Trust and continue"));
    assert!(
        !common::wait_until(Duration::from_secs(3), || selection(codex.screen())
            .is_some_and(|choice| Some(choice) != before)),
        "folder dialog selection changed without user input; {}",
        codex.evidence()
    );
    codex.wait_screen("› 1. Trust and continue");
    codex.wait_screen("Trust this folder?");
    assert!(
        !codex.log().contains("doorbell rang"),
        "{}",
        codex.evidence()
    );
    inbox::state(&codex.deck, &codex.pane, &id, "queued");
    let mut input = codex.input("\r");
    // The deck types nothing into Codex once the dialog is answered either:
    // the task waits until Codex is sent a prompt.
    codex.wait_screen("Ask Codex to do anything");
    assert!(!common::wait_until(Duration::from_secs(5), || codex
        .log()
        .contains("doorbell rang")));
    inbox::state(&codex.deck, &codex.pane, &id, "queued");
    input.write("Check for new deck messages and do what they say.");
    codex.wait_screen("Check for new deck messages");
    input.write("\r");
    codex.done(&id, name);
}

/// Scenario: Submit a slow shell directive to interactive Codex, then queue a task while its tool is running. The deck must avoid ringing mid-turn and hand the task over through the real Stop block, with visible work and acknowledgement.
#[spec("inbox/codex/011")]
#[test]
fn codex_011_live_busy_stop_hands_over_and_acks() {
    skip_unless!(common::check_codex_available());
    let codex = Codex::new(false);
    let mut input = codex.input("Run pwd, then run exactly this in the directory it prints: printf busy > codex-busy-started.txt; sleep 40; printf done > codex-busy-finished.txt. Then reply BUSY-DONE and end your turn.");
    // A painted prompt is the positive input barrier; Codex's paste-burst
    // debounce needs to settle before the separate Enter (the draft RED pins it).
    codex.wait_screen("codex-busy-started.txt");
    let recordings = common::current_test_recordings_dir();
    std::fs::create_dir_all(&recordings).unwrap();
    std::fs::write(
        recordings.join("composer-draft.txt"),
        codex
            .screen()
            .replace(codex.deck.workdir().to_str().unwrap(), "/scratch/project"),
    )
    .unwrap();
    assert!(!common::wait_until(Duration::from_secs(2), || codex
        .deck
        .workdir()
        .join("codex-busy-started.txt")
        .exists()));
    input.write("\r");
    assert!(
        common::wait_until(BUDGET, || codex
            .deck
            .workdir()
            .join("codex-busy-started.txt")
            .exists()),
        "slow directive must start; {}",
        codex.evidence()
    );
    assert!(
        !codex
            .deck
            .workdir()
            .join("codex-busy-finished.txt")
            .exists()
    );
    let name = "codex-busy-f085cd.txt";
    let id = codex.post(name);
    codex.done(&id, name);
    assert!(
        !codex.log().contains("doorbell rang"),
        "busy delivery rang; {}",
        codex.evidence()
    );
    assert!(codex.log().contains("TurnEndHook"), "{}", codex.evidence());
    assert!(
        codex.screen().contains("Blocked by hook"),
        "{}",
        codex.evidence()
    );
}

/// Scenario: Start interactive Codex with every one of the deck's hooks switched off, so none of them runs, and post a sentinel task while it is idle. The deck types nothing into Codex; the test asks Codex in its pane to run `dot-agent-deck inbox fetch`, and its model runs it inside its own sandbox, that fetch reaches the deck as a one-shot receiver (a sandbox that refuses the socket is UNVERIFIED, not a pass), the sentinel is written once, and the task is settled by its receipt with no hook having fetched.
#[spec("inbox/codex/022")]
#[test]
fn codex_022_live_untrusted_hooks_a_model_run_fetch_delivers_on_receipts() {
    skip_unless!(common::check_codex_available());
    let codex = Codex::with_hooks(false, false);
    let name = "codex-untrusted-4e1c90.txt";
    let id = codex.post(name);
    let mut input =
        codex.input("Run dot-agent-deck inbox fetch in the shell and do what the messages say.");
    codex.wait_screen("Run dot-agent-deck inbox fetch");
    input.write("\r");
    let reached = common::wait_until(BUDGET, || {
        codex
            .log()
            .lines()
            .any(|line| line.contains("hook receiver's inbox fetch") && line.contains("Cli"))
    });
    assert!(
        reached,
        "UNVERIFIED: Codex's model-run `inbox fetch` never reached the deck (a sandbox that refuses the socket is not a pass); {}",
        codex.evidence()
    );
    codex.done(&id, name);
    let log = codex.log();
    assert!(!log.contains("doorbell rang"), "{}", codex.evidence());
    assert!(
        !log.contains("DoorbellHook") && !log.contains("TurnEndHook"),
        "no deck hook fetched: {}",
        codex.evidence()
    );
    assert_eq!(
        std::fs::read_to_string(codex.deck.workdir().join(name))
            .unwrap()
            .matches("codex-inbox-action")
            .count(),
        1,
        "the sentinel is written once"
    );
}
