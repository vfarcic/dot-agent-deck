//! Claude's two managed hook handlers, exercised through isolated settings.
//! CLI install is intentional: the real version probe chooses the waiter gate.
#![cfg(unix)]

use serde_json::{Value, json};
use spec::spec;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_temp.rs"]
mod test_temp;

const STATUS: &str = " hook --agent claude-code";
const WAITER: &str = " inbox wait --agent claude-code";
const USER: &str = "printf user-hook-kept";
// The measured supported release, not a declaration of the minimum version.
const SUPPORTED: &str = "2.1.296 (Claude Code)";
// The wake wording measured by the PRD #1666 framing spike, pinned verbatim.
const REWAKE_MESSAGE: &str = "dot-agent-deck message, delivered by the deck the user runs you in (this is not a hook error):";
const REWAKE_SUMMARY: &str = "✉ dot-agent-deck message";

struct Settings {
    _dir: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    deck: PathBuf,
    path: PathBuf,
}

impl Settings {
    fn new(version: &str) -> Self {
        let dir = test_temp::tempdir().unwrap();
        let home = dir.path().join("home with spaces");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let deck = bin.join("dot-agent-deck");
        if std::fs::hard_link(env!("CARGO_BIN_EXE_dot-agent-deck"), &deck).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_dot-agent-deck"), &deck).unwrap();
        }
        let claude = bin.join("claude");
        std::fs::write(&claude, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = home.join(".claude/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        Self {
            _dir: dir,
            home,
            bin,
            deck,
            path,
        }
    }

    fn install(&self) {
        let result = Command::new(&self.deck)
            .args(["hooks", "install", "--agent", "claude-code"])
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.bin)
            .current_dir(&self.home)
            .output()
            .unwrap();
        assert!(result.status.success(), "isolated hook install: {result:?}");
    }

    fn read(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.path).unwrap()).unwrap()
    }

    fn seed_duplicates(&self) {
        let command = |suffix| format!("'{}'{}", self.deck.display(), suffix);
        let rules = json!([
            {"hooks":[{"type":"command","command":command(STATUS)},
                      {"type":"command","command":command(WAITER),"asyncRewake":true,"timeout":86400},
                      {"type":"command","command":USER}]},
            {"hooks":[{"type":"command","command":command(WAITER),"asyncRewake":true,"timeout":86400,
                       "rewakeMessage":REWAKE_MESSAGE,"rewakeSummary":REWAKE_SUMMARY}]}
        ]);
        std::fs::write(
            &self.path,
            serde_json::to_vec(&json!({
                "model":"user-model", "permissions":{"allow":["Read"]},
                "hooks":{"SessionStart":rules,"Stop":rules}
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

fn handlers<'a>(settings: &'a Value, event: &str) -> Vec<&'a Value> {
    settings["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|rule| rule["hooks"].as_array().into_iter().flatten())
        .collect()
}

fn count(settings: &Value, event: &str, suffix: &str) -> usize {
    handlers(settings, event)
        .iter()
        .filter(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| command.ends_with(suffix))
        })
        .count()
}

/// Scenario: Install Claude hooks in an isolated HOME whose Claude version is a measured supported release. SessionStart and Stop must each carry one status handler plus one asynchronous, day-long waiter that names its wake as a dot-agent-deck message, while other events carry no waiter.
#[spec("inbox/claude/001")]
#[test]
fn claude_001_supported_install_writes_status_and_day_long_waiter() {
    let fixture = Settings::new(SUPPORTED);
    fixture.install();
    let settings = fixture.read();
    for event in ["SessionStart", "Stop"] {
        assert_eq!(
            count(&settings, event, STATUS),
            1,
            "status handler for {event}"
        );
        assert_eq!(
            count(&settings, event, WAITER),
            1,
            "supported Claude needs exactly one native waiter for {event}: {settings}"
        );
        let hooks = handlers(&settings, event);
        let waiter = hooks
            .iter()
            .find(|hook| hook["command"].as_str().unwrap().ends_with(WAITER))
            .unwrap();
        assert_eq!(waiter["type"], "command");
        assert_eq!(waiter["asyncRewake"], true);
        assert_eq!(waiter["timeout"], 86400);
        assert_eq!(waiter["rewakeMessage"], REWAKE_MESSAGE, "{settings}");
        assert_eq!(waiter["rewakeSummary"], REWAKE_SUMMARY, "{settings}");
        let output = Command::new("/bin/sh")
            .args(["-c", waiter["command"].as_str().unwrap()])
            .env_clear()
            .env("HOME", &fixture.home)
            .env("PATH", &fixture.bin)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "installed waiter command must be shell-safe and inert outside a deck: {output:?}"
        );
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
    for event in settings["hooks"].as_object().unwrap().keys() {
        if !["SessionStart", "Stop"].contains(&event.as_str()) {
            assert_eq!(count(&settings, event, WAITER), 0, "no waiter on {event}");
        }
    }
}

/// Scenario: Seed stale native waiters beside a user's hooks and install with an old or unknown Claude version. Status hooks remain, unsafe waiter entries disappear, and user settings and commands survive.
#[spec("inbox/claude/002")]
#[test]
fn claude_002_old_or_unknown_version_removes_stale_waiters() {
    for version in ["2.0.0 (Claude Code)", "unparseable"] {
        let fixture = Settings::new(version);
        fixture.seed_duplicates();
        fixture.install();
        let settings = fixture.read();
        assert_eq!(settings["model"], "user-model");
        assert_eq!(settings["permissions"]["allow"], json!(["Read"]));
        for event in ["SessionStart", "Stop"] {
            assert_eq!(count(&settings, event, STATUS), 1);
            assert_eq!(
                count(&settings, event, WAITER),
                0,
                "unsupported Claude must not retain asyncRewake waiters: {settings}"
            );
            assert_eq!(count(&settings, event, USER), 1);
        }
    }
}

/// Scenario: Seed duplicate waiter and status entries co-located with a user command, one waiter written before the wake wording existed, then install three times. Each managed event must keep exactly one of each deck handler and one user command without changing unrelated settings, and the kept waiter gains the wake wording.
#[spec("inbox/claude/003")]
#[test]
fn claude_003_repeated_install_consolidates_both_handler_kinds() {
    let fixture = Settings::new(SUPPORTED);
    fixture.seed_duplicates();
    for _ in 0..3 {
        fixture.install();
    }
    let settings = fixture.read();
    assert_eq!(settings["model"], "user-model");
    for event in ["SessionStart", "Stop"] {
        assert_eq!(count(&settings, event, STATUS), 1);
        assert_eq!(
            count(&settings, event, WAITER),
            1,
            "consolidation must retain one waiter, not one of every command suffix: {settings}"
        );
        assert_eq!(count(&settings, event, USER), 1);
        let hooks = handlers(&settings, event);
        let waiter = hooks
            .iter()
            .find(|hook| hook["command"].as_str().unwrap().ends_with(WAITER))
            .unwrap();
        assert_eq!(
            (&waiter["rewakeMessage"], &waiter["rewakeSummary"]),
            (&json!(REWAKE_MESSAGE), &json!(REWAKE_SUMMARY)),
            "a kept waiter must gain the wake wording: {settings}"
        );
    }
}

/// Scenario: Uninstall from settings carrying both deck handler kinds in mixed rules, with waiters both with and without the wake wording. All deck status and waiter commands must be removed while the user's co-located commands, model and permissions remain.
#[spec("inbox/claude/004")]
#[test]
fn claude_004_uninstall_removes_waiters_and_preserves_user_hooks() {
    let fixture = Settings::new(SUPPORTED);
    fixture.seed_duplicates();
    dot_agent_deck::hooks_manage::uninstall_from(Path::new(&fixture.path)).unwrap();
    let settings = fixture.read();
    assert_eq!(settings["model"], "user-model");
    assert_eq!(settings["permissions"]["allow"], json!(["Read"]));
    for event in ["SessionStart", "Stop"] {
        assert_eq!(count(&settings, event, STATUS), 0);
        assert_eq!(
            count(&settings, event, WAITER),
            0,
            "uninstall must remove the inbox-wait handler too: {settings}"
        );
        assert_eq!(count(&settings, event, USER), 1);
    }
}
