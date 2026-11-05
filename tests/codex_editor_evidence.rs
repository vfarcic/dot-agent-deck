//! PRD #1666 part 3, U4: Codex 0.164.0's external-editor evidence, replayed
//! through the deck's own readers (`tests/fixtures/codex-inbox/
//! release-0.164.0/editor-probe/`, its README says how it was recorded).
//! Every check is in `support/codex_editor_evidence.rs`, which the lane-2
//! recorder test (`tests/e2e_codex_editor_measure.rs`) runs over a fresh
//! recording too. The proposed `editor_probe` is
//! `PROPOSED_CODEX_0164_EDITOR_PROBE` there; nothing here wires it into the
//! deck's facts.

#![cfg(unix)]

use spec::spec;

#[path = "support/codex_editor_evidence.rs"]
mod evidence;

#[path = "../src/test_temp.rs"]
mod test_temp;

/// Scenario: Read the committed Codex 0.164.0 editor recording, made with a stand-in editor in `VISUAL`/`EDITOR`, no credential and no network. Five Ctrl+G presses under `VISUAL` each hand the editor `$CODEX_HOME/editor/.tmpXXXXXX.md` holding the draft as its last argument, with the quoted word of `VISUAL` kept as one argument, and the rewritten file comes back into the box; every export is 0600, in the `editor/` directory Codex created 0775; `EDITOR` runs when `VISUAL` is unset; a set but empty one, or neither (even with `vi`, `nano` and `code` on `PATH`), opens nothing and draws its own error line; an editor exiting 37 keeps the draft and says so.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_codex_164_export_and_selection() {
    evidence::check_export_and_selection(&evidence::committed_dir());
}

/// Scenario: Replay every committed Codex 0.164.0 editor invocation's raw bytes through the deck's return-marker reader: each Ctrl+G that ran the editor wrote exactly two `?2004l` and then one `?2004h`, and the deck reads Codex's return (E1F) after it; a Ctrl+G that opened no editor wrote no `?2004` at all.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_codex_164_paste_toggles_by_the_decks_reader() {
    let counts = evidence::check_paste_toggles(&evidence::committed_dir());
    assert_eq!(counts.iter().filter(|(_, d, _)| *d == 2).count(), 14);
}

/// Scenario: Read ten committed Codex 0.164.0 launches that press Ctrl+G at the first ready composer and then again until the editor opens: the first press opens nothing and draws nothing in every launch, and every launch's editor opens within 1.5 s of its first ready composer.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_codex_164_ignores_ctrl_g_just_after_start() {
    let (last_miss, first_open) = evidence::check_startup(&evidence::committed_dir());
    eprintln!(
        "latest press that opened nothing {last_miss:.2} s after ready; earliest that opened {first_open:.2} s"
    );
}

/// Scenario: Read seven committed Codex 0.164.0 launches on one `CODEX_HOME`, each left 8 s past its first ready composer: the first four each rewrite Codex's own `config.toml` (the first adds `screen_reader_detection_done` and a model-availability notice count of 1, the next three raise it to 4, each after its first ready composer), and the fifth to seventh leave it as it was.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_codex_164_rewrites_its_config_on_its_first_four_starts() {
    let delays = evidence::check_config_writes(&evidence::committed_dir());
    eprintln!("config.toml written {delays:?} ms after the first ready composer");
}

/// Scenario: Read the committed Codex 0.164.0 wire-form recording: from a draft, Ctrl+G as BEL and as the CSI-u 103 implicit, press and repeat forms opens the editor, whole and split; a bracketed paste holding a BEL opens nothing, and the next export holds `BASEPASTETAIL`, the BEL removed, in all four pastes.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_codex_164_ctrl_g_forms_and_pasted_bel() {
    for (state, opened) in evidence::check_forms(&evidence::committed_dir()) {
        eprintln!("{state}: {opened:?}");
    }
}

/// Scenario: Replay both committed Codex 0.164.0 E1F recordings through the deck's return-marker reader: in each, twenty Ctrl+G presses per arm, with `LATE` typed at E1's `?2004h`, at the first `?2026h` after it (where the deck reads E1F), or while the editor runs. `LATE` typed at the deck's E1F is in the next export every time (40 of 40), typed during the editor it is lost every time, and no `?2026h` falls between Codex's disable and its enable; typed at E1's `?2004h` alone, before the deck's E1F frame, it was lost in 2 of the first run's 18 such trials (its other 2 wrote it after that frame) and in none of the rerun's 20.
#[spec("inbox/probe/093")]
#[test]
fn probe_093_codex_164_e1f_release_premise() {
    let dir = evidence::committed_dir();
    let first = evidence::check_e1f(&dir);
    let rerun = evidence::check_e1f(&dir.join("e1f-rerun"));
    for (run, arms) in [("first", &first), ("rerun", &rerun)] {
        for (arm, kept, n) in arms.arms.iter() {
            eprintln!("{run} {arm}: LATE kept {kept}/{n}");
        }
        let (lost, e1_only, n) = arms.e1_only;
        eprintln!("{run} at-2004h before the frame: lost {lost}/{e1_only} of {n}");
    }
    // The E1-only arm, as each committed run measured it: the race is the
    // natural one (no stop inside Codex's flush), so only these counts. The
    // first run's t01 and t13 wrote LATE after the deck's E1F `?2026h`, so
    // they released at E1F, not at E1 alone, and are not counted.
    assert_eq!(first.e1_only, (2, 18, 20));
    assert_eq!(rerun.e1_only, (0, 20, 20));
}

/// Scenario: Rebuild each of the nineteen key-binding source cases a committed Codex 0.164.0 recording launched with, read them as the deck reads a Codex launch at spawn, and compare the deck's verdict with what Ctrl+G and F6 did in Codex: the deck never treats Ctrl+G as the editor where Codex moved it, its merge order (config, profile, project, `-c`) gives Codex's result in every case Codex started in a trusted folder, and it holds wherever a project file carries a keymap, including the untrusted folder whose file Codex ignores.
#[spec("inbox/probe/094")]
#[test]
fn probe_094_codex_164_keymap_resolver_against_codex() {
    let scratch = test_temp::tempdir().unwrap();
    for case in evidence::check_keymap(&evidence::committed_dir(), scratch.path()) {
        eprintln!(
            "{}: codex {:?}, merge {:?}, deck {:?}",
            case.name, case.codex, case.merge, case.deck
        );
    }
}

/// Scenario: Run the head-echo export's deny scan over every committed Codex 0.164.0 editor-probe file, both directories, and regenerate nothing: none may carry a username, a `/home/` path, a key, token or email.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_codex_164_editor_evidence_passes_the_deny_scan() {
    for dir in [
        evidence::committed_dir(),
        evidence::committed_dir().join("e1f-rerun"),
    ] {
        deny_scan(&dir);
    }
}

fn deny_scan(dir: &std::path::Path) {
    let root = env!("CARGO_MANIFEST_DIR");
    let output = std::process::Command::new("python3")
        .arg("-I")
        .arg(format!("{root}/tests/support/codex_editor_summary.py"))
        .arg("--scan")
        .arg(dir)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("the deny scan requires python3 on PATH");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
