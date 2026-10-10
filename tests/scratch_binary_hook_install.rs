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

    fn installed_home() -> Self {
        // Real OS temp directories are deliberately ineligible for takeover.
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
        std::fs::create_dir_all(base).expect("create Cargo test directory");
        let fixture = Self {
            dir: tempfile::Builder::new()
                .prefix("hook-takeover-")
                .tempdir_in(base)
                .expect("durable test HOME"),
        };
        std::fs::create_dir_all(fixture.home().join(".claude")).unwrap();
        std::fs::create_dir_all(fixture.path().join("emptybin")).unwrap();
        fixture
    }

    fn copy_deck_to(&self, path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let built = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        if std::fs::hard_link(built, path).is_err() {
            std::fs::copy(built, path).expect("copy real binary");
        }
    }

    fn versioned_pin(&self, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let pin = self.path().join("opt/old/dot-agent-deck");
        std::fs::create_dir_all(pin.parent().unwrap()).unwrap();
        std::fs::write(
            &pin,
            format!("#!/bin/sh\nprintf 'dot-agent-deck {version}\\n'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&pin, std::fs::Permissions::from_mode(0o755)).unwrap();
        pin
    }

    fn automatic_install(&self, deck: &Path, path: &Path) -> String {
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        struct Daemon(std::process::Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let socket = self.path().join("attach.sock");
        let mut daemon = Daemon(
            Command::new(deck)
                .args(["daemon", "serve"])
                .current_dir(self.path())
                .env_clear()
                .env("HOME", self.home())
                .env("PATH", path)
                .env("DOT_AGENT_DECK_SOCKET", self.path().join("hook.sock"))
                .env("DOT_AGENT_DECK_ATTACH_SOCKET", &socket)
                .env("DOT_AGENT_DECK_STATE_DIR", self.path().join("state"))
                .env("DOT_AGENT_DECK_LOG", self.path().join("deck.log"))
                .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("automatic install via daemon serve"),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        while !socket.exists() && Instant::now() < deadline {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "daemon exited before install"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let log = std::fs::read_to_string(self.path().join("deck.log")).unwrap_or_default();
        assert!(socket.exists(), "daemon never bound attach socket:\n{log}");
        log
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
    // The `DOT_AGENT_DECK_BIN` wrapper (PRD #1497) is stripped: what these
    // tests pin is the installed path that follows it.
    out.into_iter()
        .map(|command| {
            command
                .strip_prefix(dot_agent_deck::platform::paths::HOOK_BIN_OVERRIDE_PREFIX)
                .map(str::to_string)
                .unwrap_or(command)
        })
        .collect()
}

fn combined(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The user's matcher and handler retain their original slots, and every
/// installed event has exactly one deck handler.
fn assert_automatic_pin(fixture: &Fixture, expected: &Path) {
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.settings()).unwrap()).unwrap();
    let rule = &doc["hooks"]["PreToolUse"][0];
    assert_eq!(rule["matcher"], "Bash", "matcher changed: {doc:#}");
    assert_eq!(
        rule["hooks"][1]["command"], USER_HOOK,
        "user slot changed: {doc:#}"
    );
    assert!(
        rule["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(expected.to_str().unwrap())
    );
    for (event, rules) in doc["hooks"].as_object().unwrap() {
        let mut commands = Vec::new();
        commands_in(rules, &mut commands);
        commands.retain(|c| c.ends_with(CLAUDE_SUFFIX));
        assert_eq!(
            commands.len(),
            1,
            "{event} must have one deck handler: {doc:#}"
        );
        assert!(
            commands[0].contains(expected.to_str().unwrap()),
            "wrong pin: {doc:#}"
        );
    }
}

fn seed_shared_rule(fixture: &Fixture, pin: &Path) {
    let rule = serde_json::json!({
        "matcher": "Bash",
        "hooks": [
            {"type": "command", "command": plain_form(pin)},
            {"type": "command", "command": USER_HOOK}
        ]
    });
    let doc = serde_json::json!({"hooks": {"PreToolUse": [rule,
        {"hooks": [{"type": "command", "command": plain_form(pin)}]}]}});
    std::fs::write(fixture.settings(), doc.to_string()).unwrap();
}

/// Scenario: Start a real installed copy with Claude hooks pinned to an older
/// stub, then repeat with a newer stub. Automatic startup switches only the
/// older pin in place, keeping one deck handler, the user's handler and matcher.
#[spec("hooks/install/015")]
#[test]
fn install_015_a_newer_installed_copy_takes_over_in_place() {
    for (version, takes_over) in [("0.0.1", true), ("999.0.0", false)] {
        let fixture = Fixture::installed_home();
        let installed = fixture.home().join(".local/bin/dot-agent-deck");
        fixture.copy_deck_to(&installed);
        let pin = fixture.versioned_pin(version);
        seed_shared_rule(&fixture, &pin);
        fixture.automatic_install(&installed, &fixture.path().join("emptybin"));
        assert_automatic_pin(&fixture, if takes_over { &installed } else { &pin });
    }
}

/// Scenario: Start a newer real binary from a temporary directory on PATH with
/// Claude hooks pinned to an older durable stub. Automatic startup keeps the
/// old pin and preserves the user's shared rule instead of pinning the scratch copy.
#[spec("hooks/install/016")]
#[test]
fn install_016_a_temporary_path_copy_does_not_take_over() {
    let fixture = Fixture::new();
    let scratch = fixture.scratch_deck();
    let pin = fixture.versioned_pin("0.0.1");
    seed_shared_rule(&fixture, &pin);
    fixture.automatic_install(&scratch, scratch.parent().unwrap());
    assert_automatic_pin(&fixture, &pin);
}

/// Scenario: Start the only deck from an AppTranslocation bundle-shaped path
/// with no installed fallback. Automatic startup writes no Claude settings and
/// tells the user to move Agent Deck to /Applications.
#[spec("hooks/install/017")]
#[test]
fn install_017_a_translocated_copy_does_not_install_hooks() {
    let fixture = Fixture::new();
    let deck = fixture
        .path()
        .join("AppTranslocation/uuid/d/Agent Deck.app/Contents/MacOS/dot-agent-deck");
    fixture.copy_deck_to(&deck);
    let log = fixture.automatic_install(&deck, &fixture.path().join("emptybin"));
    assert!(
        !fixture.settings().exists(),
        "translocated binary wrote settings"
    );
    assert!(log.contains("/Applications"), "missing move remedy:\n{log}");
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

/// Scenario: Seed `~/.claude/settings.json` with a trailing comma — the user is
/// mid-edit — and, beside it, the `settings.json.bak` they copied aside before
/// they started. Run `hooks install --agent claude-code` with an install
/// seeded. The command must refuse with a non-zero exit, leave both files
/// byte-for-byte as they were, and not claim the user's own `.bak` as the
/// deck's backup (issue #537 items 1 and 2).
#[spec("hooks/install/010")]
#[test]
fn install_010_a_refused_install_exits_non_zero_and_leaves_a_users_backup_alone() {
    let fixture = Fixture::new();
    fixture.seed_install();
    let scratch = fixture.scratch_deck();

    let settings = fixture.settings();
    let malformed = "{\n  \"model\": \"opus\",\n  \"hooks\": {},\n}\n";
    std::fs::write(&settings, malformed).expect("seed malformed settings.json");
    let users_backup = settings.with_file_name("settings.json.bak");
    let good = "{\n  \"model\": \"opus\",\n  \"hooks\": {}\n}\n";
    std::fs::write(&users_backup, good).expect("seed the user's own settings.json.bak");

    let out = fixture.run(&scratch, &["hooks", "install", "--agent", "claude-code"]);
    let report = combined(&out);

    assert!(
        !out.status.success(),
        "`hooks install` refused the settings file but exited 0, so a script or a \
         provisioning step cannot see the refusal:\n{report}"
    );
    assert!(
        report.contains("not valid JSON"),
        "the refusal must say why:\n{report}"
    );
    assert_eq!(
        std::fs::read_to_string(&settings).expect("read settings.json"),
        malformed,
        "a refused install rewrote the settings file it refused"
    );
    assert_eq!(
        std::fs::read_to_string(&users_backup).expect("read settings.json.bak"),
        good,
        "the refusal replaced the user's own settings.json.bak — the one copy of their \
         config that still parsed:\n{report}"
    );
    assert!(
        !report.contains("preserved at"),
        "the message claims a backup the deck did not make:\n{report}"
    );
}

// --- PRD #1497: hook commands honour `DOT_AGENT_DECK_BIN` ----------------------

/// Every `"command"` string in the settings file, unfiltered and unstripped.
fn raw_commands(path: &Path) -> Vec<String> {
    let body =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let doc: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("parse {} as JSON: {e}\n{body}", path.display()));
    let mut out = Vec::new();
    commands_in(&doc, &mut out);
    out
}

/// The command `hooks install` writes for `exe`: the installed path behind the
/// `DOT_AGENT_DECK_BIN` wrapper.
fn override_form(exe: &Path) -> String {
    format!(
        "{}{} {CLAUDE_SUFFIX}",
        dot_agent_deck::platform::paths::HOOK_BIN_OVERRIDE_PREFIX,
        exe.display()
    )
}

/// The plain command every release before PRD #1497 wrote.
fn plain_form(exe: &Path) -> String {
    format!("{} {CLAUDE_SUFFIX}", exe.display())
}

/// A settings document holding the given commands under the given hook types,
/// each in a rule of its own, plus one user hook under `PreToolUse`.
fn seeded_settings(entries: &[(&str, String)]) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    hooks.insert(
        "PreToolUse".into(),
        serde_json::json!([{ "hooks": [{ "type": "command", "command": USER_HOOK }] }]),
    );
    for (hook_type, command) in entries {
        let rules = hooks
            .entry(hook_type.to_string())
            .or_insert_with(|| serde_json::json!([]));
        rules
            .as_array_mut()
            .expect("rules are an array")
            .push(serde_json::json!({ "hooks": [{ "type": "command", "command": command }] }));
    }
    serde_json::json!({ "model": "opus", "hooks": hooks })
}

const USER_HOOK: &str = "/usr/local/bin/my-audit.sh --before-tool";

/// A recording stub at `path`: writes its name, its arguments and its stdin to
/// `$OUT`, so a test can tell which binary a hook command ran and with what.
fn write_recording_stub(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("stub name");
    std::fs::create_dir_all(path.parent().expect("stub dir")).expect("create stub dir");
    // Written by a `/bin/cat` child, as `test_isolation::write_script` does in
    // the crate (private to it): this process never holds a write descriptor
    // on a file it then executes, so a fork elsewhere cannot leave one open
    // and turn the exec into ETXTBSY.
    let mut cat = Command::new("/bin/sh")
        .args(["-c", "exec /bin/cat > \"$1\"", "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn /bin/cat");
    {
        use std::io::Write as _;
        cat.stdin
            .take()
            .expect("cat stdin")
            .write_all(
                format!("#!/bin/sh\nprintf '%s|%s|' '{name}' \"$*\" > \"$OUT\"\ncat >> \"$OUT\"\n")
                    .as_bytes(),
            )
            .expect("write stub");
    }
    assert!(cat.wait().expect("wait for cat").success());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

/// Run `command` the way Claude Code runs a hook on Linux and macOS (`sh -c`,
/// per its hooks reference), with a hook payload on stdin and
/// `DOT_AGENT_DECK_BIN` set to `bin` or unset, and return what the stub that
/// ran recorded. `reach` is put on `PATH` and made the working directory, so a
/// stub there is what a bare or relative `bin` would run if it were honoured.
fn run_hook_command(command: &str, bin: Option<&Path>, out: &Path, reach: &Path) -> String {
    use std::io::Write as _;
    let _ = std::fs::remove_file(out);
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(reach)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", reach.display()))
        .env("OUT", out)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if let Some(bin) = bin {
        cmd.env(dot_agent_deck::platform::paths::DOT_AGENT_DECK_BIN, bin);
    }
    let mut child = cmd.spawn().expect("spawn /bin/sh");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(br#"{"hook_event_name":"Stop"}"#)
        .expect("write the hook payload");
    let output = child.wait_with_output().expect("wait for the hook command");
    assert!(
        output.status.success(),
        "the hook command failed: {command}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(out).expect("the stub recorded its run")
}

/// Scenario: Seed an install at `$HOME/.local/bin` and run `hooks install
/// --agent claude-code` from a scratch copy. Every deck command must name the
/// install behind the `DOT_AGENT_DECK_BIN` wrapper, and running one through
/// `sh -c` must run the binary the variable names when it is an absolute path,
/// and the install when it is unset, empty, a bare name on `PATH` or a path
/// relative to the working directory, with the hook's arguments and stdin
/// intact (PRD #1497).
#[spec("hooks/install/011")]
#[test]
fn install_011_hook_commands_run_dot_agent_deck_bin_when_it_is_set() {
    let fixture = Fixture::new();
    let installed = fixture.seed_install();
    write_recording_stub(&installed);
    let built = fixture.path().join("my build").join("built-deck");
    write_recording_stub(&built);
    let scratch = fixture.scratch_deck();

    let out = fixture.run(&scratch, &["hooks", "install", "--agent", "claude-code"]);
    assert!(out.status.success(), "{}", combined(&out));

    let commands = raw_commands(&fixture.settings());
    let deck: Vec<_> = commands
        .iter()
        .filter(|c| c.ends_with(CLAUDE_SUFFIX))
        .collect();
    assert!(
        !deck.is_empty(),
        "no deck command was written: {commands:?}"
    );
    for command in &deck {
        assert_eq!(*command, &override_form(&installed));
    }

    let record = fixture.path().join("ran");
    let payload = r#"{"hook_event_name":"Stop"}"#;
    let reach = built.parent().expect("the build's directory");
    let installed_record = format!("{}|hook --agent claude-code|{payload}", durable_file_name());
    assert_eq!(
        run_hook_command(deck[0], None, &record, reach),
        installed_record,
        "unset, the hook must run the installed deck exactly as before"
    );
    for ignored in ["", "built-deck", "./built-deck"] {
        assert_eq!(
            run_hook_command(deck[0], Some(Path::new(ignored)), &record, reach),
            installed_record,
            "{ignored:?} is not absolute, so the hook must run the installed deck"
        );
    }
    assert_eq!(
        run_hook_command(deck[0], Some(&built), &record, reach),
        format!("built-deck|hook --agent claude-code|{payload}"),
        "set to an absolute path, the hook must run the binary DOT_AGENT_DECK_BIN names"
    );
}

/// Scenario: Seed `settings.json` with the plain `<install> hook --agent
/// claude-code` commands an older release wrote, beside a user hook, and run
/// `hooks install --agent claude-code`. Each hook type must end with exactly
/// one deck command, now in the override form, the user hook untouched, and a
/// second install must leave the file byte for byte as it was (PRD #1497).
#[spec("hooks/install/012")]
#[test]
fn install_012_reinstall_migrates_the_plain_form_in_place_without_duplicating() {
    let fixture = Fixture::new();
    let installed = fixture.seed_install();
    let scratch = fixture.scratch_deck();
    let settings = fixture.settings();
    let old = plain_form(&installed);
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&seeded_settings(&[
            ("PreToolUse", old.clone()),
            ("Stop", old.clone()),
            ("SessionStart", old.clone()),
        ]))
        .expect("serialize"),
    )
    .expect("seed settings.json");

    let out = fixture.run(&scratch, &["hooks", "install", "--agent", "claude-code"]);
    assert!(out.status.success(), "{}", combined(&out));

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("parse");
    for (hook_type, rules) in doc["hooks"].as_object().expect("hooks object") {
        let mut commands = Vec::new();
        commands_in(rules, &mut commands);
        let deck: Vec<_> = commands
            .iter()
            .filter(|c| c.ends_with(CLAUDE_SUFFIX))
            .collect();
        assert_eq!(
            deck,
            vec![&override_form(&installed)],
            "{hook_type} must hold exactly one deck command, migrated: {commands:?}"
        );
    }
    assert!(
        raw_commands(&settings).contains(&USER_HOOK.to_string()),
        "the user's own hook must survive the migration"
    );
    assert!(
        !raw_commands(&settings).contains(&old),
        "no plain-form entry may be left beside the migrated one"
    );

    let after_first = std::fs::read(&settings).expect("read");
    let out = fixture.run(&scratch, &["hooks", "install", "--agent", "claude-code"]);
    assert!(out.status.success(), "{}", combined(&out));
    assert_eq!(
        std::fs::read(&settings).expect("read"),
        after_first,
        "a second install over the migrated form must change nothing"
    );
}

/// Scenario: Seed `settings.json` with one deck command in the plain form, one
/// in the override form and a user hook, and run `hooks uninstall --agent
/// claude-code`. Both deck commands must be gone and the user hook kept (PRD
/// #1497).
#[spec("hooks/install/013")]
#[test]
fn install_013_uninstall_removes_both_the_plain_and_the_override_form() {
    let fixture = Fixture::new();
    let installed = fixture.seed_install();
    let scratch = fixture.scratch_deck();
    let settings = fixture.settings();
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&seeded_settings(&[
            ("Stop", plain_form(&installed)),
            ("SessionStart", override_form(&installed)),
        ]))
        .expect("serialize"),
    )
    .expect("seed settings.json");

    let out = fixture.run(&scratch, &["hooks", "uninstall", "--agent", "claude-code"]);
    assert!(out.status.success(), "{}", combined(&out));

    let commands = raw_commands(&settings);
    assert_eq!(
        commands,
        vec![USER_HOOK.to_string()],
        "only the user's hook may remain after an uninstall"
    );
}

/// Scenario: Run `hooks install --agent claude-code` from a deck that lives in
/// a directory named `back\'; touch PWNED; #`, with no installed deck to
/// prefer, over a `settings.json` that already holds a deck entry. The install
/// must exit non-zero naming the path and the backslash, and leave
/// `settings.json` byte for byte as it was, because fish, which Codex may run
/// a hook in, reads that backslash as an escape inside single quotes and would
/// run `touch PWNED` (PRD #1497, tester H1).
#[spec("hooks/install/014")]
#[test]
fn install_014_a_backslash_in_the_deck_path_is_refused_and_the_settings_kept() {
    let fixture = Fixture::new();
    let hostile = fixture
        .path()
        .join("opt")
        .join("back\\'; touch PWNED; #")
        .join("bin");
    std::fs::create_dir_all(&hostile).expect("create the hostile bindir");
    let deck = hostile.join(durable_file_name());
    let built = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    if std::fs::hard_link(built, &deck).is_err() {
        std::fs::copy(built, &deck).expect("copy the deck binary to the hostile path");
    }
    let settings = fixture.settings();
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&seeded_settings(&[(
            "Stop",
            plain_form(Path::new("/opt/earlier/dot-agent-deck")),
        )]))
        .expect("serialize"),
    )
    .expect("seed settings.json");
    let before = std::fs::read(&settings).expect("read");

    let out = fixture.run(&deck, &["hooks", "install", "--agent", "claude-code"]);
    let report = combined(&out);
    assert!(
        !out.status.success(),
        "a backslash path must be refused:\n{report}"
    );
    assert!(
        report.contains("backslash") && report.contains("PWNED"),
        "the refusal must name the path and the reason:\n{report}"
    );
    assert_eq!(
        std::fs::read(&settings).expect("read"),
        before,
        "the refusal must leave the existing entries untouched"
    );
    assert!(!fixture.path().join("PWNED").exists());
}
