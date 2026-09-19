//! Issue #1140 — a deck run from a **scratch copy** of itself must not pin that
//! copy into another program's persistent, user-level configuration.
//!
//! The field case: a branch build was copied to
//! `/var/tmp/dad-branch/bin/dot-agent-deck` to drive an isolated sandbox
//! daemon. That path is outside `target/`, so PRD #381's build-artifact guard
//! passed it, and hook auto-install wrote it into the **global** config of
//! every supported agent — *beside* the installed release's own entries rather
//! than replacing them, because each installer normalises only the rules naming
//! the binary currently installing. Ten Claude entries became twenty.
//!
//! **Why these drive the real binary as a subprocess.** The resolver's own
//! tests — `platform::paths`'s `mod tests` and `tests/durable_hook_binary_path.rs`
//! — inject `current_exe()`, which is what makes them able to test anything at
//! all (a real unusable `current_exe()` cannot be manufactured on demand).
//! Nothing in either exercises the **uninjected** path: the binary asking the
//! OS where it is and writing the answer into `~/.claude/settings.json`. That
//! is the whole of the defect's mechanism, and it is reachable only by exec'ing
//! a deck that genuinely lives somewhere scratch.
//!
//! Fast tier, not `e2e`: a CLI subprocess against an isolated `HOME`, no PTY,
//! no daemon, no LLM — the same shape as `tests/worktree_reclaim.rs` and the
//! `daemon/status/004`–`005` entries.
//!
//! **Unix only, and the gate is isolation rather than portability** (Greptile
//! P1 on PR #1156, confirmed by `build-windows` going red on the first push).
//! These tests hand the child an `env_clear`ed environment carrying `HOME` and
//! `PATH`, which isolates it on Unix because `platform::paths::home_dir` reads
//! `HOME` there. On Windows it reads the known-folder API instead, so the child
//! would resolve `~/.local/bin` and `~/.claude/settings.json` under the
//! runner's **real** profile while every assertion here inspects the fixture —
//! failing, and writing into a profile no test owns. Isolating it properly
//! needs the Windows profile variables and a Windows executable fixture, which
//! is more than this regression needs; the catalog entries say `mac+linux`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use spec::spec;

// Issue #322: the fixture holds a hard link to (or, across a device boundary, a
// copy of) the ~240 MB deck binary, so it is exactly the shape that must not
// land on a RAM-backed `/tmp`. See `docs/develop/e2e-temp-dirs.md`.
#[path = "../src/test_temp.rs"]
mod test_temp;

/// The deck-owned command signature Claude's installer writes
/// (`hooks_manage::HOOK_COMMAND_SUFFIX`).
const CLAUDE_SUFFIX: &str = "hook --agent claude-code";

/// The file name the resolver searches for — the crate's package name plus the
/// platform's executable suffix, exactly as
/// `platform::paths::durable_binary_file_name` builds it. Load-bearing on
/// Windows for the reason PR #733 recorded in `durable_hook_binary_path.rs`: a
/// candidate seeded under the bare name is invisible to the resolver there.
fn durable_file_name() -> String {
    format!(
        "{}{}",
        dot_agent_deck::platform::paths::DEFAULT_BINARY_NAME,
        std::env::consts::EXE_SUFFIX
    )
}

/// An isolated `HOME` plus a deck binary living somewhere that is neither the
/// canonical `~/.local/bin` install target nor on the child's `PATH`.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            dir: test_temp::tempdir().expect("scratch-binary fixture tempdir"),
        };
        // `~/.claude/` present is what makes Claude "detected"; the explicit
        // `hooks install` writes regardless, but seeding it keeps the fixture
        // the same shape the auto path sees.
        std::fs::create_dir_all(fixture.home().join(".claude")).expect("seed ~/.claude");
        // An empty directory standing in for `PATH`, so the host's own installed
        // deck can never be what a pass resolves through.
        std::fs::create_dir_all(fixture.path().join("emptybin")).expect("create empty bindir");
        fixture
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    fn settings(&self) -> PathBuf {
        self.home().join(".claude").join("settings.json")
    }

    /// The deck binary under test, placed at a scratch path: outside `target/`,
    /// outside `<home>/.local/bin`, and not on the `PATH` [`Self::run`] hands
    /// the child.
    ///
    /// A **hard link** first, so the common case costs no bytes: the link is a
    /// genuinely distinct path to the same inode, and `/proc/self/exe` reports
    /// the path the process was exec'd through, which is what this test is
    /// about. `hard_link` fails across a device boundary (the fixture root and
    /// `target/` need not share one), so a copy is the fallback rather than the
    /// default.
    fn scratch_deck(&self) -> PathBuf {
        let scratch = self.path().join("opt").join("dad-branch").join("bin");
        std::fs::create_dir_all(&scratch).expect("create scratch bindir");
        let scratch = scratch.join(durable_file_name());
        let built = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        if std::fs::hard_link(built, &scratch).is_err() {
            std::fs::copy(built, &scratch).expect("copy the deck binary to the scratch path");
        }
        scratch
    }

    /// A stub executable at `<home>/.local/bin/<name>` — the install the
    /// resolver must prefer over the scratch copy. Never executed: PRD #381
    /// Open Question 5 decided the gate is "exists and is executable", so a
    /// stat and the exec bit are the whole of what this has to satisfy.
    fn seed_install(&self) -> PathBuf {
        let installed = self
            .home()
            .join(".local")
            .join("bin")
            .join(durable_file_name());
        std::fs::create_dir_all(installed.parent().expect("candidate has a parent"))
            .expect("create ~/.local/bin");
        std::fs::write(&installed, b"#!/bin/sh\nexit 0\n").expect("write the install stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755))
                .expect("chmod the install stub");
        }
        installed
    }

    /// Run the scratch deck with an environment that reaches nothing of the
    /// host's: `env_clear`, then only `HOME` and a `PATH` with no
    /// `dot-agent-deck` on it. Without the clear, the developer's own installed
    /// deck on `PATH` would resolve step 2b and both tests would pass for the
    /// wrong reason.
    fn run(&self, deck: &Path, args: &[&str]) -> std::process::Output {
        Command::new(deck)
            .current_dir(self.path())
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env("PATH", self.path().join("emptybin"))
            .output()
            .expect("run the scratch dot-agent-deck")
    }
}

/// Every `"command"` string anywhere in a hook document — the whole tree rather
/// than the documented nesting, so a path smuggled into a differently-shaped
/// rule is still caught.
fn commands_in(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "command"
                    && let Some(text) = child.as_str()
                {
                    out.push(text.to_string());
                }
                commands_in(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                commands_in(item, out);
            }
        }
        _ => {}
    }
}

fn deck_commands(path: &Path) -> Vec<String> {
    let body =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let doc: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("parse {} as JSON: {e}\n{body}", path.display()));
    let mut out = Vec::new();
    commands_in(&doc, &mut out);
    out.retain(|command| command.trim_end().ends_with(CLAUDE_SUFFIX));
    out
}

fn combined(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Scenario: Hard-link the freshly built deck to a scratch path outside
/// `target/` (standing in for the report's `/var/tmp/dad-branch/bin/`), seed a
/// stub install at `$HOME/.local/bin/dot-agent-deck`, and run that scratch copy
/// as `hooks install --agent claude-code` with an isolated `HOME` and no deck
/// on `PATH`. Every hook command written into `~/.claude/settings.json` must
/// name the seeded install, and the scratch path must appear nowhere in the
/// file.
#[spec("hooks/install/007")]
#[test]
fn install_007_a_scratch_copy_pins_the_install_not_itself() {
    let fixture = Fixture::new();
    let installed = fixture.seed_install();
    let scratch = fixture.scratch_deck();

    let out = fixture.run(&scratch, &["hooks", "install", "--agent", "claude-code"]);
    assert!(
        out.status.success(),
        "`hooks install` failed with a durable install seeded:\n{}",
        combined(&out)
    );

    let settings = fixture.settings();
    let body = std::fs::read_to_string(&settings).expect("read settings.json");
    let commands = deck_commands(&settings);
    assert!(
        !commands.is_empty(),
        "no deck-owned rule was written:\n{body}"
    );

    let expected = format!("{} {CLAUDE_SUFFIX}", installed.display());
    for command in &commands {
        // Compared after stripping any quote wrapper, for the reason
        // `durable_hook_binary_path.rs` records: the writers quote differently
        // and what is pinned here is the PATH, not either one's quoting.
        let unquoted = command.trim_matches(['\'', '"']).to_string();
        assert!(
            unquoted == expected || command == &expected,
            "hook command `{command}` does not name the seeded install `{expected}`"
        );
    }

    let scratch_dir = scratch
        .parent()
        .expect("scratch binary has a parent")
        .display()
        .to_string();
    assert!(
        !body.contains(&scratch_dir),
        "the scratch copy was persisted into global config — issue #1140:\n{body}"
    );
}

/// Scenario: The same scratch copy, but with no install seeded at
/// `$HOME/.local/bin` and no `dot-agent-deck` on the child's `PATH`. `hooks
/// install --agent claude-code` must still succeed, pinning the running binary
/// as a last resort rather than refusing, and every command it writes must name
/// that binary — a machine whose only deck is this one gets hooks, not silence.
#[spec("hooks/install/008")]
#[test]
fn install_008_a_scratch_copy_with_no_install_pins_itself_as_a_last_resort() {
    let fixture = Fixture::new();
    let scratch = fixture.scratch_deck();

    let out = fixture.run(&scratch, &["hooks", "install", "--agent", "claude-code"]);
    let report = combined(&out);
    assert!(
        out.status.success(),
        "`hooks install` refused where the running binary was the only deck on the \
         machine — that buys no hooks at all, not caution:\n{report}"
    );

    let settings = fixture.settings();
    let body = std::fs::read_to_string(&settings).expect("read settings.json");
    let commands = deck_commands(&settings);
    assert!(
        !commands.is_empty(),
        "no deck-owned rule was written:\n{body}"
    );

    let expected = format!("{} {CLAUDE_SUFFIX}", scratch.display());
    for command in &commands {
        let unquoted = command.trim_matches(['\'', '"']).to_string();
        assert!(
            unquoted == expected || command == &expected,
            "hook command `{command}` does not name the running binary `{expected}`"
        );
    }
}
