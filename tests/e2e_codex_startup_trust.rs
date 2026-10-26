#![cfg(all(feature = "e2e", unix))]

//! Issue #1665 — the Codex startup install must not hold `daemon serve`'s
//! socket bind for as long as `codex app-server` takes to answer `hooks/list`.
//!
//! The startup install writes `hooks.json` and then records scoped trust for
//! it, which asks Codex for its hooks. A Codex that never answers held the
//! daemon's pre-bind for the whole 5 s listing bound, on every launch, and the
//! TUI's start as long; on a box with a real `codex` on the login-shell `PATH`
//! that cost every e2e deck about 5 s and turned ordinary load into failed
//! waits (`restore_001`, `dashboard_host_metrics_004`). The install now
//! writes `hooks.json` before the bind and records trust on a thread of its
//! own.
//!
//! These tests start the real binary as `daemon serve`, the way
//! `tests/e2e_daemon_hook_install.rs` does, with a `codex` stand-in first on
//! `PATH`: 014's never answers, 015's answers the way a real Codex does.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use spec::spec;

const DECK_HOOK_EVENTS: &[&str] = &[
    "session_start",
    "user_prompt_submit",
    "pre_tool_use",
    "post_tool_use",
    "permission_request",
    "stop",
    "pre_compact",
    "post_compact",
    "subagent_start",
    "subagent_stop",
    "interrupt",
];

/// The `daemon serve` line that ends the pre-bind work (PRD #1258's timing).
const PRE_BIND_DONE: &str = "daemon serve: pre-bind work done";
/// What `report_startup_trust` logs when the trust write failed, which for a
/// stand-in that never answers is the `hooks/list` timeout.
const TRUST_FAILED: &str = "codex startup install: could not record scoped hook trust";

/// A `codex app-server` stand-in that reads every request and answers none,
/// like the unanswered Codex the issue measured. It reads until its stdin
/// closes, so it exits with whichever side gives up first rather than
/// outliving the test.
const SILENT_CODEX: &str = "#!/bin/sh\nwhile IFS= read -r _request; do :; done\n";

/// Codex's `hooks/list` reply for the deck's installed events, with the
/// `__CODEX_HOME__` and `__DECK_COMMAND__` placeholders the `codex-synthetic`
/// stand-in substitutes, so it echoes exactly what the install wrote.
fn deck_hook_response() -> String {
    let hooks: Vec<Value> = DECK_HOOK_EVENTS
        .iter()
        .enumerate()
        .map(|(index, event_key)| {
            json!({
                "key": format!("__CODEX_HOME__/hooks.json:{event_key}:0:0"),
                "eventName": event_key,
                "handlerType": "command",
                "matcher": null,
                "command": "__DECK_COMMAND__",
                "timeoutSec": 600,
                "statusMessage": null,
                "sourcePath": "__CODEX_HOME__/hooks.json",
                "source": "user",
                "pluginId": null,
                "displayOrder": index,
                "enabled": true,
                "isManaged": false,
                "currentHash": format!("sha256:deck-{index}"),
                "trustStatus": "untrusted"
            })
        })
        .collect();
    json!({
        "id": 2,
        "result": {
            "data": [{ "cwd": "/workspace", "hooks": hooks, "warnings": [], "errors": [] }]
        }
    })
    .to_string()
}

fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, contents).expect("write executable fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make fixture executable");
}

/// One `daemon serve` under an isolated `HOME`, `CODEX_HOME` and sockets, with
/// `codex_dir` first on `PATH`.
struct Daemon {
    child: Child,
    work: PathBuf,
    codex_home: PathBuf,
}

impl Daemon {
    fn start(work: &Path, codex_dir: &Path, extra_env: &[(&str, String)]) -> Self {
        let home = work.join("home");
        let codex_home = work.join("codex-home");
        // The durable copy the install pins, so the host's own
        // `~/.local/bin/dot-agent-deck` cannot be what resolves, and the
        // command it writes is the one the stand-in echoes back.
        let bin_dir = home.join(".local").join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create durable bin dir");
        std::os::unix::fs::symlink(
            common::contained_deck_binary(),
            bin_dir.join("dot-agent-deck"),
        )
        .expect("seed durable deck symlink");
        // The stand-in first, then the system directories its shell needs.
        // `SHELL` is cleared with everything else, so the daemon's login-shell
        // capture (PRD #170) keeps this value and cannot reach a host `codex`.
        let path = format!("{}:/usr/bin:/bin", codex_dir.display());
        let mut command = Command::new(common::contained_deck_binary());
        command
            .args(["daemon", "serve"])
            .current_dir(work)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", path)
            .env("CODEX_HOME", &codex_home)
            .env("DOT_AGENT_DECK_SOCKET", work.join("hook.sock"))
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", work.join("attach.sock"))
            .env("DOT_AGENT_DECK_STATE_DIR", work.join("state"))
            .env("DOT_AGENT_DECK_LOG", work.join("deck.log"))
            .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
            // Never outlive this test, even if it is killed before the teardown.
            .env("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn `daemon serve`");
        Daemon {
            child,
            work: work.to_path_buf(),
            codex_home,
        }
    }

    fn wait_for_bind(&self) -> bool {
        let attach = self.work.join("attach.sock");
        common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
            attach.exists()
        })
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.work.join("deck.log")).unwrap_or_default()
    }

    /// Stop this daemon by its own pid (CLAUDE.md rule 12's teardown bullet).
    fn stop(mut self) {
        let pid = self.child.id() as i32;
        // SAFETY: kill(2) on the pid this test spawned; ESRCH/EPERM are ignored.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        if !common::wait_until(Duration::from_secs(10), || !common::process_running(pid)) {
            // SAFETY: as above.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
    }
}

/// Scenario: Start the real binary as `daemon serve` with a `codex` stand-in
/// first on `PATH` whose `app-server` reads every request and answers none. The
/// daemon must bind its attach socket with `hooks.json` already installed, and
/// its log must say the pre-bind work was done before it says the trust write
/// timed out, which it still says once the 5 s listing bound runs out.
#[spec("codex/hooks/014")]
#[test]
fn codex_hooks_014_an_unanswered_hooks_list_does_not_hold_the_daemon_bind() {
    let dir = common::race_safe_tempdir();
    let work = dir.path();
    let codex_dir = work.join("codex-bin");
    std::fs::create_dir_all(&codex_dir).expect("create stand-in dir");
    write_executable(&codex_dir.join("codex"), SILENT_CODEX);

    let daemon = Daemon::start(work, &codex_dir, &[]);
    let bound = daemon.wait_for_bind();
    let hooks_json_at_bind = daemon.codex_home.join("hooks.json").exists();
    let log_at_bind = daemon.log();
    // The trust write still runs and still gives up: wait past the 5 s bound
    // for the line that says so.
    let failed_later = common::wait_until(common::load_scaled(Duration::from_secs(20)), || {
        daemon.log().contains(TRUST_FAILED)
    });
    let log = daemon.log();
    daemon.stop();

    assert!(
        bound,
        "the daemon never bound its attach socket.\nlog:\n{log}"
    );
    assert!(
        hooks_json_at_bind,
        "the Codex hooks.json must be installed before the daemon binds.\nlog:\n{log}"
    );
    let done = log
        .find(PRE_BIND_DONE)
        .unwrap_or_else(|| panic!("no `{PRE_BIND_DONE}` line.\nlog:\n{log}"));
    assert!(
        !log_at_bind.contains(TRUST_FAILED),
        "the daemon bound only after the Codex trust write gave up on `hooks/list`, so the \
         startup install still waits out the listing bound before the bind (issue #1665).\n\
         log:\n{log}"
    );
    assert!(
        failed_later,
        "the startup trust write never reported its `hooks/list` timeout, so it did not run.\n\
         log:\n{log}"
    );
    let failed = log.find(TRUST_FAILED).expect("checked above");
    assert!(
        done < failed,
        "the trust write's timeout was logged before the pre-bind work ended (issue #1665).\n\
         log:\n{log}"
    );
}

/// Scenario: Start the real binary as `daemon serve` with the `codex-synthetic`
/// stand-in first on `PATH`, answering `hooks/list` with the deck's hooks as
/// the startup install wrote them. Once the daemon has bound, Codex's
/// `config.toml` must come to trust exactly the deck's hook keys, so the trust
/// write that no longer runs before the bind still completes.
#[spec("codex/hooks/015")]
#[test]
fn codex_hooks_015_the_startup_trust_write_still_lands_when_codex_answers() {
    let dir = common::race_safe_tempdir();
    let work = dir.path();
    let codex_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-synthetic");

    let daemon = Daemon::start(
        work,
        &codex_dir,
        &[("CODEX_HOOK_LIST_RESPONSE", deck_hook_response())],
    );
    let bound = daemon.wait_for_bind();
    let config = daemon.codex_home.join("config.toml");
    let mut expected: Vec<String> = DECK_HOOK_EVENTS
        .iter()
        .map(|event_key| format!("{}/hooks.json:{event_key}:0:0", daemon.codex_home.display()))
        .collect();
    expected.sort();
    let trusted_keys = || -> Vec<String> {
        let Ok(contents) = std::fs::read_to_string(&config) else {
            return Vec::new();
        };
        let Ok(root) = toml::from_str::<toml::Value>(&contents) else {
            return Vec::new();
        };
        let mut keys: Vec<String> = root
            .get("hooks")
            .and_then(|hooks| hooks.get("state"))
            .and_then(toml::Value::as_table)
            .map(|state| state.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    };
    let landed = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        trusted_keys() == expected
    });
    let keys = trusted_keys();
    let log = daemon.log();
    daemon.stop();

    assert!(
        bound,
        "the daemon never bound its attach socket.\nlog:\n{log}"
    );
    assert!(
        landed,
        "Codex's config.toml never came to trust exactly the deck's hook keys after the \
         daemon bound.\nexpected: {expected:?}\nfound: {keys:?}\nlog:\n{log}"
    );
}
