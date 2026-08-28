//! PRD #381 — hook installation must resolve a **durable** binary path before
//! writing it into another program's persistent, user-level configuration.
//!
//! The field defect: `hooks install` (and, silently, dashboard startup) wrote
//! `std::env::current_exe()` into `~/.claude/settings.json`, `~/.codex/hooks.json`
//! and the OpenCode plugin. From a local build that is a `target/debug` or
//! `target/release` path — gitignored, removed by `cargo clean`, and gone the
//! moment its worktree is pruned — so every hook then failed with
//! `/bin/sh: 1: /…/target/release/dot-agent-deck: not found`.
//!
//! The reason no test caught it is structural, and closing it is the PRD's
//! highest-value milestone: `hooks_manage::auto_install_to` — the seam the
//! existing hook-install tests drive — hardcoded
//! `let binary_path = "dot-agent-deck".to_string();`, so **the derivation that
//! produced the bad value was never executed by a test**. That seam now takes
//! the resolver, and every test here drives it with a
//! `…/target/release/dot-agent-deck` `current_exe()` of its own choosing and
//! asserts that value never reaches the file.
//!
//! Conventions follow `hook_rule_identification.rs`: the public
//! explicit-path seams against a `tempfile` fixture, no `$HOME` manipulation,
//! no spawned processes. These are lib units, NOT `#[spec]` catalog entries —
//! the catalog entries for this PRD are the two L2 tests in
//! `e2e_hook_binary_path.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dot_agent_deck::platform::paths::durable_binary_path_with;
use serde_json::{Value, json};
use tracing_subscriber::fmt::MakeWriter;

#[path = "../src/test_temp.rs"]
mod test_temp;

/// Claude's deck-owned command signature (`hooks_manage::HOOK_COMMAND_SUFFIX`).
const CLAUDE_SUFFIX: &str = "hook --agent claude-code";

/// The Codex equivalent (`codex_hooks_manage::HOOK_COMMAND_SUFFIX`).
const CODEX_SUFFIX: &str = "hook --agent codex";

/// A scratch tree with the three inputs the resolver takes, wired so a test can
/// say "the running binary is a build artifact" without being one.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: test_temp::tempdir().expect("resolver fixture tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The isolated home the resolver's `~/.local/bin` candidate hangs off.
    fn home(&self) -> PathBuf {
        let home = self.path().join("home");
        std::fs::create_dir_all(&home).expect("create fixture home");
        home
    }

    /// A `…/target/release/dot-agent-deck` that really exists — the exact input
    /// the field defect wrote into global config.
    fn build_artifact(&self) -> PathBuf {
        let artifact = self
            .path()
            .join("checkout")
            .join("target")
            .join("release")
            .join("dot-agent-deck");
        write_executable(&artifact);
        artifact
    }

    /// The durable candidate at `<home>/.local/bin/dot-agent-deck` (resolution
    /// order step 2a).
    fn durable(&self) -> PathBuf {
        let durable = self
            .home()
            .join(".local")
            .join("bin")
            .join("dot-agent-deck");
        write_executable(&durable);
        durable
    }

    /// An empty settings.json path inside the fixture (its parent exists, which
    /// is what `auto_install_to`'s guard requires).
    fn settings(&self) -> PathBuf {
        let dir = self.path().join("claude");
        std::fs::create_dir_all(&dir).expect("create claude dir");
        dir.join("settings.json")
    }

    /// Drive `hooks_manage::auto_install_to` — the PRD #381 M3 seam — and
    /// return the log lines it produced *for this fixture*.
    ///
    /// Every `auto_install_to` call in this file goes through here rather than
    /// calling it directly, which is what guarantees the shared subscriber is
    /// installed before any of those callsites is first reached. See
    /// [`log_buffer`] for why that ordering matters.
    fn auto_install(
        &self,
        settings: &Path,
        resolve: impl FnOnce() -> Result<String, String>,
    ) -> String {
        let _ = log_buffer();
        dot_agent_deck::hooks_manage::auto_install_to(settings, resolve);
        logs_mentioning(self.path().to_str().expect("fixture path is UTF-8"))
    }
}

/// A real executable file at `path`, parents created — the resolver's step-2a
/// gate is "exists and is executable", so the exec bit is load-bearing.
fn write_executable(path: &Path) {
    std::fs::create_dir_all(path.parent().expect("path has a parent")).expect("create parent");
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Every `"command"` string anywhere in a hook document. Walks the whole tree
/// rather than the documented nesting, so a path smuggled into a
/// differently-shaped rule is still caught.
fn collect_commands(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "command"
                    && let Some(text) = child.as_str()
                {
                    out.push(text.to_string());
                }
                collect_commands(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_commands(item, out);
            }
        }
        _ => {}
    }
}

fn commands_in(path: &Path) -> Vec<String> {
    let body =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let doc: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("parse {} as JSON: {e}\n{body}", path.display()));
    let mut out = Vec::new();
    collect_commands(&doc, &mut out);
    out
}

/// The deck-owned commands in `path` — those ending with `suffix`, which is how
/// both writers identify their own rules.
fn deck_commands(path: &Path, suffix: &str) -> Vec<String> {
    let mut commands = commands_in(path);
    commands.retain(|command| command.trim_end().ends_with(suffix));
    commands
}

/// The invariant this whole PRD exists for, asserted on the raw bytes so it
/// cannot be satisfied by a rule shape the walker above does not know.
fn assert_no_build_artifact(path: &Path) {
    let body = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read: {e}"));
    for marker in ["target/release", "target/debug"] {
        assert!(
            !body.contains(marker),
            "{} contains `{marker}` — gitignored, removed by `cargo clean`, and gone when its \
             worktree is pruned, so it must never be written into persistent user config:\n{body}",
            path.display()
        );
    }
}

/// A `MakeWriter` over a shared in-memory buffer, so a test can read back what
/// the code under test logged. Same shape as `logging_filter.rs`'s, and
/// hand-rolled for the same reason (`tracing-subscriber`'s blanket impls do not
/// cover a shareable `Mutex<Vec<u8>>`).
#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture buffer poisoned").write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The process-wide log buffer, installed as the **global** tracing subscriber
/// the first time any test here drives an installer.
///
/// A global subscriber rather than a per-test `with_default`, and the
/// difference is not stylistic. `tracing` caches each callsite's interest
/// process-wide the first time that callsite is hit, so under `cargo test`'s
/// thread parallelism a callsite first reached from a thread that has no
/// subscriber can be cached as "never" and then stay silent for every later
/// thread. That is not hypothetical here: it made the self-heal assertion below
/// pass in isolation and fail in the full run, depending purely on which test
/// won the race. One subscriber, installed before any installer can run,
/// removes the race — and because every fixture lives in its own tempdir, a
/// test can still pick its own lines out of the shared buffer unambiguously.
fn log_buffer() -> &'static Arc<Mutex<Vec<u8>>> {
    static BUFFER: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    BUFFER.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(CaptureWriter(Arc::clone(&buf)))
            .with_ansi(false)
            .without_time()
            .finish();
        // Another integration test in this binary could in principle have set
        // one already; losing that race is not a reason to fail, only to read
        // whatever the winner captured.
        let _ = tracing::subscriber::set_global_default(subscriber);
        buf
    })
}

/// Everything currently in the shared buffer whose line mentions `needle` — a
/// fixture's own tempdir path, which is unique per test.
fn logs_mentioning(needle: &str) -> String {
    let bytes = log_buffer().lock().expect("log buffer poisoned").clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| line.contains(needle))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The regression guard the PRD calls its highest-value milestone.
// ---------------------------------------------------------------------------

/// The flagship: the Claude auto-install seam, driven by the REAL resolver with
/// a `…/target/release/dot-agent-deck` `current_exe()`. That value must not
/// appear anywhere in the written settings — the durable candidate must.
#[test]
fn claude_auto_install_never_writes_the_build_artifact_it_is_running_from() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let durable = fixture.durable();
    let settings = fixture.settings();

    fixture.auto_install(&settings, || {
        durable_binary_path_with(Ok(artifact.clone()), &home, None)
    });

    assert_no_build_artifact(&settings);
    let commands = deck_commands(&settings, CLAUDE_SUFFIX);
    assert_eq!(
        commands.len(),
        10,
        "one deck rule per hook type: {commands:?}"
    );
    let expected = format!("{} {CLAUDE_SUFFIX}", durable.display());
    for command in &commands {
        assert_eq!(command, &expected);
    }
}

/// The same property for Codex's `hooks.json`, which is a separate writer with
/// its own document shape.
#[test]
fn codex_install_never_writes_the_build_artifact_it_is_running_from() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let durable = fixture.durable();
    let codex_home = fixture.path().join("codex");

    let resolved = durable_binary_path_with(Ok(artifact.clone()), &home, None)
        .expect("a seeded ~/.local/bin candidate must resolve");
    dot_agent_deck::codex_hooks_manage::install_to(&codex_home, &resolved)
        .expect("install Codex hooks.json");

    let hooks = codex_home.join("hooks.json");
    assert_no_build_artifact(&hooks);
    let commands = deck_commands(&hooks, CODEX_SUFFIX);
    assert!(!commands.is_empty(), "no deck-owned Codex rule was written");
    let expected = format!("{} {CODEX_SUFFIX}", durable.display());
    for command in &commands {
        assert_eq!(command, &expected);
    }
}

// ---------------------------------------------------------------------------
// M4 — self-heal, and the three properties that keep it safe.
// ---------------------------------------------------------------------------

/// A deck-owned rule whose binary is positively gone is repaired on the auto
/// path, and the repair is LOGGED — silently mutating global config is the same
/// class of thing that caused this bug.
///
/// The dead rule sits beside an already-current one, which is the case that
/// used to be dropped on the floor: every hook type reports "already
/// installed", so the pass installed nothing and returned before writing the
/// prune out.
#[test]
fn self_heal_rewrites_a_deck_rule_whose_binary_is_gone_and_says_so() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let durable = fixture.durable();
    let settings = fixture.settings();
    let dead = fixture
        .path()
        .join("pruned-worktree")
        .join("dot-agent-deck");
    assert!(!dead.exists(), "the dead path must genuinely not exist");

    let current = format!("{} {CLAUDE_SUFFIX}", durable.display());
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": format!("{} {CLAUDE_SUFFIX}", dead.display()) } ] },
                    { "hooks": [ { "type": "command", "command": current } ] },
                ]
            }
        }))
        .expect("serialize fixture"),
    )
    .expect("seed settings.json");

    let logs = fixture.auto_install(&settings, || {
        durable_binary_path_with(Ok(artifact.clone()), &home, None)
    });

    let body = std::fs::read_to_string(&settings).expect("read settings");
    assert!(
        !body.contains("pruned-worktree"),
        "the dead deck rule survived the repair:\n{body}"
    );
    assert!(
        logs.contains("repaired") && logs.contains("stale dot-agent-deck hook command"),
        "the repair was not logged:\n{logs}"
    );
}

/// The safety property that makes self-heal tolerable: a deck rule whose path
/// EXISTS but differs from what would be written is left alone. PRD #381 Open
/// Question 3 — the trigger is "the target is missing", never "the target is
/// not what I would have written", because the second reading is what would let
/// a startup silently repoint a developer's or a user's deliberate choice.
#[test]
fn self_heal_leaves_a_different_but_still_valid_deck_path_alone() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let durable = fixture.durable();
    let settings = fixture.settings();
    let other = fixture.path().join("other-install").join("dot-agent-deck");
    write_executable(&other);

    let other_command = format!("{} {CLAUDE_SUFFIX}", other.display());
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": other_command.clone() } ] }
                ]
            }
        }))
        .expect("serialize fixture"),
    )
    .expect("seed settings.json");

    fixture.auto_install(&settings, || {
        durable_binary_path_with(Ok(artifact.clone()), &home, None)
    });

    let commands = deck_commands(&settings, CLAUDE_SUFFIX);
    assert!(
        commands.contains(&other_command),
        "an existing, still-valid deck path was rewritten: {commands:?}"
    );
    assert!(
        commands.contains(&format!("{} {CLAUDE_SUFFIX}", durable.display())),
        "the durable rule was not added alongside it: {commands:?}"
    );
}

/// A user-authored command that merely MENTIONS `dot-agent-deck` is never
/// rewritten or deleted. Deck ownership is decided by the exact
/// `hook --agent claude-code` signature, not by a substring, and this pins that
/// the repair pass does not widen it.
#[test]
fn self_heal_never_touches_a_user_command_that_merely_mentions_the_deck() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let settings = fixture.settings();
    let _durable = fixture.durable();
    // Deliberately a path that does not exist: even so, this is not ours.
    let user_command = "/opt/audit/wrapper --watch dot-agent-deck --report";

    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "Stop": [
                    { "matcher": "mine", "hooks": [ { "type": "command", "command": user_command } ] }
                ]
            }
        }))
        .expect("serialize fixture"),
    )
    .expect("seed settings.json");

    fixture.auto_install(&settings, || {
        durable_binary_path_with(Ok(artifact.clone()), &home, None)
    });

    let commands = commands_in(&settings);
    assert!(
        commands.iter().any(|c| c == user_command),
        "a user-authored command mentioning the deck was removed: {commands:?}"
    );
    let body = std::fs::read_to_string(&settings).expect("read settings");
    assert!(
        body.contains("\"matcher\": \"mine\""),
        "the user's own matcher was dropped:\n{body}"
    );
}

/// Repair is idempotent: a second pass over the repaired file leaves it
/// byte-identical. Without this, a "self-heal" could churn global config on
/// every dashboard start.
#[test]
fn self_heal_is_idempotent_byte_for_byte() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let settings = fixture.settings();
    let _durable = fixture.durable();
    let dead = fixture
        .path()
        .join("pruned-worktree")
        .join("dot-agent-deck");

    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "model": "sonnet",
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": format!("{} {CLAUDE_SUFFIX}", dead.display()) } ] }
                ]
            }
        }))
        .expect("serialize fixture"),
    )
    .expect("seed settings.json");

    let install = || {
        fixture.auto_install(&settings, || {
            durable_binary_path_with(Ok(artifact.clone()), &home, None)
        });
    };
    install();
    let first = std::fs::read(&settings).expect("read after first pass");
    install();
    let second = std::fs::read(&settings).expect("read after second pass");

    assert_eq!(
        String::from_utf8_lossy(&first),
        String::from_utf8_lossy(&second),
        "a second self-heal pass changed the file"
    );
    let body = String::from_utf8_lossy(&first);
    assert!(
        body.contains("\"model\": \"sonnet\""),
        "the user's unrelated settings did not survive:\n{body}"
    );
}

// ---------------------------------------------------------------------------
// M6 — loud failure, and genuinely no write.
// ---------------------------------------------------------------------------

/// The explicit install refuses with an actionable message and writes nothing —
/// no file created, no partial JSON. The refusal is the first statement in
/// `install_with`, before the settings path is even computed, which is what
/// makes "nothing was touched" structural rather than incidental.
#[test]
fn explicit_install_refuses_loudly_and_writes_nothing() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let settings = fixture.settings();

    // No `~/.local/bin` candidate and an empty `$PATH` — the 2c refusal.
    let empty = fixture.path().join("empty-bin");
    std::fs::create_dir_all(&empty).expect("create empty PATH dir");
    let path_value = std::env::join_paths([empty]).expect("join synthetic PATH");

    let err = dot_agent_deck::hooks_manage::install_with(|| {
        durable_binary_path_with(Ok(artifact.clone()), &home, Some(path_value.as_os_str()))
    })
    .expect_err("no durable path must be an error, not a silent bad write");

    assert!(
        err.contains(artifact.to_str().expect("artifact is UTF-8")),
        "the error must name the rejected path: {err}"
    );
    assert!(
        err.contains("cargo install --path ."),
        "the error must name the fix: {err}"
    );
    assert!(
        !settings.exists(),
        "a refused install created {}",
        settings.display()
    );
}

/// The silent startup path refuses the same way but through `tracing` — it must
/// write nothing and it must not print. `auto_install`'s documented contract is
/// "never prints to stdout" and the dashboard-startup path depends on it, so the
/// refusal is a `warn!` and nothing else.
#[test]
fn auto_install_refusal_writes_nothing_and_only_warns() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let artifact = fixture.build_artifact();
    let settings = fixture.settings();
    let empty = fixture.path().join("empty-bin");
    std::fs::create_dir_all(&empty).expect("create empty PATH dir");
    let path_value = std::env::join_paths([empty]).expect("join synthetic PATH");

    let logs = fixture.auto_install(&settings, || {
        durable_binary_path_with(Ok(artifact.clone()), &home, Some(path_value.as_os_str()))
    });

    assert!(
        !settings.exists(),
        "a refused auto-install created {}",
        settings.display()
    );
    assert!(
        logs.contains("WARN") && logs.contains("auto-install"),
        "the refusal was not reported on the log surface:\n{logs}"
    );
    assert!(
        logs.contains("cargo install --path ."),
        "the logged refusal must stay actionable:\n{logs}"
    );
}

/// Issue #536, at the level a user feels it: when `current_exe()` itself fails,
/// the deck must not write a bare `dot-agent-deck` into a file Claude Code
/// hands to `/bin/sh`. Nothing is written at all.
#[test]
fn an_unresolvable_current_exe_writes_no_bare_command_name() {
    let fixture = Fixture::new();
    let home = fixture.home();
    let settings = fixture.settings();
    // Seeded on purpose: even with a durable candidate right there, an unknown
    // `current_exe()` is a refusal, never a guess.
    let _durable = fixture.durable();

    fixture.auto_install(&settings, || {
        durable_binary_path_with(Err(std::io::Error::other("no such process")), &home, None)
    });

    assert!(
        !settings.exists(),
        "issue #536: a failed current_exe() must write nothing, not a bare command name"
    );
}
