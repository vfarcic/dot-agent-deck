#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! A measurement, not a contract: what one Claude Code release's
//! external-editor action does, for the `editor_probe` field of its
//! `doorbell_activation` record (PRD #1666 part 3, activation).
//!
//! Each test runs `tests/support/claude_editor_measure.py` in a pane of the
//! deck's TUI harness, with the harness's own HOME and work directory, never
//! the host's. The recorder launches the pinned executable that
//! `DOT_AGENT_DECK_CLAUDE_MENU_<release>` names on its own PTY, sets the
//! user's editor to a stand-in that logs its argv, environment and export and
//! then keeps or rewrites the file, and drives Ctrl+G and the other keys. No
//! prompt is submitted; a private UserPromptSubmit hook blocks any that is,
//! so no model turn starts.
//!
//! The capture root is printed and kept under `/var/tmp`: `result.json`
//! holds every row, and each launch's raw PTY bytes are beside it.

mod common;

use serde_json::Value;
use spec::spec;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RELEASE: &str = "2.1.317";

/// A private executable search path whose `claude` is the pinned executable.
fn pinned_path(release: &str) -> Result<(tempfile::TempDir, String), String> {
    let key = format!("DOT_AGENT_DECK_CLAUDE_MENU_{}", release.replace('.', "_"));
    let binary = std::env::var(&key).map_err(|_| format!("set {key} to the pinned executable"))?;
    let root = tempfile::Builder::new()
        .prefix("dad-claude-editor-bin-")
        .tempdir_in("/var/tmp")
        .unwrap();
    std::os::unix::fs::symlink(&binary, root.path().join("claude")).unwrap();
    let path = format!(
        "{}:{}",
        root.path().display(),
        std::env::var("PATH").unwrap()
    );
    Ok((root, path))
}

struct Capture {
    root: PathBuf,
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

fn measure(mode: &str, wait: Duration) -> Option<Capture> {
    let (_bin, path) = match pinned_path(RELEASE) {
        Ok(p) => p,
        Err(why) => {
            common::_skip_if_err(Err::<(), _>(why));
            return None;
        }
    };
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
        .with_env("PATH", path)
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
    assert_eq!(result["release"], RELEASE);
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
        result,
    })
}

fn count(modes: &Value, wanted: &str) -> usize {
    modes
        .as_array()
        .map(|m| m.iter().filter(|x| *x == wanted).count())
        .unwrap_or(0)
}

/// Scenario: On pinned Claude Code 2.1.317, type a draft and press Ctrl+G five times with the user's editor set in VISUAL to a logging stand-in that rewrites the file, then once with the stand-in exiting 37; launch again with only EDITOR set, and twice with neither, the stand-in installed as vi and nano and then as code, vi and nano. Each invocation's argv, export path and contents, and the bracketed-paste mode writes Claude makes around it, are recorded.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_317_editor_export_selection_and_paste_toggles() {
    skip_unless!(common::check_claude_available());
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
    eprintln!(
        "PASTE-TOGGLES before: {}l {}h; after: {}l {}h",
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
            "{cell}: opened={} argv={}",
            row["opened"], row["start"]["argv"]
        );
        assert_eq!(row["opened"], true, "{cell}: no editor ran");
        assert_eq!(row["start"]["argv"][3], chosen, "{cell}");
    }
}

/// Scenario: Launch pinned Claude Code 2.1.317 five times with a keybindings.json in its HOME that unbinds Ctrl+G and binds Ctrl+E to the external editor, and at the first composer frame type a draft, then press Ctrl+G and Ctrl+E. Ctrl+G must open no editor and Ctrl+E must open it, every time.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_317_keymap_loaded_at_start() {
    skip_unless!(common::check_claude_available());
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

/// Scenario: Launch pinned Claude Code 2.1.317 with the Ctrl+G-unbinding keybindings.json present, leave the file untouched, and press Ctrl+G once a minute for ten minutes, then Ctrl+E. Ctrl+G must open no editor at any sample, the file's stat must not change, and Ctrl+E must still open the editor.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_317_keymap_unchanged_ten_minutes() {
    skip_unless!(common::check_claude_available());
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

/// Scenario: Launch pinned Claude Code 2.1.317 with no keybindings.json, confirm Ctrl+G opens the editor, then create the unbinding file and alternate Ctrl+G and Ctrl+E until the binding has moved, then remove the file and alternate again until it has moved back. The time each change took to show is recorded.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_317_keymap_created_and_removed() {
    skip_unless!(common::check_claude_available());
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

/// Scenario: Put the Ctrl+G-unbinding keybindings.json in the project's own .claude directory instead of the HOME, launch pinned Claude Code 2.1.317 there, once with user settings only and once with every settings source as a deck pane launches it, type a draft and press Ctrl+G, then Ctrl+E. The result shows whether the project file is a source of key bindings.
#[spec("inbox/probe/088")]
#[test]
fn probe_088_live_claude_317_project_keymap() {
    skip_unless!(common::check_claude_available());
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

/// Scenario: In pinned Claude Code 2.1.317, put the composer in each of six states (a draft, the slash menu, help, reverse search, shell mode and after Escape twice) and send each of the twelve candidate wire forms of Ctrl+G, whole and split in two writes; then Escape and BEL with 0, 30 and 60 ms between them, and a bracketed paste holding a BEL, whole and split. Whether each opened the editor, and what the next export held after the paste, is recorded.
#[spec("inbox/probe/089")]
#[test]
fn probe_089_live_claude_317_ctrl_g_forms() {
    skip_unless!(common::check_claude_available());
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
    for row in c.rows("paste-bel") {
        eprintln!(
            "PASTE split={} opened={} next_export={:?}",
            row["split"], row["opened"], row["next_export_hex"]
        );
    }
}
