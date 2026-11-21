#![cfg(all(feature = "e2e", unix))]

//! Issue #1973 — a Claude Code agent the daemon starts with no TUI attached
//! starts with the deck's hooks in `~/.claude/settings.json`, even when Claude
//! Code was set up, or the deck copy its hooks named was deleted, after the
//! daemon started; and hooks that already name the deck are not rewritten
//! when an agent starts. `hooks/install/035` is the Antigravity counterpart:
//! an `agy` installed after the daemon started, whose `~/.gemini/config/
//! hooks.json` gets the deck's one named hook before the agent starts.
//!
//! `daemon serve` installs Claude Code's hooks when it starts (issue #1157,
//! `hooks/install/009`). Before an agent starts it runs that install again
//! only when the hooks name no deck or a copy that is gone, because
//! `settings.json` is the user's own file. The OpenCode counterpart, which
//! refreshes the deck-owned plugin before every start, is `hooks/install/023`.

dot_agent_deck_test_harness::link!();

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spec::spec;

const WAIT: Duration = Duration::from_secs(20);

const CLAUDE_SUFFIX: &str = " hook --agent claude-code";

/// What the stand-in records when `settings.json` does not exist.
const ABSENT: &str = "absent";

/// A headless daemon with `phases` scheduled tasks (`phase-1`…) whose command
/// is a stand-in `claude`, and no `~/.claude` when it started, so its startup
/// install wrote nothing.
struct ClaudeDeck {
    daemon: common::DaemonProc,
    spawns: PathBuf,
    settings: PathBuf,
    /// The daemon's tracing log (`DOT_AGENT_DECK_LOG`).
    log: PathBuf,
    // Last, so the daemon (and the stand-ins it reaps) go before their files.
    _scratch: tempfile::TempDir,
}

impl ClaudeDeck {
    fn new(phases: usize) -> Self {
        let tempdir = common::harness_tempdir().expect("scratch tempdir");
        let scratch = tempdir.path().to_path_buf();
        let bin = scratch.join("claude-bin");
        std::fs::create_dir_all(&bin).expect("create the stand-in's bindir");
        let standin = bin.join("claude");
        let spawns = scratch.join("spawns");
        std::fs::create_dir_all(&spawns).expect("create the spawn record dir");
        let toml: String = (1..=phases)
            .map(|n| {
                format!(
                    "[[scheduled_tasks]]\n\
                     name = \"phase-{n}\"\n\
                     cron = \"0 0 1 1 *\"\n\
                     working_dir = \"{work}\"\n\
                     command = \"{command}\"\n\
                     prompt = \"ISSUE-1973-PHASE-{n}\"\n\
                     enabled = true\n",
                    work = scratch.display(),
                    command = standin.display(),
                )
            })
            .collect();
        // The stand-in's directory first on the daemon's PATH, so the
        // `claude --version` the installer asks is answered by the stand-in,
        // never by a real Claude Code installed on the host.
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let log = scratch.join("deck.log");
        let log_str = log.display().to_string();
        let daemon = common::spawn_daemon_serve_with_env(
            Some(&toml),
            "0",
            &[("PATH", &path), ("DOT_AGENT_DECK_LOG", &log_str)],
        );
        let settings = daemon.home.join(".claude").join("settings.json");
        assert!(
            !settings.exists(),
            "precondition: with no ~/.claude, the daemon's startup install writes no settings.json"
        );

        // A stand-in Claude Code that copies what `settings.json` holds the
        // moment it starts (or `absent`), then waits to be reaped with the
        // daemon's process group. It answers `--version` and exits without
        // recording, since the installer asks it which hooks it accepts.
        std::fs::write(
            &standin,
            format!(
                "#!/bin/sh\n\
                 case \"$1\" in --version|-v) echo '2.1.30 (Claude Code)'; exit 0;; esac\n\
                 dir='{spawns}'\n\
                 n=$(ls \"$dir\" | grep -c '^spawn-')\n\
                 if [ -e '{settings}' ]; then cat '{settings}'; else printf '{ABSENT}'; fi > \"$dir/tmp-$$\"\n\
                 mv \"$dir/tmp-$$\" \"$dir/spawn-$n\"\n\
                 exec sleep 600\n",
                spawns = spawns.display(),
                settings = settings.display(),
            ),
        )
        .expect("write the stand-in");
        std::fs::set_permissions(&standin, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in executable");
        Self {
            daemon,
            spawns,
            settings,
            log,
            _scratch: tempdir,
        }
    }

    /// Fire `phase-<n>` and return the `settings.json` its stand-in saw as it
    /// started.
    fn spawn(&self, phase: usize) -> String {
        let before = std::fs::read_dir(&self.spawns)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().starts_with("spawn-"))
                    .count()
            })
            .unwrap_or(0);
        self.daemon
            .run_now(&format!("phase-{phase}"))
            .expect("run the scheduled task now");
        let record = self.spawns.join(format!("spawn-{before}"));
        assert!(
            common::wait_until(WAIT, || record.is_file()),
            "the scheduled Claude Code stand-in for phase-{phase} never started; agents: {:?}",
            self.daemon.agent_records()
        );
        std::fs::read_to_string(&record).expect("read what the stand-in saw")
    }

    /// The deck copy the daemon pins: its durable `~/.local/bin` install.
    fn durable(&self) -> PathBuf {
        self.daemon
            .home
            .join(".local")
            .join("bin")
            .join("dot-agent-deck")
    }
}

/// Every `"command"` string anywhere in a hook document that ends in the
/// Claude writer's suffix.
fn claude_commands(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "command"
                    && let Some(text) = child.as_str()
                    && text.trim_end().ends_with(CLAUDE_SUFFIX)
                {
                    out.push(text.trim_end().to_string());
                }
                claude_commands(child, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|i| claude_commands(i, out)),
        _ => {}
    }
}

/// The binary each deck hook command in `settings` runs.
fn pinned_binaries(settings: &str) -> Vec<String> {
    let doc: serde_json::Value = serde_json::from_str(settings)
        .unwrap_or_else(|e| panic!("settings.json does not parse ({e}):\n{settings}"));
    let mut commands = Vec::new();
    claude_commands(&doc, &mut commands);
    commands
        .iter()
        .map(|command| {
            let word = command
                .strip_suffix(CLAUDE_SUFFIX)
                .expect("filtered on the suffix");
            word.strip_prefix(dot_agent_deck::platform::paths::HOOK_BIN_OVERRIDE_PREFIX)
                .unwrap_or(word)
                .trim_matches('\'')
                .to_string()
        })
        .collect()
}

/// The deck hook command the installer writes for `binary`.
fn deck_command(binary: &Path) -> String {
    format!(
        "{}{}{CLAUDE_SUFFIX}",
        dot_agent_deck::platform::paths::HOOK_BIN_OVERRIDE_PREFIX,
        binary.display()
    )
}

/// What identifies the file at `path` as the same, unrewritten file: its
/// bytes, inode and modification time.
fn file_identity(path: &Path) -> (Vec<u8>, u64, std::time::SystemTime) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    (
        bytes,
        meta.ino(),
        meta.modified().expect("the platform reports mtime"),
    )
}

/// Scenario: Start a headless `daemon serve` in a HOME with no `~/.claude`, so
/// its startup install writes no Claude Code hooks, then create `~/.claude` (as
/// a first run of Claude Code does) and fire a scheduled task whose command is
/// a Claude Code stand-in that records `settings.json` the moment it starts.
/// The agent must start with the deck's hooks already there, pinned to the
/// daemon's install; and a second scheduled start must leave that file exactly
/// as it was, not rewrite it.
#[spec("hooks/install/028")]
#[test]
fn install_028_a_claude_code_agent_set_up_after_the_daemon_starts_with_its_hooks() {
    let deck = ClaudeDeck::new(2);
    std::fs::create_dir_all(deck.settings.parent().unwrap()).expect("set up ~/.claude");

    let seen = deck.spawn(1);
    assert_ne!(
        seen, ABSENT,
        "the agent started with no ~/.claude/settings.json: the daemon did not install \
         Claude Code's hooks before it started (issue #1973)"
    );
    let durable = deck.durable();
    let binaries = pinned_binaries(&seen);
    assert!(
        !binaries.is_empty(),
        "the agent started with no deck hook in settings.json:\n{seen}"
    );
    assert!(
        binaries.iter().all(|b| Path::new(b) == durable),
        "every deck hook must run the daemon's install `{}`; the agent saw {binaries:?}",
        durable.display()
    );

    let installed = file_identity(&deck.settings);
    let again = deck.spawn(2);
    assert_eq!(
        again,
        String::from_utf8_lossy(&installed.0),
        "the second agent must see the hooks the first start installed"
    );
    let after = file_identity(&deck.settings);
    assert!(
        after == installed,
        "starting another Claude Code agent rewrote settings.json (bytes, inode or mtime \
         changed) although its hooks already named the deck; it is the user's file and is \
         repaired only when it needs it"
    );
}

/// Scenario: Start a headless daemon with no `~/.claude`, then give Claude Code
/// a `settings.json` holding a deck hook pinned to a deck copy that no longer
/// exists, a hook of the user's own and an unrelated setting, and fire a
/// scheduled Claude Code stand-in. As it starts, the deck hooks must name the
/// daemon's install and nothing names the deleted copy, while the user's hook
/// and setting are kept.
#[spec("hooks/install/029")]
#[test]
fn install_029_a_claude_code_agent_starts_with_hooks_repaired_from_a_deleted_copy() {
    let deck = ClaudeDeck::new(1);
    let deleted = deck
        .daemon
        .home
        .join("removed-install")
        .join("bin")
        .join("dot-agent-deck");
    let user_hook = "/usr/bin/true issue-1973-users-own-hook";
    let seeded = serde_json::json!({
        "theme": "issue-1973-theme",
        "hooks": {
            "Stop": [
                {"hooks": [{"type": "command", "command": deck_command(&deleted)}]},
                {"hooks": [{"type": "command", "command": user_hook}]},
            ],
        },
    });
    std::fs::create_dir_all(deck.settings.parent().unwrap()).expect("set up ~/.claude");
    std::fs::write(
        &deck.settings,
        serde_json::to_string_pretty(&seeded).unwrap(),
    )
    .expect("seed settings.json");

    let seen = deck.spawn(1);
    let durable = deck.durable();
    let binaries = pinned_binaries(&seen);
    assert!(
        !binaries.is_empty() && binaries.iter().all(|b| Path::new(b) == durable),
        "every deck hook must run the daemon's install `{}` by the time the agent starts, \
         not the deleted copy; the agent saw {binaries:?}:\n{seen}",
        durable.display()
    );
    assert!(
        !seen.contains(&*deleted.to_string_lossy()),
        "the deleted copy is still named:\n{seen}"
    );
    assert!(
        seen.contains(user_hook),
        "the user's own hook must survive the repair:\n{seen}"
    );
    assert!(
        seen.contains("issue-1973-theme"),
        "the user's other settings must survive the repair:\n{seen}"
    );
}

/// Scenario: Start a headless daemon with no `~/.claude`, then give Claude Code
/// a `settings.json` that is not valid JSON and fire a scheduled Claude Code
/// stand-in. The agent must still start, the file must be left byte for byte
/// as the user wrote it, and the deck's log must say the hooks could not be
/// repaired, naming the file it refused, rather than skipping it in silence.
#[spec("hooks/install/030")]
#[test]
fn install_030_a_malformed_claude_settings_file_is_left_alone_and_the_refusal_logged() {
    let deck = ClaudeDeck::new(1);
    let malformed = "{ \"theme\": \"issue-1973\", \"hooks\": ";
    std::fs::create_dir_all(deck.settings.parent().unwrap()).expect("set up ~/.claude");
    std::fs::write(&deck.settings, malformed).expect("seed a malformed settings.json");

    let seen = deck.spawn(1);
    assert_eq!(
        seen, malformed,
        "the agent must start with the user's malformed settings.json exactly as they wrote it"
    );
    assert_eq!(
        std::fs::read_to_string(&deck.settings).expect("read settings.json"),
        malformed,
        "the deck must never write over a settings file it cannot parse"
    );
    let logged = || {
        std::fs::read_to_string(&deck.log)
            .unwrap_or_default()
            .lines()
            .any(|line| {
                line.contains("could not repair the agent's hooks before it started")
                    && line.contains("Claude Code")
            })
    };
    assert!(
        common::wait_until(WAIT, logged),
        "the deck's log must say Claude Code's hooks could not be repaired before it \
         started; log:\n{}",
        std::fs::read_to_string(&deck.log).unwrap_or_default()
    );
    let log = std::fs::read_to_string(&deck.log).unwrap_or_default();
    assert!(
        log.contains(&*deck.settings.to_string_lossy()),
        "the refusal must name the settings file it left alone; log:\n{log}"
    );
}

/// The Antigravity hook command's suffix (`antigravity_hooks_manage`).
const AGY_SUFFIX: &str = " hook --agent antigravity";

/// Scenario: Start a headless `daemon serve` whose `PATH` holds a directory with
/// no `agy` in it, so its startup install writes no Antigravity hook, then put
/// an `agy` stand-in there (Antigravity installed after the deck started) with
/// a `~/.gemini/config/hooks.json` holding only a hook of the user's own, and
/// fire a scheduled task that runs it. The agent must start with the deck's one
/// `dot-agent-deck` hook pinned to the daemon's install and the user's hook kept
/// byte for byte; a second start must not rewrite the file.
#[spec("hooks/install/035")]
#[test]
fn install_035_an_antigravity_agent_set_up_after_the_daemon_starts_with_its_hook() {
    let tempdir = common::harness_tempdir().expect("scratch tempdir");
    let scratch = tempdir.path().to_path_buf();
    let bin = scratch.join("agy-bin");
    std::fs::create_dir_all(&bin).expect("create the stand-in's bindir");
    let standin = bin.join("agy");
    let spawns = scratch.join("spawns");
    std::fs::create_dir_all(&spawns).expect("create the spawn record dir");
    let toml: String = (1..=2)
        .map(|n| {
            format!(
                "[[scheduled_tasks]]\n\
                 name = \"phase-{n}\"\n\
                 cron = \"0 0 1 1 *\"\n\
                 working_dir = \"{work}\"\n\
                 command = \"{command}\"\n\
                 prompt = \"ISSUE-1973-AGY-PHASE-{n}\"\n\
                 enabled = true\n",
                work = scratch.display(),
                command = standin.display(),
            )
        })
        .collect();
    // The stand-in's directory first, and no directory holding a host `agy`,
    // so the startup install finds none and the per-start repair finds only
    // the stand-in.
    let host = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.clone())
            .chain(std::env::split_paths(&host).filter(|dir| !dir.join("agy").exists())),
    )
    .expect("join the daemon's PATH");
    let path = path.to_string_lossy().into_owned();
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &[("PATH", &path)]);
    let hooks = daemon
        .home
        .join(".gemini")
        .join("config")
        .join("hooks.json");
    assert!(
        !daemon.home.join(".gemini").exists(),
        "precondition: with no agy on its PATH, the daemon's startup install creates no ~/.gemini"
    );

    // The user's own hook, with spacing the deck must not normalise.
    let users = "{\n  \"mine\":   {\"PreToolUse\": [ {\"matcher\": \"*\", \"command\": \"true issue-1973-agy-own\"} ]}\n}\n";
    let users_value =
        "{\"PreToolUse\": [ {\"matcher\": \"*\", \"command\": \"true issue-1973-agy-own\"} ]}";
    std::fs::create_dir_all(hooks.parent().unwrap()).expect("set up ~/.gemini/config");
    std::fs::write(&hooks, users).expect("seed the user's hooks.json");
    std::fs::write(
        &standin,
        format!(
            "#!/bin/sh\n\
             case \"$1\" in --version|-v) echo '1.5.9'; exit 0;; esac\n\
             dir='{spawns}'\n\
             n=$(ls \"$dir\" | grep -c '^spawn-')\n\
             cat '{hooks}' > \"$dir/tmp-$$\"\n\
             mv \"$dir/tmp-$$\" \"$dir/spawn-$n\"\n\
             exec sleep 600\n",
            spawns = spawns.display(),
            hooks = hooks.display(),
        ),
    )
    .expect("write the stand-in");
    std::fs::set_permissions(&standin, std::fs::Permissions::from_mode(0o755))
        .expect("make the stand-in executable");

    let spawn = |phase: usize| {
        daemon
            .run_now(&format!("phase-{phase}"))
            .expect("run the scheduled task now");
        let record = spawns.join(format!("spawn-{}", phase - 1));
        assert!(
            common::wait_until(WAIT, || record.is_file()),
            "the scheduled agy stand-in for phase-{phase} never started; agents: {:?}",
            daemon.agent_records()
        );
        std::fs::read_to_string(&record).expect("read what the stand-in saw")
    };

    let seen = spawn(1);
    let doc: serde_json::Value = serde_json::from_str(&seen)
        .unwrap_or_else(|e| panic!("hooks.json does not parse ({e}):\n{seen}"));
    let ours = doc.get("dot-agent-deck").unwrap_or_else(|| {
        panic!(
            "the agent started with no deck hook in hooks.json: the daemon did not install \
             Antigravity's hook before it started (issue #1973):\n{seen}"
        )
    });
    let mut commands = Vec::new();
    fn collect(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => map.iter().for_each(|(key, child)| {
                if key == "command"
                    && let Some(text) = child.as_str()
                {
                    out.push(text.to_string());
                }
                collect(child, out);
            }),
            serde_json::Value::Array(items) => items.iter().for_each(|i| collect(i, out)),
            _ => {}
        }
    }
    collect(ours, &mut commands);
    let durable = daemon
        .home
        .join(".local")
        .join("bin")
        .join("dot-agent-deck");
    assert!(
        !commands.is_empty()
            && commands.iter().all(|c| {
                c.strip_suffix(AGY_SUFFIX)
                    .map(|word| {
                        word.strip_prefix(dot_agent_deck::platform::paths::HOOK_BIN_OVERRIDE_PREFIX)
                            .unwrap_or(word)
                            .trim_matches('\'')
                            == durable.to_string_lossy()
                    })
                    .unwrap_or(false)
            }),
        "every deck hook must run the daemon's install `{}`; the agent saw {commands:?}",
        durable.display()
    );
    assert!(
        seen.contains(users_value),
        "the user's own hook must be kept byte for byte:\n{seen}"
    );

    let installed = file_identity(&hooks);
    let again = spawn(2);
    assert_eq!(
        again,
        String::from_utf8_lossy(&installed.0),
        "the second agent must see the hook the first start installed"
    );
    assert!(
        file_identity(&hooks) == installed,
        "starting another Antigravity agent rewrote hooks.json (bytes, inode or mtime \
         changed) although its hook already named the deck"
    );
}
