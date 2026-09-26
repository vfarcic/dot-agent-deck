#![cfg(all(feature = "e2e", unix))]

//! Issue #1157 — a headless `daemon serve`, which is all the packaged desktop
//! app ever starts, must install Claude Code's hooks and the OpenCode plugin
//! the way a TUI launch does.
//!
//! The desktop resolves its bundled sidecar (`tauri.bundle.conf.json`'s
//! `externalBin`) as a sibling of its own executable and spawns it as `daemon
//! serve` by absolute path (`daemon_bridge::resolve_daemon_executable`, then
//! `spawn_daemon_serve_detached_with_exe`). Until this issue that subcommand ran
//! only the Codex and Devin installers; Claude Code and OpenCode were installed
//! from `run_tui_session` alone, so a desktop-only user — no CLI deck, so no TUI
//! ever launched — got no Claude Code hooks at all. Measured against the
//! v0.42.0 `.deb`'s sidecar in a scratch `HOME`: `~/.claude/` present,
//! `~/.claude/settings.json` never written.
//!
//! **Why a hard link at a bundle-shaped path.** The sidecar of a desktop-only
//! install is neither in `~/.local/bin` nor on the daemon's `PATH`, so it
//! reaches `durable_binary_path`'s step 3 and is pinned as the only deck there
//! is. The fixture reproduces exactly that: the freshly built binary linked to
//! `…/Agent Deck.app/Contents/MacOS/dot-agent-deck` (the v0.42.0 `.dmg`'s
//! layout, space included), an empty `PATH`, and no install. The space is not
//! decoration — a `/bin/sh` hook command naming that path unquoted would split
//! into two words, so it also proves the Claude writer's quoting on the daemon
//! route.
//!
//! Unix only for the reason `tests/scratch_binary_hook_install.rs` records:
//! `env_clear` plus `HOME` isolates the child here and not on Windows.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use spec::spec;

const CLAUDE_SUFFIX: &str = " hook --agent claude-code";

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

/// The freshly built deck at a path shaped like the macOS desktop's bundled
/// sidecar: a hard link where the filesystem allows one, a copy otherwise.
fn bundled_sidecar(root: &Path) -> PathBuf {
    let dir = root
        .join("Applications")
        .join("Agent Deck.app")
        .join("Contents")
        .join("MacOS");
    std::fs::create_dir_all(&dir).expect("create the bundle's MacOS dir");
    let sidecar = dir.join("dot-agent-deck");
    let built = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    if std::fs::hard_link(built, &sidecar).is_err() {
        std::fs::copy(built, &sidecar).expect("copy the deck binary to the sidecar path");
    }
    sidecar
}

/// Scenario: Link the freshly built deck to a desktop-bundle-shaped path
/// (`…/Agent Deck.app/Contents/MacOS/dot-agent-deck`), seed `~/.claude/` and
/// `~/.opencode/` in an isolated `HOME` with no CLI install anywhere, and start
/// that binary as `daemon serve` by absolute path — exactly how the packaged
/// desktop starts its sidecar, with no TUI involved. Once the daemon binds its
/// attach socket, `~/.claude/settings.json` must carry deck hook commands and
/// the OpenCode plugin must exist, and both must name the sidecar itself.
#[spec("hooks/install/009")]
#[test]
fn install_009_a_headless_daemon_installs_claude_hooks_and_the_opencode_plugin() {
    let dir = common::race_safe_tempdir();
    let work = dir.path();
    let home = work.join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("seed ~/.claude");
    std::fs::create_dir_all(home.join(".opencode")).expect("seed ~/.opencode");
    // No `dot-agent-deck` on the child's PATH, so the host's own install can
    // never be what resolves; `SHELL` is cleared with everything else, so the
    // daemon's login-shell PATH capture (PRD #170) keeps this value.
    let empty_bin = work.join("emptybin");
    std::fs::create_dir_all(&empty_bin).expect("create empty bindir");
    let sidecar = bundled_sidecar(work);
    let attach_socket = work.join("attach.sock");

    let mut daemon = std::process::Command::new(&sidecar)
        .args(["daemon", "serve"])
        .current_dir(work)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", &empty_bin)
        .env("DOT_AGENT_DECK_SOCKET", work.join("hook.sock"))
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &attach_socket)
        .env("DOT_AGENT_DECK_STATE_DIR", work.join("state"))
        .env("DOT_AGENT_DECK_LOG", work.join("deck.log"))
        .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
        // Never outlive this test, even if it is killed before the teardown.
        .env("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the sidecar as `daemon serve`");
    let daemon_pid = daemon.id() as i32;

    // The installers run before `run_daemon_serve_cli` binds anything, so once
    // the attach socket exists their writes are complete.
    let bound = common::wait_until(Duration::from_secs(15), || attach_socket.exists());

    let settings = home.join(".claude").join("settings.json");
    let plugin = home
        .join(".opencode")
        .join("plugin")
        .join("dot-agent-deck.js");
    let settings_body = std::fs::read_to_string(&settings);
    let plugin_body = std::fs::read_to_string(&plugin);

    // Teardown by this daemon's own pid before asserting, so a failed assertion
    // cannot leak it (CLAUDE.md rule 12's teardown bullet).
    // SAFETY: kill(2) on the pid this test spawned; ESRCH/EPERM are ignored.
    unsafe {
        libc::kill(daemon_pid, libc::SIGTERM);
    }
    if !common::wait_until(Duration::from_secs(10), || {
        !common::process_running(daemon_pid)
    }) {
        // SAFETY: as above.
        unsafe {
            libc::kill(daemon_pid, libc::SIGKILL);
        }
    }
    let _ = daemon.wait();
    let log = std::fs::read_to_string(work.join("deck.log")).unwrap_or_default();

    assert!(
        bound,
        "the daemon never bound its attach socket.\nlog:\n{log}"
    );

    let settings_body = settings_body.unwrap_or_else(|e| {
        panic!(
            "a headless `daemon serve` wrote no ~/.claude/settings.json ({e}) — a desktop-only \
             user gets no Claude Code hooks (issue #1157).\nlog:\n{log}"
        )
    });
    let doc: serde_json::Value =
        serde_json::from_str(&settings_body).expect("settings.json parses as JSON");
    let mut commands = Vec::new();
    claude_commands(&doc, &mut commands);
    assert!(
        !commands.is_empty(),
        "settings.json carries no deck hook command:\n{settings_body}"
    );
    let sidecar_str = sidecar.to_str().expect("sidecar path is UTF-8");
    for command in &commands {
        let binary = command
            .strip_suffix(CLAUDE_SUFFIX)
            .expect("filtered on the suffix");
        assert!(
            binary.starts_with('\'') && binary.ends_with('\''),
            "`{command}` names a path with a space unquoted; /bin/sh would split it"
        );
        assert_eq!(
            binary.trim_matches('\''),
            sidecar_str,
            "the hook command must name the sidecar, the only deck on this machine"
        );
    }

    let plugin_body = plugin_body.unwrap_or_else(|e| {
        panic!(
            "a headless `daemon serve` wrote no OpenCode plugin at {} ({e}) — a desktop-only \
             user gets no OpenCode status (issue #1157).\nlog:\n{log}",
            plugin.display()
        )
    });
    let expected = format!(
        "const BINARY_PATH = {};",
        serde_json::to_string(sidecar_str).expect("encode the sidecar path")
    );
    assert!(
        plugin_body.contains(&expected),
        "the OpenCode plugin does not pin the sidecar (`{expected}`):\n{plugin_body}"
    );
}
