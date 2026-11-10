#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! What the installed Claude Code's external-editor action does, measured
//! against the `editor_probe` facts the doorbell holds for Claude Code (PRD
//! #1666 part 3, activation; issue #2146: the installed release, not a pinned
//! one, so a release that breaks a fact turns these tests red).
//!
//! Each test runs `tests/support/claude_editor_measure.py` in a pane of the
//! deck's TUI harness, with the harness's own HOME and work directory, never
//! the host's. The recorder launches the `claude` the harness's `PATH`
//! resolves on its own PTY, sets the
//! user's editor to a stand-in that logs its argv, environment and export and
//! then keeps or rewrites the file, and drives Ctrl+G and the other keys. No
//! prompt is submitted; a private UserPromptSubmit hook blocks any that is,
//! so no model turn starts.
//!
//! The capture root is printed and kept under `/var/tmp`: `result.json`
//! holds every row, and each launch's raw PTY bytes are beside it.

dot_agent_deck_test_harness::link!();
#[path = "support/installed_agent.rs"]
mod installed;

use serde_json::Value;
use spec::spec;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Capture {
    root: PathBuf,
    /// The installed release the recorder ran.
    release: String,
    result: Value,
}

impl Capture {
    fn rows(&self, cell: &str) -> Vec<&Value> {
        self.result["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["cell"] == cell)
            .collect()
    }
}

/// Runs one recorder mode on the installed Claude Code, or `None` after
/// reporting why it cannot run (`SKIP:`).
fn measure(mode: &str, wait: Duration) -> Option<Capture> {
    if common::_skip_if_err(common::check_claude_available()) {
        return None;
    }
    let claude = installed::or_skip(installed::claude())?;
    let python = std::process::Command::new("python3")
        .args(["-c", "import pyte"])
        .output()
        .unwrap();
    assert!(python.status.success(), "the recorder requires Python pyte");
    let root = tempfile::Builder::new()
        .prefix("dad-claude-editor-measure-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    let output = root.join("capture");
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let script = root.join("claude_editor_measure.py");
    std::fs::write(&script, include_str!("support/claude_editor_measure.py")).unwrap();
    let command = format!("exec python3 {} {mode} {}", quote(&script), quote(&output));
    let deck = common::TuiDeck::builder()
        .with_env("PATH", claude.path.clone())
        .with_env(
            "PYTHONPATH",
            std::env::var("PYTHONPATH").unwrap_or_default(),
        )
        .with_env(
            "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS",
            (wait.as_secs() + 120).to_string(),
        )
        .with_pty_size(220, 60)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_continue_session("editor-measure", command)
        .launch_with_fixture("minimal");
    assert!(
        common::wait_until(wait, || output.join("result.json").exists()
            || root.join("launcher-error.json").exists()),
        "measurement did not finish; {}\n{}",
        output.display(),
        deck.snapshot_grid()
    );
    assert!(
        !root.join("launcher-error.json").exists(),
        "recorder setup failed: {}",
        std::fs::read_to_string(root.join("launcher-error.json")).unwrap_or_default()
    );
    let result: Value =
        serde_json::from_slice(&std::fs::read(output.join("result.json")).unwrap()).unwrap();
    eprintln!(
        "Claude {} {mode} evidence: {}",
        result["release"].as_str().unwrap(),
        output.display()
    );
    claude.assert_recorded(&result, &output);
    assert!(
        result["error"].is_null(),
        "measurement premise differed: {}; {}",
        result["error"],
        output.display()
    );
    let hooks = std::fs::read_to_string(output.join("hook.jsonl")).unwrap_or_default();
    assert_eq!(hooks.lines().count(), 0, "nothing may be submitted");
    drop(deck);
    Some(Capture {
        root: output,
        release: claude.release,
        result,
    })
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn count(modes: &Value, wanted: &str) -> usize {
    modes
        .as_array()
        .map(|m| m.iter().filter(|x| *x == wanted).count())
        .unwrap_or(0)
}

/// Scenario: On the installed Claude Code, type a draft and press Ctrl+G five times with the user's editor set in VISUAL to a logging stand-in that rewrites the file, then once with the stand-in exiting 37; launch again with only EDITOR set, and twice with neither, the stand-in installed as vi and nano and then as code, vi and nano. Each invocation's argv, export path and contents, and the bracketed-paste mode writes Claude makes around it, are recorded.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_editor_export_selection_and_paste_toggles() {
    editor_export_selection_and_paste_toggles();
}

/// The `probe_088_live_claude_editor_export_selection_and_paste_toggles` measurement on the installed release.
fn editor_export_selection_and_paste_toggles() {
    let Some(c) = measure("editor", Duration::from_secs(300)) else {
        return;
    };
    let mut toggles = Vec::new();
    for row in c.rows("visual") {
        let trial = &row["trial"];
        assert_eq!(
            row["opened"],
            true,
            "visual trial {trial}; {}",
            c.root.display()
        );
        let rule = &row["export_rule"];
        assert_eq!(rule["last_argument"], true, "trial {trial}: {rule}");
        assert_eq!(
            rule["under_tmpdir_claude_uid"], true,
            "trial {trial}: {rule}"
        );
        assert_eq!(row["export_had_draft"], row["draft"], "trial {trial}");
        assert!(
            row["start"]["argv"][3] == "visual",
            "VISUAL selects the editor, split on spaces: {}",
            row["start"]["argv"]
        );
        assert!(
            row["composer_after"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("M317_EDITED_{trial}")),
            "trial {trial}: Claude must reimport the editor's file; {}",
            row["screen_after"]
        );
        toggles.push((
            count(&row["modes_before"], "\x1b[?2004l"),
            count(&row["modes_before"], "\x1b[?2004h"),
            count(&row["modes_after"], "\x1b[?2004l"),
            count(&row["modes_after"], "\x1b[?2004h"),
        ));
        eprintln!(
            "visual {trial}: before={} during={} after={}",
            row["modes_before"], row["modes_during"], row["modes_after"]
        );
    }
    assert_eq!(toggles.len(), 5);
    assert!(
        toggles.iter().all(|t| *t == toggles[0]),
        "the ?2004 writes differ between trials: {toggles:?}"
    );
    // The value, not only its equality across trials: one `?2004l` before
    // the editor and one `?2004h` after it, none the other way round. This
    // is the `VISUAL` path, the one the deck's helper is reached by. The
    // other cells are printed, not asserted: in 2.1.317's capture (and in
    // 2.1.318's, 2026-11-04) the
    // `EDITOR` and PATH `vi` rows wrote the same, and the PATH `code -w -g`
    // fallback wrote no `?2004` at all.
    assert_eq!(
        toggles[0],
        (1, 0, 0, 1),
        "the ?2004 writes on the VISUAL path (before l, before h, after l, after h)"
    );
    eprintln!(
        "PASTE-TOGGLES (VISUAL path) before: {}l {}h; after: {}l {}h",
        toggles[0].0, toggles[0].1, toggles[0].2, toggles[0].3
    );
    let rc = c.rows("visual-rc37");
    eprintln!(
        "RC37: opened={} composer_after={:?}",
        rc[0]["opened"], rc[0]["composer_after"]
    );
    for row in c.rows("editor") {
        assert_eq!(
            row["opened"], true,
            "EDITOR must be used when VISUAL is empty"
        );
        assert_eq!(row["start"]["argv"][3], "editor");
    }
    for (cell, chosen) in [
        ("path-vi-nano", "path-vi"),
        ("path-code-vi-nano", "path-code"),
    ] {
        let row = c.rows(cell)[0];
        eprintln!(
            "{cell}: opened={} argv={} ?2004 before l/h {}/{} after l/h {}/{}",
            row["opened"],
            row["start"]["argv"],
            count(&row["modes_before"], "\x1b[?2004l"),
            count(&row["modes_before"], "\x1b[?2004h"),
            count(&row["modes_after"], "\x1b[?2004l"),
            count(&row["modes_after"], "\x1b[?2004h"),
        );
        assert_eq!(row["opened"], true, "{cell}: no editor ran");
        assert_eq!(row["start"]["argv"][3], chosen, "{cell}");
    }
}

/// Scenario: Launch the installed Claude Code five times with a keybindings.json in its HOME that unbinds Ctrl+G and binds Ctrl+E to the external editor, and at the first composer frame type a draft, then press Ctrl+G and Ctrl+E. Ctrl+G must open no editor and Ctrl+E must open it, every time.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_keymap_loaded_at_start() {
    keymap_loaded_at_start();
}

/// The `probe_088_live_claude_keymap_loaded_at_start` measurement on the installed release.
fn keymap_loaded_at_start() {
    let Some(c) = measure("keymap-start", Duration::from_secs(300)) else {
        return;
    };
    let rows = c.rows("keymap-start");
    assert_eq!(rows.len(), 5);
    for row in rows {
        eprintln!(
            "start {}: first composer {} ms after spawn; ctrlg opened={} ctrle opened={}",
            row["trial"],
            row["ms_from_spawn_to_first_composer"],
            row["ctrlg"]["opened"],
            row["ctrle"]["opened"]
        );
        assert_eq!(row["ctrlg"]["opened"], false, "{}", c.root.display());
        assert_eq!(row["ctrle"]["opened"], true, "{}", c.root.display());
    }
}

/// Scenario: Launch the installed Claude Code with the Ctrl+G-unbinding keybindings.json present, leave the file untouched, and press Ctrl+G once a minute for ten minutes, then Ctrl+E. Ctrl+G must open no editor at any sample, the file's stat must not change, and Ctrl+E must still open the editor.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_keymap_unchanged_ten_minutes() {
    keymap_unchanged_ten_minutes();
}

/// The `probe_088_live_claude_keymap_unchanged_ten_minutes` measurement on the installed release.
fn keymap_unchanged_ten_minutes() {
    let Some(c) = measure("keymap-unchanged", Duration::from_secs(900)) else {
        return;
    };
    let rows = c.rows("keymap-unchanged");
    assert_eq!(rows.len(), 10);
    for row in rows {
        assert_eq!(row["stat_unchanged"], true);
        assert_eq!(row["ctrlg"]["opened"], false, "minute {}", row["minute"]);
    }
    assert_eq!(
        c.rows("keymap-unchanged-positive")[0]["ctrle"]["opened"],
        true
    );
}

/// Scenario: Launch the installed Claude Code with no keybindings.json, confirm Ctrl+G opens the editor, then create the unbinding file and alternate Ctrl+G and Ctrl+E until the binding has moved, then remove the file and alternate again until it has moved back. The time each change took to show is recorded.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_keymap_created_and_removed() {
    keymap_created_and_removed();
}

/// The `probe_088_live_claude_keymap_created_and_removed` measurement on the installed release.
fn keymap_created_and_removed() {
    let Some(c) = measure("keymap-reload", Duration::from_secs(300)) else {
        return;
    };
    assert_eq!(c.rows("keymap-reload-baseline")[0]["ctrlg"]["opened"], true);
    for change in ["created", "removed"] {
        let row = c.rows(&format!("keymap-reload-{change}"))[0];
        let last = row["samples"].as_array().unwrap().last().unwrap();
        eprintln!(
            "{change}: flipped={} after {} samples, at {} s",
            row["flipped"],
            row["samples"].as_array().unwrap().len(),
            last["elapsed_s"]
        );
        assert_eq!(row["flipped"], true, "{change}; {}", c.root.display());
    }
}

/// Scenario: Put the Ctrl+G-unbinding keybindings.json in the project's own .claude directory instead of the HOME, launch the installed Claude Code there, once with user settings only and once with every settings source as a deck pane launches it, type a draft and press Ctrl+G, then Ctrl+E. The result shows whether the project file is a source of key bindings.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_project_keymap() {
    project_keymap();
}

/// The `probe_088_live_claude_project_keymap` measurement on the installed release.
fn project_keymap() {
    let Some(c) = measure("keymap-project", Duration::from_secs(180)) else {
        return;
    };
    for cell in ["keymap-project", "keymap-project-all-sources"] {
        let row = c.rows(cell)[0];
        eprintln!(
            "PROJECT-KEYMAP {cell}: ctrlg opened={} ctrle opened={}",
            row["ctrlg"]["opened"], row["ctrle"]["opened"]
        );
    }
}

/// Scenario: In the installed Claude Code, put the composer in each of six states (a draft, the slash menu, help, reverse search, shell mode and after Escape twice) and send each of the twelve candidate wire forms of Ctrl+G, whole and split in two writes; then Escape and BEL with 0, 30 and 60 ms between them, and a bracketed paste holding a BEL, whole and split. Whether each opened the editor, and what the next export held after the paste, is recorded.
#[spec("inbox/probe/089")]
#[test]
fn probe_089_live_claude_ctrl_g_forms() {
    ctrl_g_forms();
}

/// The `probe_089_live_claude_ctrl_g_forms` measurement on the installed release.
fn ctrl_g_forms() {
    let Some(c) = measure("forms", Duration::from_secs(1500)) else {
        return;
    };
    let mut table: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for row in c.rows("forms") {
        let key = row["form"].as_str().unwrap().to_string();
        let e = table.entry(key).or_default();
        e.0 += 1;
        e.1 += usize::from(row["opened"] == true);
    }
    for (form, (n, opened)) in &table {
        eprintln!("FORM {form}: opened {opened}/{n}");
    }
    let bel = table["bel"];
    assert!(bel.1 > 0, "plain BEL must open the editor somewhere");
    // The pasted BEL is read from the next export, the bytes the editor
    // stand-in read from the file, never from `composer_after`: that is the
    // screen, where a BEL kept in the draft is not drawn, so it reads
    // `BASEPASTETAIL` whether the BEL was kept or dropped. Claude Code's
    // facts say "kept as text" (their `pasted_bel_is_text` stays `Absent`,
    // the safe default), measured on 2.1.317 and on 2.1.318 (2026-11-04), so
    // this test holds that reading on the installed release: a run that
    // reads "dropped" fails rather than only printing the flip.
    let pastes = c.rows("paste-bel");
    for split in [false, true] {
        assert!(
            pastes.iter().any(|row| row["split"] == split),
            "no paste-bel row with split={split}; {}",
            c.root.display()
        );
    }
    for row in pastes {
        let export = hex_bytes(row["next_export_hex"].as_str().unwrap_or_default());
        let reading = match export.as_slice() {
            b"BASEPASTE\x07TAIL" => "kept as text",
            b"BASEPASTETAIL" => "dropped",
            _ => "neither",
        };
        eprintln!(
            "PASTE split={} opened={} next_export={:?} BEL {reading} (screen showed {:?})",
            row["split"],
            row["opened"],
            String::from_utf8_lossy(&export),
            row["composer_after"]
        );
        assert_ne!(
            reading,
            "neither",
            "the next export after the paste is neither reading; {}",
            c.root.display()
        );
        assert_eq!(
            reading,
            "kept as text",
            "{}: 2.1.317 and 2.1.318 were measured keeping the pasted BEL as text; {}",
            c.release,
            c.root.display()
        );
    }
}
