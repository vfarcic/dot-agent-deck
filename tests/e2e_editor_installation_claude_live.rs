#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! What installing the deck's editor helper into a real Claude Code pane
//! changes for the person's own tools (PRD #1666 part 3, Q35-28's lane-2
//! gate `inbox/probe/044`): Claude Code's Bash tool, in the foreground, in
//! the background and in a subagent, sees the person's own `VISUAL` and
//! `EDITOR` exactly, through the tool environment file the deck installs
//! (`CLAUDE_ENV_FILE`), in each state they can be in at spawn; a release the
//! deck hasn't measured the editor probe of is installed all the same and
//! never checked; and a command the rollout doesn't cover isn't installed
//! at all.
//!
//! Real interactive Claude Code (Haiku) in an installed pane
//! (`support/claude_installed.rs`). No e2e editor-probe assumption. The
//! unmeasured-release case runs the executable `DAD_PROBE_044_UNMEASURED_CLAUDE`
//! names, which must be the pinned official 2.1.312 build (its sha256 is
//! checked), and is otherwise `SKIP:`; the `e2e`
//! build's `DOT_AGENT_DECK_E2E_UNRECORD_CLAUDE_RELEASES` can't stand in for
//! one, since it removes a record from the ring's view only, not from the
//! editor probe's permission.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/claude_installed.rs"]
mod installed;

use installed::{BUDGET, CLAUDE, Installed};
use serde_json::{Value, json};
use spec::spec;
use std::time::Duration;

/// Writes `envprobe-<label>.json`: `VISUAL` and `EDITOR` as the process
/// sees them, `null` for unset.
const ENVPROBE: &str = r#"import json, os, sys
with open("envprobe-%s.json" % sys.argv[1], "w") as f:
    json.dump({k: os.environ.get(k) for k in ("VISUAL", "EDITOR")}, f)
"#;

/// The person's editor settings at spawn, as the deck's environment has
/// them: unset, exported empty, or set (with a space and an apostrophe, so
/// the restoring file has to quote them).
#[derive(Clone, Copy)]
enum Saved {
    Unset,
    Empty,
    Set,
}

impl Saved {
    fn env(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Saved::Unset => vec![],
            Saved::Empty => vec![("VISUAL", ""), ("EDITOR", "")],
            Saved::Set => vec![
                ("VISUAL", "t042-visual --wait"),
                ("EDITOR", "t042 editor's choice"),
            ],
        }
    }

    fn expected(self) -> Value {
        match self {
            Saved::Unset => json!({"VISUAL": null, "EDITOR": null}),
            Saved::Empty => json!({"VISUAL": "", "EDITOR": ""}),
            Saved::Set => json!({"VISUAL": "t042-visual --wait", "EDITOR": "t042 editor's choice"}),
        }
    }
}

/// Submit `prompt` as the person, then wait for `file` and for the turn to
/// end. The file's JSON. The prompt is submitted with the harness's
/// `submit_claude_prompt` (revact29 finding 5: an Enter typed straight after
/// the prompt was once left unsubmitted in the box on 2.1.318), which waits
/// for the prompt to render before Enter and confirms Claude Code's
/// `UserPromptSubmit` carried it; its probe is the label `file` names
/// (`envprobe-<label>.json`), which every prompt here passes to the script.
fn tool_run(claude: &Installed, prompt: &str, file: &str) -> Value {
    assert!(
        common::wait_until(BUDGET, || claude.record()["live"]["status"] == "Idle"
            && claude.pane_grid().contains("? for shortcuts")),
        "Claude Code must be ready for the prompt; {}",
        claude.evidence()
    );
    let label = file
        .strip_prefix("envprobe-")
        .and_then(|rest| rest.strip_suffix(".json"))
        .expect("an envprobe-<label>.json file");
    let events = claude.deck.subscribe_events();
    claude
        .deck
        .submit_claude_prompt(&events, &claude.pane.agent, prompt, label);
    let path = claude.deck.workdir().join(file);
    assert!(
        common::wait_until(BUDGET, || std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .is_some()),
        "Haiku must run the probe for {file}; {}",
        claude.evidence()
    );
    assert!(
        common::wait_until(BUDGET, || claude.record()["live"]["status"] == "Idle"),
        "the turn must end; {}",
        claude.evidence()
    );
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

fn tools_see_the_saved_editor(saved: Saved) {
    skip_unless!(installed::checkable());
    let claude = Installed::new(&saved.env());
    claude.assert_installed();
    // What the deck saved for its helper: present only for a set value.
    for (name, var) in [
        (
            "VISUAL",
            dot_agent_deck::composer_probe::install::SAVED_VISUAL_ENV,
        ),
        (
            "EDITOR",
            dot_agent_deck::composer_probe::install::SAVED_EDITOR_ENV,
        ),
    ] {
        assert_eq!(
            claude.env(var).map(Value::from).unwrap_or(Value::Null),
            saved.expected()[name],
            "the deck saved the person's {name} as it was"
        );
    }
    std::fs::write(claude.deck.workdir().join("envprobe.py"), ENVPROBE).unwrap();
    let rows = [
        (
            "foreground",
            "Use the Bash tool to run exactly: python3 envprobe.py fg-044 . Then reply DONE and end your turn.",
            "envprobe-fg-044.json",
        ),
        (
            "background",
            "Use the Bash tool with run_in_background set to true to run exactly: python3 envprobe.py bg-044 . Then reply DONE and end your turn; do not wait for it or read its output.",
            "envprobe-bg-044.json",
        ),
        (
            "subagent",
            "Use your Task (agent) tool to start one general-purpose subagent whose whole prompt is: Use the Bash tool to run exactly: python3 envprobe.py sub-044 . Then reply DONE. -- Do not run that command yourself. When the subagent finishes, reply DONE and end your turn.",
            "envprobe-sub-044.json",
        ),
    ];
    for (row, prompt, file) in rows {
        let seen = tool_run(&claude, prompt, file);
        eprintln!("044 {row}: {seen}");
        assert_eq!(
            seen,
            saved.expected(),
            "{row}: Claude Code's Bash tool sees the person's own editor settings"
        );
    }
    // The subagent row ran in a subagent: Claude Code's own transcript has
    // the probe's command in a sidechain (subagent) entry.
    assert!(
        sidechain_ran(claude.deck.home_dir(), "sub-044"),
        "the subagent row's command must come from a subagent's transcript; {}",
        claude.evidence()
    );
}

/// Whether a transcript under `home`'s Claude Code projects has a sidechain
/// (subagent) entry naming `needle`.
fn sidechain_ran(home: &std::path::Path, needle: &str) -> bool {
    fn walk(dir: &std::path::Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|e| {
            let path = e.path();
            if path.is_dir() {
                walk(&path, needle)
            } else {
                path.extension().is_some_and(|x| x == "jsonl")
                    && std::fs::read_to_string(&path).is_ok_and(|t| {
                        t.lines()
                            .any(|l| l.contains(needle) && l.contains("\"isSidechain\":true"))
                    })
            }
        })
    }
    walk(&home.join(".claude/projects"), needle)
}

/// Scenario: Start real interactive Claude Code in an installed pane with no VISUAL or EDITOR in the person's environment, and ask it to run a small script from its Bash tool in the foreground, in the background, and from a subagent. Each run reports both variables unset, though Claude Code itself runs with the deck's helper as its editor.
#[spec("inbox/probe/044")]
#[test]
fn probe_044_live_tools_see_unset_editor_settings() {
    tools_see_the_saved_editor(Saved::Unset);
}

/// Scenario: As the unset case, with VISUAL and EDITOR exported empty in the person's environment. Each run, foreground, background and subagent, reports both exported and empty.
#[spec("inbox/probe/044")]
#[test]
fn probe_044_live_tools_see_empty_editor_settings() {
    tools_see_the_saved_editor(Saved::Empty);
}

/// Scenario: As the unset case, with VISUAL and EDITOR set to values holding a space and an apostrophe. Each run, foreground, background and subagent, reports exactly those values.
#[spec("inbox/probe/044")]
#[test]
fn probe_044_live_tools_see_set_editor_settings() {
    tools_see_the_saved_editor(Saved::Set);
}

/// The official Claude Code 2.1.312 Linux x64 executable, as its record and
/// the `tests/fixtures/claude-inbox/r5wake/claude-2.1.312-*` fixtures pin it:
/// the `claude` binary in the `@anthropic-ai/claude-code-linux-x64@2.1.312`
/// npm package (fetched with `npm pack` and unpacked). Its record verifies
/// the ring and leaves `editor_probe` absent, so it is the unmeasured release
/// this test runs; if a later activation verifies 2.1.312's editor probe,
/// pin another release here.
const UNMEASURED_CLAUDE_SHA256: &str =
    "4f4178ce41d83e23341ebfbaecdbcf8ea6bf7baea0690025abe49eeec3a5d01d";

/// The Claude Code executable whose release the deck has not measured the
/// editor probe of: `DAD_PROBE_044_UNMEASURED_CLAUDE`, which must be the
/// pinned build [`UNMEASURED_CLAUDE_SHA256`] names. Its release, or why the
/// test cannot run.
fn unmeasured_claude() -> Result<(std::path::PathBuf, String), String> {
    use dot_agent_deck::doorbell_activation::{Measurement, RELEASE_RECORDS};
    use sha2::{Digest, Sha256};
    let path = std::env::var_os("DAD_PROBE_044_UNMEASURED_CLAUDE")
        .map(std::path::PathBuf::from)
        .ok_or("DAD_PROBE_044_UNMEASURED_CLAUDE names no Claude Code executable")?;
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if sha256 != UNMEASURED_CLAUDE_SHA256 {
        return Err(format!(
            "{} has sha256 {sha256}, not the pinned 2.1.312 build's {UNMEASURED_CLAUDE_SHA256}",
            path.display()
        ));
    }
    let out = std::process::Command::new(&path)
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .output()
        .map_err(|e| format!("{} --version: {e}", path.display()))?;
    let release = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let version = semver::Version::parse(&release).map_err(|e| format!("{release:?}: {e}"))?;
    let verified = RELEASE_RECORDS.iter().any(|r| {
        r.agent == dot_agent_deck::event::AgentType::ClaudeCode
            && r.release == version
            && matches!(r.editor_probe, Measurement::Verified(_))
    });
    if verified {
        return Err(format!(
            "Claude Code {release}'s editor probe is verified: not an unmeasured release"
        ));
    }
    Ok((path, release))
}

/// Scenario: Start real interactive Claude Code of a release whose editor probe the deck has not measured, through a plain claude command so the pane is installed, with the person's VISUAL and EDITOR set. The pane is installed all the same and its Bash tool sees the person's settings, but when the person leaves a draft and a task is queued, for twenty seconds the deck asks no check, types nothing and delivers nothing, and the card names the hold.
#[spec("inbox/probe/044")]
#[test]
fn probe_044_live_an_unmeasured_release_is_installed_and_never_checked() {
    // It runs its own binary, so the host's release needs no record.
    skip_unless!(installed::rollout_installs_claude());
    let (binary, release) = match unmeasured_claude() {
        Ok(found) => found,
        Err(why) => {
            skip_unless!(Err(why));
            return;
        }
    };
    eprintln!(
        "unmeasured Claude Code release: {release} at {}",
        binary.display()
    );
    // A `claude` of its own, so the pane's command is the Direct class.
    let bin = common::harness_tempdir().unwrap();
    std::os::unix::fs::symlink(&binary, bin.path().join("claude")).unwrap();
    let command = CLAUDE.replacen(
        "claude",
        &bin.path().join("claude").display().to_string(),
        1,
    );
    let claude = Installed::launch(&command, &Saved::Set.env());
    assert!(
        common::wait_until(BUDGET, || claude.listing()["armed"].as_u64().unwrap_or(0)
            > 0),
        "native SessionStart waiter must arm; {}",
        claude.evidence()
    );
    claude.assert_installed();
    let latched = format!("release={release}");
    assert!(
        common::wait_until(BUDGET, || claude
            .log()
            .lines()
            .any(|l| l.contains("pane release") && l.contains(&latched))),
        "the deck latched {release} from the header; {}",
        claude.evidence()
    );
    assert!(
        common::wait_until(BUDGET, || claude
            .log()
            .lines()
            .any(|l| l.contains("editor probe permission settled"))),
        "the permission settles; {}",
        claude.evidence()
    );
    let settled: Vec<String> = claude
        .log()
        .lines()
        .filter(|l| l.contains("editor probe permission settled"))
        .map(str::to_string)
        .collect();
    assert!(
        settled.iter().all(|l| !l.contains("Armable")),
        "an unmeasured release is never armable: {settled:?}"
    );
    std::fs::write(claude.deck.workdir().join("envprobe.py"), ENVPROBE).unwrap();
    let seen = tool_run(
        &claude,
        "Use the Bash tool to run exactly: python3 envprobe.py unmeasured-044 . Then reply DONE and end your turn.",
        "envprobe-unmeasured-044.json",
    );
    assert_eq!(
        seen,
        Saved::Set.expected(),
        "an unmeasured release's tools still see the person's settings"
    );
    assert!(
        common::wait_until(BUDGET, || claude.listing()["armed"].as_u64().unwrap_or(0)
            > 0),
        "the next native waiter; {}",
        claude.evidence()
    );
    claude.kill_waiter();
    claude.deck.send_keys(b"draft-044");
    claude.expect_composer("draft-044");
    let since = claude.log().len();
    let name = "unmeasured-044.txt";
    let id = claude.post(&claude.directive(name));
    common::wait_until(Duration::from_secs(20), || {
        let message = claude.message(&id);
        assert_eq!(message["state"], "queued", "{message}");
        assert_eq!(message["deliveries"], 0, "{message}");
        let log = claude.log();
        assert!(
            !log[since..].contains("doorbell tick probe")
                && !log[since..].contains("doorbell rang"),
            "no check is asked and nothing rings; {}",
            claude.evidence()
        );
        assert!(
            claude.ops(since).iter().all(|o| !o.deck),
            "the deck types nothing: {:?}",
            claude.ops(since)
        );
        false
    });
    claude.expect_composer("draft-044");
    let hold = claude.record()["inbox"]["wake_hold"].clone();
    eprintln!(
        "044 unmeasured {release}: installed, permission {settled:?}, tools see the person's settings, no check in 20 s, wake_hold={hold}"
    );
    assert!(
        hold.is_string(),
        "the card names why the task waits; {}",
        claude.evidence()
    );
}

/// Scenario: Start real interactive Claude Code through env, a launcher the activation does not install, with the person's VISUAL and EDITOR set. Claude Code runs with the person's own settings, not the deck's helper, and no tool environment file or generation from the deck.
#[spec("inbox/probe/044")]
#[test]
fn probe_044_live_a_launcher_is_not_installed() {
    // Only the rollout matters: a launcher isn't installed at any release.
    skip_unless!(installed::rollout_installs_claude());
    let claude = Installed::launch(&format!("env T042_LAUNCHER=1 {CLAUDE}"), &Saved::Set.env());
    assert_eq!(claude.env("T042_LAUNCHER").as_deref(), Some("1"));
    assert_eq!(
        claude.env("VISUAL").as_deref(),
        Some("t042-visual --wait"),
        "a launcher's Claude Code keeps the person's VISUAL"
    );
    assert_eq!(
        claude.env("EDITOR").as_deref(),
        Some("t042 editor's choice")
    );
    assert_eq!(
        claude.env(dot_agent_deck::composer_probe::install::GENERATION_ENV),
        None
    );
    assert_eq!(claude.env("CLAUDE_ENV_FILE"), None);
    assert!(
        !claude.log().contains("editor helper installed"),
        "nothing was installed; {}",
        claude.evidence()
    );
}
