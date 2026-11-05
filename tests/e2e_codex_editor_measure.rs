#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! A measurement, not a contract: what a pinned Codex release's
//! external-editor action does, for the `editor_probe` its facts would need
//! (PRD #1666 part 3, U4), how its return orders against the deck's E1F
//! frontier, and M58r (the deck's key-binding resolver against Codex's own
//! behaviour).
//!
//! Each test runs `tests/support/codex_editor_measure.py`, which launches the
//! pinned native Codex (`DOT_AGENT_DECK_CODEX_0_164_0`, else the package the
//! earlier Codex units unpacked under `/var/tmp`) on its own PTY, with no
//! credential: a dummy key and a canned Responses server on 127.0.0.1, inside
//! `bwrap --unshare-net` with `/home`, `/root`, `/tmp` and `/run/user` empty,
//! and the environment started from five names. No prompt is submitted. It
//! then exports a summary with `tests/support/codex_editor_summary.py` and
//! runs the same checks as the fast tier's `tests/codex_editor_evidence.rs`
//! over it. The capture root is printed and kept under `/var/tmp`.
//!
//! It is lane 2 because it runs a real agent binary; it spends no credential.

mod common;

use spec::spec;
use std::path::{Path, PathBuf};

#[path = "support/codex_editor_evidence.rs"]
mod evidence;

const DEFAULT_CODEX: &str =
    "/var/tmp/dad-cxhe-codex-0.164.0/x64/package/vendor/x86_64-unknown-linux-musl/bin/codex";

fn pinned_codex() -> Result<PathBuf, String> {
    let path = std::env::var_os("DOT_AGENT_DECK_CODEX_0_164_0")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CODEX));
    if !path.is_file() {
        return Err(format!(
            "the pinned Codex 0.164.0 is not at {}: set DOT_AGENT_DECK_CODEX_0_164_0 to the native \
             executable of @openai/codex-linux-x64@0.164.0",
            path.display()
        ));
    }
    if !Path::new("/usr/bin/bwrap").is_file() {
        return Err("the recorder's sandbox requires /usr/bin/bwrap".into());
    }
    Ok(path)
}

fn python(script: &str, args: &[&std::ffi::OsStr]) -> std::process::Output {
    let root = env!("CARGO_MANIFEST_DIR");
    std::process::Command::new("/usr/bin/python3")
        .arg("-I")
        .arg(format!("{root}/tests/support/{script}"))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap()
}

/// Records `mode` and exports its summary; the directory the checks read.
fn record(mode: &str) -> Option<PathBuf> {
    let codex = match pinned_codex() {
        Ok(p) => p,
        Err(why) => {
            common::_skip_if_err(Err::<(), _>(why));
            return None;
        }
    };
    let parent = tempfile::Builder::new()
        .prefix("dad-codex-editor-measure-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    let root = parent.join("run");
    let out = python(
        "codex_editor_measure.py",
        &[mode.as_ref(), root.as_os_str(), codex.as_os_str()],
    );
    assert!(
        out.status.success(),
        "recorder {mode} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let summary = parent.join("summary");
    let out = python(
        "codex_editor_summary.py",
        &[root.join("capture").as_os_str(), summary.as_os_str()],
    );
    assert!(
        out.status.success(),
        "summary {mode} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("Codex 0.164.0 {mode} evidence: {}", root.display());
    Some(summary)
}

/// Scenario: Launch the pinned Codex 0.164.0 with a stand-in editor and no credential, and press Ctrl+G on a draft under each editor setting: `VISUAL` five times and with an editor exiting 37, `EDITOR` alone, each set but empty, and neither with `vi`, `nano` and `code` on `PATH`. The export, argument rule, selection, error lines and the `?2004` writes the deck reads must be those `PROPOSED_CODEX_0164_EDITOR_PROBE` states.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_live_codex_164_export_selection_and_paste_toggles() {
    let Some(dir) = record("editor") else { return };
    evidence::check_export_and_selection(&dir);
    let counts = evidence::check_paste_toggles(&dir);
    eprintln!("PASTE-TOGGLES {counts:?}");
}

/// Scenario: Launch the pinned Codex 0.164.0 ten times and press Ctrl+G at the first ready composer, then again until the editor opens: the first press opens nothing and draws nothing, and the editor opens within the next second and a half.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_live_codex_164_ctrl_g_just_after_start() {
    let Some(dir) = record("startup") else { return };
    let (last_miss, first_open) = evidence::check_startup(&dir);
    eprintln!("STARTUP latest miss {last_miss:.2} s, earliest open {first_open:.2} s after ready");
}

/// Scenario: Launch the pinned Codex 0.164.0 seven times on one fresh `CODEX_HOME`, each left 8 s past its first ready composer: the first four each rewrite its `config.toml` after that composer, and the rest leave it as it was.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_live_codex_164_config_writes_at_start() {
    let Some(dir) = record("config-writes") else {
        return;
    };
    let delays = evidence::check_config_writes(&dir);
    eprintln!("CONFIG-WRITES {delays:?} ms after the first ready composer");
}

/// Scenario: In one pinned Codex 0.164.0, send twelve wire forms of Ctrl+G, whole and split, from a draft, the slash popup, reverse search and the shortcuts overlay, then paste `PASTE`, a BEL and `TAIL` four times: Ctrl+G as BEL opens the editor from a draft, and the pasted BEL never reaches the export.
#[spec("inbox/probe/092")]
#[test]
fn probe_092_live_codex_164_ctrl_g_forms_and_pasted_bel() {
    let Some(dir) = record("forms") else { return };
    for (state, opened) in evidence::check_forms(&dir) {
        eprintln!("FORM {state}: {opened:?}");
    }
}

/// Scenario: In the pinned Codex 0.164.0, press Ctrl+G twenty times per arm on the draft `BASE` and type `LATE` at E1's `?2004h`, at the first `?2026h` after it, or while the editor runs, then export the draft again: typed at the deck's E1F it is kept every time, typed during the editor it is lost every time.
#[spec("inbox/probe/093")]
#[test]
fn probe_093_live_codex_164_e1f_release_premise() {
    let Some(dir) = record("e1f") else { return };
    for (arm, kept, n) in evidence::check_e1f(&dir) {
        eprintln!("E1F {arm}: LATE kept {kept}/{n}");
    }
}

/// Scenario: Launch the pinned Codex 0.164.0 once for each of nineteen key-binding source cases (M58's eight, the precedence of config, profile, project and `-c`, a symlinked profile and an untrusted project) and press Ctrl+G and F6 on a draft, then read the same files as the deck reads at spawn: the deck never treats a moved Ctrl+G as the editor, and its merge gives Codex's result wherever Codex started in a trusted folder.
#[spec("inbox/probe/094")]
#[test]
fn probe_094_live_codex_164_keymap_resolver_against_codex() {
    let Some(dir) = record("keymap") else { return };
    let scratch = tempfile::tempdir_in("/var/tmp").unwrap();
    for case in evidence::check_keymap(&dir, scratch.path()) {
        eprintln!(
            "M58R {}: codex {:?}, merge {:?}, deck {:?}",
            case.name, case.codex, case.merge, case.deck
        );
    }
}
