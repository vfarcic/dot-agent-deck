#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! Real Codex through the wrapper and inbox hooks in a scratch HOME.
dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/codex_inbox_live.rs"]
mod live;
use live::{BUDGET, Codex};
use spec::spec;
use std::time::Duration;

/// Scenario: Leave interactive Codex idle in its deck pane, at whatever release is installed, with nothing typed into it, and post a sentinel task. The deck latches the installed release from its header and types the doorbell line into Codex once on Codex's facts, and Codex takes it with no prompt from the test: Codex's real prompt hook hands the task over, Codex creates the sentinel and the task is settled by its receipt.
#[spec("inbox/codex/009")]
#[test]
fn codex_009_live_idle_installed_release_is_rung_and_delivers() {
    skip_unless!(common::check_codex_available());
    skip_unless!(live::codex_facts_admit_installed_release());
    let installed = live::installed_codex_release().expect("the preflight read it");
    let codex = Codex::new(false);
    let name = "codex-idle-19cd36.txt";
    let id = codex.post(name);
    codex.done(&id, name);
    let log = codex.log();
    assert!(
        log.lines()
            .any(|line| line.contains("pane release")
                && line.contains(&format!("release={installed}"))),
        "the deck latched the installed release; {}",
        codex.evidence()
    );
    let rang: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("doorbell rang"))
        .collect();
    assert_eq!(rang.len(), 1, "one ring; {}", codex.evidence());
    assert!(rang[0].contains("rings=1"), "{}", codex.evidence());
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("doorbell attempt ended"))
            .map(|line| line.contains("outcome=Submitted"))
            .collect::<Vec<_>>(),
        [true],
        "one attempt, submitted; {}",
        codex.evidence()
    );
    for clause in ["missing_measurement", "turned_off"] {
        assert!(
            !log.contains(&format!("clause=\"{clause}\"")),
            "Codex's facts admit {installed}: no {clause}; {}",
            codex.evidence()
        );
    }
    // Before the header latches, the generation's release is not known yet,
    // so one `doorbell off … unknown_release` line at spawn is expected. Once
    // the installed release has latched, nothing may hold it as unknown.
    let after_latch = log
        .lines()
        .skip_while(|line| {
            !(line.contains("pane release") && line.contains(&format!("release={installed}")))
        })
        .skip(1);
    assert!(
        !after_latch
            .into_iter()
            .any(|line| line.contains("clause=\"unknown_release\"")),
        "Codex's facts admit {installed}: no unknown_release once it latched; {}",
        codex.evidence()
    );
    // The submitted line's prompt hook handed the task over.
    assert!(log.contains("DoorbellHook"), "{}", codex.evidence());
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

/// Scenario: Start interactive Codex with every one of the deck's hooks switched off, so none of them runs, type into its pane a prompt asking it to run `dot-agent-deck inbox fetch`, then post a sentinel task while it is idle. Because its box was typed into, the deck types nothing into Codex; on the prompt's Enter its model runs the fetch inside its own sandbox, that fetch reaches the deck as a one-shot receiver (a sandbox that refuses the socket is UNVERIFIED, not a pass), the sentinel is written once, and the task is settled by its receipt with no hook having fetched.
#[spec("inbox/codex/022")]
#[test]
fn codex_022_live_untrusted_hooks_a_model_run_fetch_delivers_on_receipts() {
    skip_unless!(common::check_codex_available());
    let codex = Codex::with_hooks(false, false);
    let name = "codex-untrusted-4e1c90.txt";
    // The prompt is typed before the task is posted: a Codex box something
    // was typed into is not rung until Codex restarts, so the no-ring
    // assertion below rests on that rule rather than on the prompt beating
    // the settle window.
    let mut input =
        codex.input("Run dot-agent-deck inbox fetch in the shell and do what the messages say.");
    codex.wait_screen("Run dot-agent-deck inbox fetch");
    let id = codex.post(name);
    let fetched = || {
        codex
            .log()
            .lines()
            .any(|line| line.contains("hook receiver's inbox fetch") && line.contains("Cli"))
    };
    // A painted prompt is the positive input barrier, but Codex's paste-burst
    // debounce must settle before the separate Enter, as in the busy test
    // above: an Enter written straight after the paint was taken into the
    // draft and the prompt never submitted (two of two runs on PRD #1666 part
    // 3, 2026-11-04). The wait doubles as a check that nothing fetched before
    // the prompt was submitted.
    assert!(
        !common::wait_until(Duration::from_secs(2), fetched),
        "nothing may fetch before the prompt is submitted; {}",
        codex.evidence()
    );
    input.write("\r");
    let reached = common::wait_until(BUDGET, fetched);
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

/// Issue #2346: start real Codex whose deck hooks, after printing a
/// hand-over, hold while `hold` exists; answers Codex, `hold` and the folder
/// holding it, which must outlive the test.
fn holding_codex() -> (Codex, std::path::PathBuf, tempfile::TempDir) {
    let dir = common::harness_tempdir().unwrap();
    let hold = dir.path().join("hold-after-print");
    let knob = hold.to_str().expect("a UTF-8 path").to_string();
    let codex = Codex::with_pane_env(
        false,
        true,
        &[(
            dot_agent_deck::inbox_oneshot::ENV_TEST_HOLD_AFTER_PRINT,
            knob.as_str(),
        )],
    );
    (codex, hold, dir)
}

/// Issue #2346: with a deck hook holding after it printed `id`'s hand-over,
/// interrupt Codex (Esc) in that window, release the hook, and check `id`
/// is neither acted on nor acknowledged for longer than the witness
/// watches; then have Codex take its messages and check it acts on `id`
/// once (one appended line), settled by its receipt with two deliveries.
fn interrupted_hand_over_is_handed_again(
    codex: &Codex,
    hold: &std::path::Path,
    id: &str,
    name: &str,
) {
    let mut held = hold.as_os_str().to_owned();
    held.push(".held");
    let held = std::path::PathBuf::from(held);
    assert!(
        common::wait_until(BUDGET, || held.exists()),
        "a deck hook must print the hand-over and hold; {}",
        codex.evidence()
    );
    let mut input = codex.input("\x1b");
    codex.wait_screen("Conversation interrupted");
    std::fs::remove_file(hold).unwrap();
    let sentinel = codex.deck.workdir().join(name);
    assert!(
        !common::wait_until(Duration::from_secs(30), || {
            sentinel.exists()
                || inbox::listing(&codex.deck, &codex.pane)["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["id"] == id && m["state"] == "acknowledged")
        }),
        "an interrupted hand-over is not acknowledged; {}",
        codex.evidence()
    );
    // The deck may ring Codex again by itself; if it has not, ask Codex.
    if !common::wait_until(Duration::from_secs(45), || sentinel.exists()) {
        input.write("Check for new deck messages and do what they say.");
        codex.wait_screen("Check for new deck messages");
        input.write("\r");
    }
    codex.done(id, name);
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "codex-inbox-action\n",
        "Codex acted on the task once; {}",
        codex.evidence()
    );
    let summary = inbox::listing(&codex.deck, &codex.pane)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .cloned()
        .unwrap_or_default();
    assert_eq!(summary["deliveries"], 2, "{summary}; {}", codex.evidence());
}

/// Scenario: Start interactive Codex whose deck hooks hold after printing a hand-over, and post a sentinel task while it is idle. The deck rings Codex (or, if a starved machine makes the deck withdraw its ring, the test submits the same doorbell line), Codex's real prompt hook prints the task as its context and holds, and the test presses Esc in Codex's pane, so Codex interrupts the turn and drops what the hook printed. The task must not be acted on or acknowledged for longer than the deck watches Codex's session log; once Codex takes its messages again it writes the sentinel once and the task is settled by its receipt with two deliveries.
#[spec("inbox/codex/031")]
#[test]
fn codex_031_live_interrupted_doorbell_hand_over_is_handed_again() {
    skip_unless!(common::check_codex_available());
    skip_unless!(live::codex_facts_admit_installed_release());
    let (codex, hold, _dir) = holding_codex();
    std::fs::write(&hold, b"").unwrap();
    let name = "codex-interrupted-doorbell-3d81.txt";
    let id = codex.post(name);
    // On a starved machine the deck can withdraw its own ring before Codex
    // submits it, and then holds the task until Codex takes a prompt; the
    // ring is `inbox/codex/009`'s to pin. What this test needs is Codex's
    // prompt hook handing the task over, so it then submits the same
    // doorbell line itself.
    let mut held = hold.as_os_str().to_owned();
    held.push(".held");
    let held = std::path::PathBuf::from(held);
    let withdrawn = || codex.log().contains("doorbell ring withdrawn");
    if !common::wait_until(BUDGET, || held.exists() || withdrawn()) || !held.exists() {
        let line = inbox::fixed_line(&id);
        let mut input = codex.input(line.trim_start());
        codex.wait_screen(&id);
        input.write("\r");
    }
    interrupted_hand_over_is_handed_again(&codex, &hold, &id, name);
}

/// Scenario: Start interactive Codex whose deck hooks hold after printing a hand-over, submit a slow shell directive, and post a sentinel task while its tool runs. At the turn's end Codex's real Stop hook prints the task as its block reason and holds, and the test presses Esc in Codex's pane, so Codex interrupts the turn and drops the reason. The task must not be acted on or acknowledged for longer than the deck watches Codex's session log; once Codex takes its messages again it writes the sentinel once and the task is settled by its receipt with two deliveries.
#[spec("inbox/codex/032")]
#[test]
fn codex_032_live_interrupted_stop_hand_over_is_handed_again() {
    skip_unless!(common::check_codex_available());
    let (codex, hold, _dir) = holding_codex();
    let mut input = codex.input("Run pwd, then run exactly this in the directory it prints: printf busy > codex-hold-started.txt; sleep 20. Then reply BUSY-DONE and end your turn.");
    codex.wait_screen("codex-hold-started.txt");
    input.write("\r");
    assert!(
        common::wait_until(BUDGET, || codex
            .deck
            .workdir()
            .join("codex-hold-started.txt")
            .exists()),
        "slow directive must start; {}",
        codex.evidence()
    );
    std::fs::write(&hold, b"").unwrap();
    let name = "codex-interrupted-stop-9a4c.txt";
    let id = codex.post(name);
    interrupted_hand_over_is_handed_again(&codex, &hold, &id, name);
    assert!(codex.log().contains("TurnEndHook"), "{}", codex.evidence());
}
