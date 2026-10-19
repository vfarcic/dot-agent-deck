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

/// Scenario: Leave interactive Codex idle in its deck pane and post a sentinel task. The fixed doorbell and real submit hook must wake Codex, acknowledge the message, create the sentinel and show an envelope on its card.
#[spec("inbox/codex/009")]
#[test]
fn codex_009_live_idle_doorbell_injects_and_acks() {
    skip_unless!(common::check_codex_available());
    let codex = Codex::new(false);
    let name = "codex-idle-19cd36.txt";
    let id = codex.post(name);
    codex.done(&id, name);
    assert!(
        codex
            .deck
            .wait_for_grid_string_within("✉", Duration::from_secs(15)),
        "idle delivery card must show envelope; {}",
        codex.evidence()
    );
    assert!(
        codex.log().contains("doorbell rang"),
        "{}",
        codex.evidence()
    );
    assert!(codex.log().contains("DoorbellHook"), "{}", codex.evidence());
    assert!(codex.screen().contains("DONE"), "{}", codex.evidence());
}

/// Scenario: Queue a task while real Codex displays its folder-trust selection. The dialog must stay on the same choice until the user accepts it, after which Codex must receive, act on and acknowledge the task.
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
    let _input = codex.input("\r");
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
