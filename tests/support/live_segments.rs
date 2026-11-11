//! A native synthetic agent owns the real installed helper; the reviewed F2
//! Python composer supplies its screen, input map and loss mechanisms. The
//! composer asks its native parent to invoke VISUAL through an atomic mailbox,
//! so helper ancestry is the genuine agent -> helper path, not a Bash tool.
//! No production state or observation is injected by that mailbox.
#![allow(dead_code)] // Individual scenarios use different fixture controls.

use crate::{common, inbox};
use dot_agent_deck::composer_probe::arm::{ArmReader, Publication};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

pub const WAIT: Duration = Duration::from_secs(10);
const MODE: &str = "DAD_LIVE_SEGMENTS_BASE";
const PANE: &str = "live-segments";

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

pub fn atomic(path: &Path, value: &Value) {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    temp.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    temp.persist(path).unwrap();
}

pub fn read_json(path: &Path) -> Value {
    assert!(
        common::wait_until(common::child_boot_budget(), || {
            fs::read(path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some()
        }),
        "fixture acknowledgement missing: {}",
        path.display()
    );
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// `path` with each entry that holds a `codex` replaced by a directory under
/// `mirrors` linking every other name in it, so no lookup by name finds a
/// `codex` while the entry's other programs (`python3`, wherever it lives)
/// stay where the daemon looks for them. An entry that can't be listed is
/// dropped rather than kept, since it may hold a `codex`.
fn path_without_codex(path: &str, mirrors: &Path) -> String {
    let entries = std::env::split_paths(path)
        .enumerate()
        .filter_map(|(i, dir)| {
            if fs::symlink_metadata(dir.join("codex")).is_err() {
                return Some(dir);
            }
            let mirror = mirrors.join(i.to_string());
            fs::create_dir(&mirror).expect("create a PATH mirror");
            for entry in fs::read_dir(&dir).ok()? {
                let name = entry.ok()?.file_name();
                if name != "codex" {
                    std::os::unix::fs::symlink(dir.join(&name), mirror.join(&name))
                        .expect("link a PATH mirror entry");
                }
            }
            Some(mirror)
        });
    std::env::join_paths(entries.collect::<Vec<_>>())
        .expect("PATH entries rejoin")
        .to_string_lossy()
        .into_owned()
}

fn checked_replace(source: &str, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "live-segments fixture patch must match exactly once: {anchor:?}"
    );
    source.replacen(anchor, replacement, 1)
}

pub struct Fixture {
    pub daemon: common::DaemonProc,
    pub base: PathBuf,
    pub agent: String,
    pub generation: u64,
    pub kind: &'static str,
    instance: PathBuf,
    exe: PathBuf,
    /// The daemon's tracing log, printed when a test fails (diagnosis only:
    /// nothing asserts on it here).
    log: PathBuf,
    _log_dir: Option<tempfile::TempDir>,
    _codex_bin: Option<tempfile::TempDir>,
    /// The mirrors [`path_without_codex`] put on the daemon's `PATH`.
    _daemon_bin: tempfile::TempDir,
    /// A Codex pane's own `PATH` and `hooks/list` answer (Q129b item 0).
    codex_pane: Option<CodexPaneEnv>,
    launcher: Option<Launcher>,
    hosted: Option<Hosted>,
    /// A program the pane's command runs the native fixture through
    /// ([`Self::absent_through`]).
    through: Option<&'static str>,
    /// The pane's `CODEX_HOME`, under the daemon's HOME, which is also the
    /// pane's working directory ([`DEFAULT_CODEX_HOME`]).
    codex_home: &'static str,
}

/// The pane's Codex home: the working directory's own `.codex`, which Codex's
/// sandbox keeps read-only, so the deck can check the box (ruling Q227b holds
/// a home elsewhere inside the working directory).
pub const DEFAULT_CODEX_HOME: &str = ".codex";

/// Test-owned launcher histories; these change processes, never daemon state.
#[derive(Clone, Copy)]
pub struct Launcher {
    pub mode: &'static str,
    pub declared: bool,
    pub lookalike: bool,
    pub pins: NativePins,
}

#[derive(Clone, Copy)]
pub enum NativePins {
    Both,
    Neither,
    OtherOnly,
}

/// Q35-22's interpreter-hosted installs: what the pane's interpreter-hosted
/// entrypoint (`discovery_npm_entry.py`) hands its process to before it
/// spawns the native agent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hosted {
    /// The npm shape itself: the script the spawn PATH names, under the
    /// interpreter its shebang names.
    Genuine,
    /// A byte-identical copy of that script, at another path, under the same
    /// interpreter.
    OtherScript,
    /// The same script under another interpreter: a copy of the same
    /// `python3` at another inode.
    OtherInterpreter,
    /// The genuine script, started by name through a launcher shaped like
    /// `devbox run codex-big` (issue #2258): the launcher prints its own
    /// line, puts a directory holding another `python3` (a copy at another
    /// inode) first on `PATH`, as devbox puts its own `node` there, and runs
    /// a generated script through `sh -c` that runs a version check
    /// (`codex --version`) and then `codex` by name with `codex-big`'s own
    /// `-c` options, none of which sets a key binding. `env python3` then
    /// finds that other interpreter on the `PATH` the launcher handed it.
    LauncherPath,
}

impl Hosted {
    fn variant(self) -> &'static str {
        match self {
            Self::Genuine => "genuine",
            Self::OtherScript => "other-script",
            Self::OtherInterpreter => "other-interpreter",
            Self::LauncherPath => "launcher-path",
        }
    }
}

/// What a Codex pane is spawned with beyond the common environment: the
/// `codex app-server` stand-in first on its `PATH`, and the stand-in's
/// `hooks/list` answer.
struct CodexPaneEnv {
    path: String,
    hook_list: String,
}

/// Q129b item 0: record the deck's scoped trust for its own Codex hooks in
/// the pane's `$CODEX_HOME` before the first spawn, through the real
/// `hooks install --agent codex`, as an install the user ran earlier would
/// have. The deck takes the keymap identity of `$CODEX_HOME/config.toml`
/// at spawn (R6 §9.2), so it then already holds the records, and the
/// wrapper's own trust write at spawn finds them in place and writes
/// nothing (`codex_hooks_manage::edit_trust_state`: an edit that changes
/// nothing is not a write).
///
/// The seeding sidesteps a production case this fixture therefore can't
/// show: where nothing recorded the trust before the spawn, the wrapper's
/// own trust write lands after the spawn's keymap identity was taken, and
/// every check of that pane is refused `Keymap(Changed)` for the whole
/// generation. That happens for a pane whose `CODEX_HOME` differs from the
/// daemon's, or a Codex not on the daemon's `PATH` at startup (the daemon's
/// startup install skips it). Named for Q35-22 / Q35-29.
fn seed_codex_trust(daemon: &common::DaemonProc, pane: &CodexPaneEnv, codex_home: &Path) {
    let mut command = Command::new(common::contained_deck_binary());
    command
        .args(["hooks", "install", "--agent", "codex"])
        .current_dir(&daemon.home)
        .env_clear()
        .env("PATH", &pane.path)
        .env("HOME", &daemon.home)
        .env("CODEX_HOME", codex_home)
        .env("CODEX_HOOK_LIST_RESPONSE", &pane.hook_list);
    for (key, value) in common::config_containment_env() {
        command.env(key, value);
    }
    let out = command.output().expect("run hooks install --agent codex");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("Trusted hooks: 6"),
        "seeding the deck's Codex hook trust: {out:?}"
    );
    assert!(
        fs::read_to_string(codex_home.join("config.toml"))
            .is_ok_and(|toml| toml.contains("trusted_hash")),
        "the trust records are in the pane's config.toml before the spawn"
    );
}

/// What a failing test prints of the daemon's log: the probe's and the
/// doorbell's lines, newest last, at most this many.
const LOG_LINES_PRINTED: usize = 1500;
/// The longest line printed whole.
const LOG_LINE_CHARS: usize = 700;

/// The targets whose lines a failing test prints, and the verbosity the
/// fixture's daemon logs them at.
const LOG_FILTER: &str = "composer_probe=debug,doorbell=debug";

impl Fixture {
    pub fn start(kind: &'static str) -> Self {
        Self::with_env(kind, &[])
    }

    pub fn with_env(kind: &'static str, extra: &[(&str, &str)]) -> Self {
        Self::configured(kind, extra, true, None, None, None, DEFAULT_CODEX_HOME)
    }

    /// [`Self::with_env`] with the pane's `CODEX_HOME` at `codex_home` under
    /// the daemon's HOME, the pane's working directory.
    pub fn with_codex_home(
        kind: &'static str,
        extra: &[(&str, &str)],
        codex_home: &'static str,
    ) -> Self {
        Self::configured(kind, extra, true, None, None, None, codex_home)
    }

    /// [`Self::with_env`] with `files` (paths relative to the daemon's HOME,
    /// which is also the pane's HOME and working directory, and contents)
    /// written before the first spawn, so the spawn's key-binding baseline
    /// reads them.
    pub fn with_home_files(
        kind: &'static str,
        extra: &[(&str, &str)],
        files: &[(&str, &str)],
    ) -> Self {
        Self::seeded(
            kind,
            extra,
            true,
            None,
            None,
            None,
            DEFAULT_CODEX_HOME,
            files,
        )
    }

    /// Production Absent control: no editor-probe assumption or binding seam.
    pub fn absent(kind: &'static str, extra: &[(&str, &str)]) -> Self {
        Self::configured(kind, extra, false, None, None, None, DEFAULT_CODEX_HOME)
    }

    /// [`Self::absent`], with the pane's command running the native fixture
    /// through `program` (e.g. `env`), so the deck classes the pane as a
    /// launcher of its declared agent rather than the agent started directly.
    /// None of [`Self::launcher`]'s discovery histories or seams apply.
    pub fn absent_through(
        kind: &'static str,
        program: &'static str,
        extra: &[(&str, &str)],
    ) -> Self {
        Self::configured(
            kind,
            extra,
            false,
            None,
            None,
            Some(program),
            DEFAULT_CODEX_HOME,
        )
    }

    pub fn launcher(kind: &'static str, launcher: Launcher, extra: &[(&str, &str)]) -> Self {
        Self::configured(
            kind,
            extra,
            true,
            Some(launcher),
            None,
            None,
            DEFAULT_CODEX_HOME,
        )
    }

    /// An npm-shaped install (Q35-22's interpreter-hosted rule): the pane
    /// starts `kind` by bare name, and the PATH it is spawned with resolves
    /// that name only to a symlink to an interpreter-hosted script
    /// (`#!/usr/bin/env python3`, standing in for npm Codex's
    /// `#!/usr/bin/env node`), which spawns the native agent, kept off that
    /// PATH, as its child. Declared, so the pane is its agent's direct class,
    /// and delayed until [`Self::launch`] like a launcher history.
    pub fn interpreter_hosted(kind: &'static str, hosted: Hosted, extra: &[(&str, &str)]) -> Self {
        let launcher = Launcher {
            mode: "spawn",
            declared: true,
            lookalike: false,
            pins: NativePins::Both,
        };
        Self::configured(
            kind,
            extra,
            true,
            Some(launcher),
            Some(hosted),
            None,
            DEFAULT_CODEX_HOME,
        )
    }

    fn configured(
        kind: &'static str,
        extra: &[(&str, &str)],
        assumed: bool,
        launcher: Option<Launcher>,
        hosted: Option<Hosted>,
        through: Option<&'static str>,
        codex_home: &'static str,
    ) -> Self {
        Self::seeded(
            kind,
            extra,
            assumed,
            launcher,
            hosted,
            through,
            codex_home,
            &[],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn seeded(
        kind: &'static str,
        extra: &[(&str, &str)],
        assumed: bool,
        launcher: Option<Launcher>,
        hosted: Option<Hosted>,
        through: Option<&'static str>,
        codex_home: &'static str,
        files: &[(&str, &str)],
    ) -> Self {
        let mut env = vec![("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")];
        // Claude Code's alone: Codex's classes are in the shipped rollout
        // since the Codex activation, and its generations are checked with
        // Codex's measured facts (the assumption refuses a list naming a
        // Codex release).
        if assumed {
            env.push(("DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE", "claude@2.1.310"));
        }
        // Diagnosis (Q35-21): the daemon logs the probe's and the
        // doorbell's decisions, and a failing test prints them. A caller
        // that names its own log keeps it, and its lines are printed.
        let named = extra
            .iter()
            .rev()
            .find(|(k, _)| *k == "DOT_AGENT_DECK_LOG")
            .map(|(_, v)| PathBuf::from(v));
        let log_dir = named
            .is_none()
            .then(|| common::harness_tempdir().expect("fixture log dir"));
        let log = named.unwrap_or_else(|| log_dir.as_ref().unwrap().path().join("deck.log"));
        let log_value = log.to_str().expect("log path is UTF-8").to_owned();
        env.push(("RUST_LOG", LOG_FILTER));
        env.push(("DOT_AGENT_DECK_LOG", &log_value));
        // Codex's hooks are trusted, as for a Codex the deck could set up:
        // the deck's wrapper asks `codex app-server` for its hooks' trust
        // hashes, which the copied native fixture (named `codex`) can't
        // answer. Without trust the wrapper would report every redraw as the
        // card's status (issue #1493's degraded mode), and a ring's own echo
        // would turn the card busy mid-ring. The stand-in is the one
        // `tests/e2e_inbox_codex_doorbell.rs` uses, copied out of the
        // checkout: the harness drops every `PATH` entry inside it.
        //
        // It goes on the PANE's `PATH` only (Q129b item 0). On the daemon's,
        // its startup install would write trust into `$HOME/.codex` from a
        // thread of its own, and for a pane launched in `$HOME` that file is
        // one of Codex's project layers, which are keymap sources (R6 §9.2):
        // a write after the spawn's keymap identity refuses every check
        // `Keymap(Changed)`.
        let codex_bin = (kind == "codex").then(|| {
            let dir = common::harness_tempdir().expect("codex stand-in dir");
            let to = dir.path().join("codex");
            fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-synthetic/codex"),
                &to,
            )
            .expect("copy the codex app-server stand-in");
            fs::set_permissions(&to, fs::Permissions::from_mode(0o755)).unwrap();
            dir
        });
        // The daemon's own `PATH` carries no `codex`, whatever this machine
        // has installed (loc10): the comment above holds only while that is
        // so. A developer's real Codex in a directory such as
        // `/usr/local/bin` survives `common::contain_path`, so the daemon's
        // startup install ran that `codex app-server` and wrote trust into
        // `$HOME/.codex/config.toml` after the spawn's keymap identity, and
        // every check of a Codex pane was refused `Keymap(Changed)` there,
        // while the same tests passed on GitHub Actions and RWX.
        let daemon_bin = common::harness_tempdir().expect("daemon PATH dir");
        // A launcher history's daemon gets the fixture-only `/usr/bin:/bin`
        // (S-22), which this machine's agents are not in.
        let daemon_path = path_without_codex(
            &if launcher.is_some() {
                "/usr/bin:/bin".to_owned()
            } else {
                common::contain_path(&std::env::var("PATH").unwrap_or_default())
            },
            daemon_bin.path(),
        );
        env.push(("PATH", &daemon_path));
        env.extend_from_slice(extra);
        let daemon = common::spawn_daemon_serve_with_env(None, "0", &env);
        let codex_pane_env = codex_bin.as_ref().map(|dir| {
            let pane = CodexPaneEnv {
                path: common::contain_path(&format!(
                    "{}:{}",
                    dir.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                )),
                hook_list: codex_hook_list_response(),
            };
            seed_codex_trust(&daemon, &pane, &daemon.home.join(codex_home));
            pane
        });
        let native = daemon.home.join("native");
        fs::create_dir(&native).unwrap();
        let exe = native.join(kind);
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        if launcher.is_some() {
            executable(
                &native.join("launcher"),
                include_str!("discovery_launcher.py"),
            );
            let fake = native.join("lookalike").join(kind);
            fs::create_dir(fake.parent().unwrap()).unwrap();
            // Same bytes, name and claimed argv; a different kernel executable.
            fs::copy(&exe, &fake).unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
            // An undeclared wrapper may claim either agent through its hook.
            // Give the saved launch PATH independent native identities for both.
            let other = native.join(if kind == "codex" { "claude" } else { "codex" });
            fs::copy(&exe, &other).unwrap();
            fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
            let pins = native.join("spawn-pins");
            fs::create_dir(&pins).unwrap();
            if launcher.is_some_and(|l| matches!(l.pins, NativePins::OtherOnly)) {
                fs::hard_link(&other, pins.join(other.file_name().unwrap())).unwrap();
            }
        }
        let composer = checked_replace(
            include_str!("composer_external_edit.py"),
            "# M31: keys unread by an editor during suspension disappear.",
            "# Optional adversarial post-enable flush witness, local to S-20.\n                if self.path('.gate-flush').exists():\n                    os.write(1, b'\\x1b[?2004h')\n                    self.path('.flush-arrived').touch()\n                    while not self.path('.flush-go').exists(): time.sleep(.005)\n                # M31: keys unread by an editor during suspension disappear.",
        );
        let composer = checked_replace(
            &composer,
            "    def tokens(self, chunk):\n",
            "    def tokens(self, chunk):\n        # A configured whole terminal report is inert to this stand-in.\n        # Candidate histories deliberately retain F2's normal Ctrl+G map.\n        report = self.path('.reported-osc')\n        if report.exists(): chunk = chunk.replace(report.read_bytes(), b'')\n",
        );
        let composer = checked_replace(
            &composer,
            "            path.unlink(missing_ok=True)\n            os.write(1, b'\\x1b[?2004h')",
            "            path.unlink(missing_ok=True)\n            os.write(1, b'\\x1b[?2004h')\n            self.return_count = getattr(self, 'return_count', 0) + 1\n            self.path(f'.composer-return-{self.return_count}').touch()",
        );
        fs::write(native.join("composer_external_edit.py"), composer).unwrap();
        let composer = if kind == "codex" {
            include_str!("codex_composer.py").to_owned()
        } else {
            checked_replace(
                include_str!("claude_composer.py"),
                "            result = subprocess.run([deck, 'inbox', 'fetch']",
                "            if os.path.exists(base + '.fetch-paused') or (os.path.exists(base + '.fetch-notice-only') and b'dot-agent-deck: message ' not in line):\n                continue\n            result = subprocess.run([deck, 'inbox', 'fetch']",
            )
        };
        let composer = checked_replace(
            &composer,
            "    if not select.select([0], [], [], 0.025)[0]:",
            "    if os.path.exists(base + '.external-edit-request'):\n        os.remove(base + '.external-edit-request')\n        external_edit.invoke(composer)\n        redraw()\n        open(base + '.external-edit-returned', 'w').close()\n    if not select.select([0], [], [], 0.025)[0]:",
        );
        fs::write(native.join("composer.py"), composer).unwrap();
        let editor = checked_replace(
            include_str!("composer_test_editor.py"),
            "mode, base, *args = sys.argv[1:]",
            "mode, base, *args = sys.argv[1:]\nmode_file = Path(base + '.editor-mode')\nif mode_file.exists(): mode = mode_file.read_text().strip()",
        );
        let editor = checked_replace(
            &editor,
            "path.write_bytes(b'EDITED",
            "if mode == 'survivor':\n    survivor = os.fork()\n    if survivor == 0:\n        deadline = time.monotonic() + 30\n        while not Path(base + '.survivor-go').exists() and time.monotonic() < deadline: time.sleep(.005)\n        os._exit(0)\n    Path(base + '.survivor-pid').write_text(str(survivor))\npath.write_bytes(b'EDITED",
        );
        let editor = checked_replace(
            &editor,
            "path.write_bytes(b'EDITED   \\n' if mode == 'trim' else b'EDITED')",
            "if mode != 'preserve':\n    path.write_bytes(b'EDITED   \\n' if mode == 'trim' else b'EDITED')",
        );
        executable(&native.join("editor"), &editor);
        executable(&native.join("broker"), BROKER);
        if launcher.is_some() {
            for name in [
                "composer.py",
                "composer_external_edit.py",
                "editor",
                "broker",
            ] {
                fs::copy(native.join(name), native.join("lookalike").join(name)).unwrap();
            }
        }
        if hosted.is_some() {
            // npm's layout: `bin/<kind>` is a symlink to the package's
            // entrypoint script; the native agent is `native/<kind>`, beside
            // the package and off the spawn PATH.
            let package = native.join("npm");
            for dir in ["bin", "lib", "other"] {
                fs::create_dir_all(package.join(dir)).unwrap();
            }
            let entry = include_str!("discovery_npm_entry.py");
            executable(&package.join("lib").join("entry.py"), entry);
            executable(&package.join("other").join("entry.py"), entry);
            std::os::unix::fs::symlink("../lib/entry.py", package.join("bin").join(kind)).unwrap();
            // What `/usr/bin/env python3` runs on the spawn PATH, copied to
            // another inode for the other-interpreter control.
            let python = fs::canonicalize("/usr/bin/python3").expect("the hosts' /usr/bin/python3");
            let copy = package.join("other").join("python3");
            fs::copy(&python, &copy).unwrap();
            fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();
            if hosted == Some(Hosted::LauncherPath) {
                // `devbox run codex-big`'s shape: `devbox` prints its line,
                // puts its own profile first on `PATH` and runs the
                // generated script through `sh -c`; the script runs this
                // repo's version check, then `codex` by name. The profile is
                // `other/`, whose `python3` is another inode than the one the
                // spawn `PATH` resolves.
                let launcher = package.join("launcher");
                fs::create_dir_all(&launcher).unwrap();
                let check = launcher.join("require-codex-version.sh");
                executable(
                    &check,
                    "#!/bin/sh\ninstalled=$(codex --version 2>/dev/null | awk '{print $NF}') || true\n[ -n \"$installed\" ] || { echo 'Codex is not installed' >&2; exit 1; }\n",
                );
                let generated = launcher.join("codex-big.sh");
                executable(
                    &generated,
                    &format!(
                        "set -e\n{}\ncodex --live-segments-fixture -c model_reasoning_effort=high -c check_for_update_on_startup=false\n",
                        quote(&check)
                    ),
                );
                executable(
                    &launcher.join("devbox"),
                    &format!(
                        "#!/bin/sh\nprintf 'Info: Running script \"codex-big\" on %s\\r\\n' \"$PWD\"\nPATH={}:\"$PATH\"\nexport PATH\nexec /bin/sh -c {}\n",
                        quote(&package.join("other")),
                        quote(Path::new(&quote(&generated)))
                    ),
                );
            }
        }
        let mut f = Self {
            daemon,
            base: PathBuf::new(),
            agent: String::new(),
            generation: 0,
            kind,
            instance: PathBuf::new(),
            exe,
            log,
            _log_dir: log_dir,
            _codex_bin: codex_bin,
            _daemon_bin: daemon_bin,
            codex_pane: codex_pane_env,
            launcher,
            hosted,
            through,
            codex_home,
        };
        for (path, contents) in files {
            let path = f.daemon.home.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
        }
        f.spawn("first", false);
        f
    }

    /// `after_stop` is set only when the pane's previous generation was just
    /// stopped: `StopAgent` replies before it drops its pane-cleanup hold
    /// (#1218), so a spawn into the same pane straight after it can be
    /// refused with `Duplicate pane id`. That exact refusal is retried,
    /// bounded by `WAIT`; any other reply, and the same refusal once the
    /// bound is spent, still fails the spawn.
    fn spawn(&mut self, name: &str, after_stop: bool) {
        self.base = self.daemon.home.join(name);
        assert!(
            self.exe.is_file(),
            "native copied fixture exists before spawn: {}",
            self.exe.display()
        );
        let editor = format!(
            "{} fast {}",
            self.exe.parent().unwrap().join("editor").display(),
            self.base.display()
        );
        let mut env = json!([
            [MODE, self.base.to_str().unwrap()],
            ["DOT_AGENT_DECK_PANE_ID", PANE],
            ["DAD_LIVE_SEGMENTS_KIND", self.kind],
            ["VISUAL", editor],
            ["EDITOR", editor],
            ["TMPDIR", self.daemon.home.join("tmp")],
            ["CODEX_HOME", self.daemon.home.join(self.codex_home)]
        ]);
        if let Some(pane) = &self.codex_pane {
            let env = env.as_array_mut().unwrap();
            // A launcher history sets the pane's `PATH` itself, below.
            if self.launcher.is_none() {
                env.push(json!(["PATH", pane.path]));
            }
            env.push(json!(["CODEX_HOOK_LIST_RESPONSE", pane.hook_list]));
        }
        let command = if self.hosted == Some(Hosted::LauncherPath) {
            format!(
                "{} run codex-big",
                quote(
                    &self
                        .exe
                        .parent()
                        .unwrap()
                        .join("npm")
                        .join("launcher")
                        .join("devbox")
                )
            )
        } else if self.hosted.is_some() {
            format!("{} --live-segments-fixture", self.kind)
        } else if self.launcher.is_some() {
            format!(
                "{} --discovery-fixture",
                quote(&self.exe.parent().unwrap().join("launcher"))
            )
        } else if let Some(program) = self.through {
            format!("{program} {} --live-segments-fixture", quote(&self.exe))
        } else {
            format!("{} --live-segments-fixture", quote(&self.exe))
        };
        let mut request = json!({
        "op":"start-agent", "command":command, "cwd":self.daemon.home,
            "rows":40,"cols":140,"display_name":"segment-fixture",
            "agent_type":if self.kind == "codex" {"codex"} else {"claude_code"},
            "env":env,
        });
        if let Some(launcher) = self.launcher {
            if !launcher.declared {
                request.as_object_mut().unwrap().remove("agent_type");
            }
            let env = request["env"].as_array_mut().unwrap();
            let path_dir = if self.hosted.is_some() {
                self.exe.parent().unwrap().join("npm").join("bin")
            } else if matches!(launcher.pins, NativePins::Both) {
                self.exe.parent().unwrap().to_owned()
            } else {
                self.exe.parent().unwrap().join("spawn-pins")
            };
            let spawn_path = format!("{}:/usr/bin:/bin", path_dir.display());
            if !matches!(launcher.pins, NativePins::Both) {
                for name in ["claude", "codex"] {
                    let resolved = std::env::split_paths(&spawn_path).find_map(|dir| {
                        let candidate = dir.join(name);
                        candidate.is_file().then_some(candidate)
                    });
                    if name != self.kind && matches!(launcher.pins, NativePins::OtherOnly) {
                        assert_eq!(
                            resolved,
                            Some(path_dir.join(name)),
                            "only the other pin exists"
                        );
                    } else {
                        assert_eq!(resolved, None, "spawn PATH must lack native name {name}");
                    }
                }
            }
            env.extend([
                json!(["DAD_DISCOVERY_LAUNCH", launcher.mode]),
                json!([
                    "DAD_DISCOVERY_LOOKALIKE",
                    if launcher.lookalike { "1" } else { "0" }
                ]),
                json!([
                    "DAD_DISCOVERY_ABSOLUTE",
                    if matches!(launcher.pins, NativePins::Both) {
                        "0"
                    } else {
                        "1"
                    }
                ]),
                json!(["PATH", spawn_path]),
            ]);
            if let Some(hosted) = self.hosted {
                env.push(json!(["DAD_HOSTED_VARIANT", hosted.variant()]));
            }
        }
        let duplicate = format!("Duplicate pane id: {PANE}");
        let deadline = std::time::Instant::now() + WAIT;
        let reply = loop {
            let reply = inbox::attach(&self.daemon, request.clone());
            if !after_stop
                || reply["error"] != duplicate.as_str()
                || std::time::Instant::now() >= deadline
            {
                break reply;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        assert_eq!(reply["ok"], true, "native synthetic spawn: {reply}");
        self.agent = reply["id"].as_str().unwrap().into();
        let ready = read_json(&self.path(if self.launcher.is_some() {
            "launcher-ready"
        } else {
            "ready"
        }));
        self.generation = ready["generation"]
            .as_u64()
            .expect("installed synthetic generation");
        self.instance = PathBuf::from(ready["instance"].as_str().unwrap());
        if self.launcher.is_none() {
            assert_eq!(ready["exe"], json!(self.exe), "native exec acknowledgement");
            self.wait_screen();
        }
    }

    pub fn launch(&self) -> Value {
        self.flag("launch-go");
        let ready = read_json(&self.path("ready"));
        let expected = if self.launcher.is_some_and(|l| l.lookalike) {
            self.exe.parent().unwrap().join("lookalike").join(self.kind)
        } else {
            self.exe.clone()
        };
        assert_eq!(ready["exe"], json!(expected), "native exec acknowledgement");
        assert_eq!(ready["generation"], self.generation);
        self.wait_screen();
        ready
    }

    pub fn instance(&self) -> &Path {
        &self.instance
    }

    pub fn native_executable(&self) -> &Path {
        &self.exe
    }

    fn wait_screen(&self) {
        assert!(
            common::wait_until(common::child_boot_budget(), || {
                let screen = common::pane_snapshot_on(&self.daemon.attach_socket, &self.agent);
                let mut parser =
                    vt100::Parser::new(40.try_into().unwrap(), 140.try_into().unwrap(), 0);
                parser.process(&screen);
                parser
                    .screen()
                    .contents()
                    .contains(if self.kind == "codex" {
                        "OpenAI Codex"
                    } else {
                        "Claude Code"
                    })
            }),
            "F2 screen must be visible before any probe assertion: {}",
            self.screen()
        );
    }

    pub fn path(&self, suffix: &str) -> PathBuf {
        self.base.with_extension(suffix)
    }
    pub fn flag(&self, suffix: &str) {
        fs::write(self.path(suffix), "").unwrap();
    }
    pub fn setting(&self, suffix: &str, value: &str) {
        fs::write(self.path(suffix), value).unwrap();
    }
    pub fn remove(&self, suffix: &str) {
        let _ = fs::remove_file(self.path(suffix));
    }
    pub fn wait(&self, suffix: &str) {
        assert!(
            common::wait_until(common::child_boot_budget(), || self.path(suffix).exists()),
            "fixture never reached {suffix}; input {:?}",
            self.input()
        );
    }
    pub fn connect(&self) -> common::AttachInput {
        common::AttachInput::attach(&self.daemon.attach_socket, &self.agent)
    }
    /// The same attach as [`Self::connect`], keeping the refusals it is sent.
    pub fn connect_watching_refusals(&self) -> RefusalWatch {
        use dot_agent_deck::daemon_protocol::{AttachRequest, KIND_REQ, KIND_STREAM_REJECT};
        use std::io::Read;
        let stream = std::os::unix::net::UnixStream::connect(&self.daemon.attach_socket)
            .expect("connect to the attach socket");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = stream.try_clone().unwrap();
        let refusals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = refusals.clone();
        let reader = std::thread::spawn(move || {
            loop {
                let mut header = [0u8; 5];
                if reader.read_exact(&mut header).is_err() {
                    break;
                }
                let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
                let mut body = vec![0; len];
                if reader.read_exact(&mut body).is_err() {
                    break;
                }
                if header[0] == KIND_STREAM_REJECT {
                    log.lock().unwrap().push(body);
                }
            }
        });
        let mut watch = RefusalWatch {
            stream,
            refusals,
            reader: Some(reader),
        };
        let request = AttachRequest::AttachStream {
            id: self.agent.clone(),
            rows: None,
            cols: None,
            geometry_updates: false,
            client_id: None,
        };
        watch.frame(KIND_REQ, &serde_json::to_vec(&request).unwrap());
        watch
    }
    pub fn input(&self) -> Vec<u8> {
        fs::read(self.path("bytes")).unwrap_or_default()
    }
    pub fn expect(&self, expected: &[u8]) {
        assert!(
            common::wait_until(WAIT, || self.input() == expected),
            "raw PTY bytes: expected {expected:?}, actual {:?}",
            self.input()
        );
    }
    pub fn stable(&self, expected: &[u8]) {
        self.command(json!({"action":"barrier"}));
        assert!(
            !common::wait_until(Duration::from_millis(250), || self.input() != expected),
            "input escaped or duplicated: expected {expected:?}, actual {:?}",
            self.input()
        );
    }
    pub fn command(&self, value: Value) -> Value {
        assert!(!self.path("command").exists());
        assert!(!self.path("response").exists());
        atomic(&self.path("command"), &value);
        let reply = read_json(&self.path("response"));
        fs::remove_file(self.path("response")).unwrap();
        reply
    }
    pub fn emit(&self, bytes: &[u8]) {
        self.command(json!({"action":"emit","bytes":bytes}));
        let needle = bytes.to_vec();
        assert!(
            common::wait_until(WAIT, || common::pane_snapshot_on(
                &self.daemon.attach_socket,
                &self.agent
            )
            .windows(needle.len())
            .any(|w| w == needle)),
            "output must reach real master reader before next input: {bytes:?}"
        );
    }

    pub fn control_for(&self, generation: u64, operation: &str) -> Value {
        inbox::attach(
            &self.daemon,
            json!({"op":"e2e-composer-control","pane_id":PANE,"generation":generation,"operation":operation}),
        )
    }
    pub fn control(&self, operation: &str) -> Value {
        let reply = self.control_for(self.generation, operation);
        assert_eq!(
            reply["ok"], true,
            "Q35-20 requires generation-bound {operation}: {reply}"
        );
        reply
    }
    pub fn epoch(&self) -> (u64, u64, u64) {
        let r = self.control("epoch");
        let e = &r["composer_epoch"];
        (
            e["epoch"].as_u64().expect("epoch number"),
            e["observed"].as_u64().expect("observed number"),
            e["last_foreign"].as_u64().expect("foreign number"),
        )
    }
    /// Request a first probe against this generation's persistent production
    /// schedule. A not-due request leaves it unchanged; an admitted attempt
    /// reaches writer-thread admission, never a direct arm/fence publication.
    pub fn probe(&self) -> Value {
        self.control("probe-request")
    }
    pub fn held(&self) -> bool {
        self.control("fence-status")["composer_fence"]["held_ops"]
            .as_u64()
            .unwrap()
            > 0
    }
    pub fn wait_held(&self) {
        assert!(
            common::wait_until(WAIT, || self.held()),
            "person's keys must be accepted behind the live segment"
        );
    }
    pub fn wait_open(&self) {
        assert!(
            common::wait_until(
                WAIT,
                || self.control("fence-status")["composer_fence"]["fenced"] == false
            ),
            "previous segment must finish before the native-publication control"
        );
    }
    pub fn page(&self) -> Publication {
        let file = File::open(self.instance.join(format!("arm-{}", self.generation))).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        // SAFETY: read-only mapping of this fixture's installed page; aligned
        // atomic reads finish before the single munmap.
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let p = ArmReader::at(ptr.cast(), len).unwrap().read_publication();
            assert_eq!(libc::munmap(ptr, len), 0);
            p.expect("coherent arm publication")
        }
    }
    pub fn scope(&self) -> String {
        self.control("observer-status")["composer_observer"]["scope"]
            .as_str()
            .expect("observer scope")
            .into()
    }
    pub fn screen(&self) -> String {
        let mut parser = vt100::Parser::new(40.try_into().unwrap(), 140.try_into().unwrap(), 0);
        parser.process(&common::pane_snapshot_on(
            &self.daemon.attach_socket,
            &self.agent,
        ));
        parser.screen().contents()
    }
    pub fn pause_helper(&self) -> HelperPause {
        use std::os::unix::fs::OpenOptionsExt;
        let fifo = self.path("helper-fifo");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a new private path within this fixture's sandbox.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
            .unwrap();
        self.flag("pause-helper");
        HelperPause(writer)
    }
    pub fn replace(&mut self) {
        self.replace_observing_retirement(|_| {});
    }
    pub fn replace_observing_retirement(&mut self, retired: impl FnOnce(&Self)) {
        self.flag("editor-go");
        self.flag("helper-go");
        let stopped = inbox::attach(&self.daemon, json!({"op":"stop-agent","id":self.agent}));
        assert_eq!(stopped["ok"], true, "stop fixture generation: {stopped}");
        retired(self);
        self.spawn("successor", true);
    }
    pub fn snapshot(&self) -> Value {
        let r = inbox::attach(&self.daemon, json!({"op":"list-agents"}));
        r["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == self.agent)
            .unwrap()
            .clone()
    }
    pub fn enqueue(&self) -> String {
        self.enqueue_body("TEST-ONLY-WAITING-MAIL")
    }
    pub fn enqueue_body(&self, body: &str) -> String {
        let r = inbox::attach(
            &self.daemon,
            json!({"op":"post-message","pane_id":PANE,"expected_agent_id":self.agent,"subject":"segment admission","body":body,"origin":"seed"}),
        );
        assert_eq!(
            r["ok"], true,
            "post ordinary mail to exercise the live tick: {r}"
        );
        r["id"].as_str().expect("queued message id").into()
    }
    pub fn hook_event(&self, event: &str) {
        self.hook_event_as(event, self.kind);
    }
    pub fn hook_event_as(&self, event: &str, claimed_kind: &str) {
        let pid = read_json(&self.path("ready"))["pid"].as_u64().unwrap();
        let initial = fs::read(format!("/proc/{pid}/environ")).unwrap();
        let token = initial
            .split(|b| *b == 0)
            .find_map(|e| e.strip_prefix(b"DOT_AGENT_DECK_PANE_CAPABILITY="))
            .map(|v| std::str::from_utf8(v).unwrap())
            .expect("own native fixture capability");
        let session = if claimed_kind == "codex" {
            format!("{}-session", self.agent)
        } else {
            format!("segment-session-{}", self.generation)
        };
        let request = json!({"session_id":session,"pane_id":PANE,"agent_id":self.agent,"token":token,"deck_build":"","event_type":event,"agent_type":if claimed_kind=="codex" {"codex"} else {"claude_code"},"timestamp":chrono::Utc::now(),"cwd":self.daemon.home,"metadata":{}});
        let mut stream = inbox::connect(&self.daemon, &request).into_inner();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        use std::io::Read;
        let mut discarded = Vec::new();
        stream.read_to_end(&mut discarded).unwrap();
    }
}

pub struct HelperPause(File);
impl HelperPause {
    pub fn release(mut self) {
        self.0.write_all(b"x").unwrap();
    }
}
impl Drop for HelperPause {
    fn drop(&mut self) {
        let _ = self.0.write_all(b"x");
    }
}

/// A person's attach connection that, unlike [`common::AttachInput`], keeps
/// the input refusals the daemon sends it (`KIND_STREAM_REJECT` bodies), so a
/// scenario can see which notice its own write was answered with.
pub struct RefusalWatch {
    stream: std::os::unix::net::UnixStream,
    refusals: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl RefusalWatch {
    fn frame(&mut self, kind: u8, bytes: &[u8]) {
        self.stream.write_all(&[kind]).unwrap();
        self.stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        self.stream.write_all(bytes).unwrap();
        self.stream.flush().unwrap();
    }
    /// One `KIND_STREAM_IN` frame carrying `bytes`.
    pub fn write(&mut self, bytes: &str) {
        self.frame(
            dot_agent_deck::daemon_protocol::KIND_STREAM_IN,
            bytes.as_bytes(),
        );
    }
    pub fn refusals(&self) -> Vec<Vec<u8>> {
        self.refusals.lock().unwrap().clone()
    }
}
impl Drop for RefusalWatch {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Fixture {
    /// The daemon's log as written so far.
    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Diagnosis only (Q35-21): print the daemon log's probe and doorbell
    /// lines, so a red run says which check settled how and why a ring or
    /// a probe was or wasn't admitted. Asserts nothing.
    fn print_log(&self) {
        let text = fs::read_to_string(&self.log).unwrap_or_default();
        // A line repeated but for its timestamp is shown once, with a count,
        // so a tick's repeated refusal doesn't push the start out of view.
        let body = |l: &str| l.split_once(' ').map_or(l, |(_, rest)| rest).to_owned();
        let mut lines: Vec<String> = Vec::new();
        let mut repeats = 0usize;
        for l in text.lines().filter(|l| {
            l.contains("composer_probe")
                || l.contains("doorbell")
                || l.contains("codex")
                || l.contains("Received ")
                || l.contains("inbox")
        }) {
            if lines.last().is_some_and(|last| body(last) == body(l)) {
                repeats += 1;
                continue;
            }
            if repeats > 0 {
                lines.push(format!("    (the line above {repeats} more times)"));
                repeats = 0;
            }
            lines.push(l.to_owned());
        }
        if repeats > 0 {
            lines.push(format!("    (the line above {repeats} more times)"));
        }
        let skip = lines.len().saturating_sub(LOG_LINES_PRINTED);
        let mut out = format!(
            "---- fixture deck log ({}; {} lines after collapsing repeats, {} omitted from the start) ----\n",
            self.log.display(),
            lines.len(),
            skip
        );
        for line in &lines[skip..] {
            let mut cut = line.len().min(LOG_LINE_CHARS);
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push_str(&line[..cut]);
            if cut < line.len() {
                out.push_str(" …");
            }
            out.push('\n');
        }
        out.push_str("---- end of fixture deck log ----\n");
        eprint!("{}", common::redact_credentials_for_output(&out));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.print_log();
        }
        for suffix in [
            "native-stop",
            "wrapper-stop",
            "launch-go",
            "reparent-go",
            "editor-go",
            "editor-publish-go",
            "helper-go",
            "resume-go",
            "flush-go",
            "survivor-go",
        ] {
            let _ = fs::write(self.path(suffix), "");
        }
        for n in 1..=8 {
            let _ = fs::write(self.path(&format!("resume-go-{n}")), "");
        }
        let root = self.daemon.home.join("probe-gate");
        let _ = fs::write(root.join("scheduled.release"), "");
        // Release only this fixture's discovery barriers before StopAgent.
        let root = self.daemon.home.join("discovery-gate");
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                if let Some(generation) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.strip_suffix(".arrived"))
                {
                    let _ = fs::write(
                        entry.path().with_file_name(format!("{generation}.release")),
                        "",
                    );
                }
            }
        }
        let _ = common::attach_request_on(
            &self.daemon.attach_socket,
            &dot_agent_deck::daemon_protocol::AttachRequest::StopAgent {
                id: self.agent.clone(),
                keep_worktree: false,
            },
        );
    }
}

/// What the `codex app-server` stand-in answers to `hooks/list`: the deck's
/// own hooks, by the command the install wrote (as
/// `tests/e2e_inbox_codex_doorbell.rs` and `tests/e2e_codex_status.rs` answer).
/// The `result` of `tests/fixtures/codex-synthetic/codex`'s `hooks/list`
/// reply to `canned` (a [`codex_hook_list_response`]): `__CODEX_HOME__`
/// becomes `$CODEX_HOME`, and `__DECK_COMMAND__` the first hook command in
/// that home's `hooks.json`, taken still JSON-escaped from the first line
/// naming one, as the stand-in's `sed` takes it.
fn stand_in_hook_list(canned: &str) -> Option<Value> {
    let home = std::env::var("CODEX_HOME").ok()?;
    let hooks = fs::read_to_string(Path::new(&home).join("hooks.json")).unwrap_or_default();
    let command = hooks
        .lines()
        .find_map(|line| {
            let rest = &line[line.find("\"command\"")? + "\"command\"".len()..];
            let rest = rest.trim_start().strip_prefix(':')?.trim_start();
            let rest = rest.strip_prefix('"')?;
            Some(rest[..rest.rfind('"')?].to_owned())
        })
        .unwrap_or_default();
    let reply: Value = serde_json::from_str(
        &canned
            .replace("__CODEX_HOME__", &home)
            .replace("__DECK_COMMAND__", &command),
    )
    .ok()?;
    Some(reply["result"].clone())
}

fn codex_hook_list_response() -> String {
    let entry = |event: &str, camel: &str| {
        json!({
            "key": format!("__CODEX_HOME__/hooks.json:{event}:0:0"),
            "eventName": camel, "handlerType": "command", "matcher": null,
            "command": "__DECK_COMMAND__", "timeoutSec": 600, "statusMessage": null,
            "sourcePath": "__CODEX_HOME__/hooks.json", "source": "user", "pluginId": null,
            "displayOrder": 0, "enabled": true, "isManaged": false,
            "currentHash": format!("sha256:{event}"), "trustStatus": "untrusted"
        })
    };
    json!({"id": 2, "result": {"data": [{"cwd": "/workspace", "hooks": [
        entry("session_start", "sessionStart"),
        entry("user_prompt_submit", "userPromptSubmit"),
        entry("pre_tool_use", "preToolUse"),
        entry("post_tool_use", "postToolUse"),
        entry("permission_request", "permissionRequest"),
        entry("stop", "stop"),
    ], "warnings": [], "errors": []}]}})
    .to_string()
}

// The bridge changes only ancestry, not F2's parsing or key-loss mechanisms.
const BROKER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
base = pathlib.Path(os.environ['DAD_LIVE_SEGMENTS_BASE'])
request = base.with_suffix('.invoke')
temporary = base.with_suffix('.invoke.tmp')
temporary.write_text(json.dumps({'path':sys.argv[-1]}))
temporary.replace(request)
response = base.with_suffix('.invoked')
while not response.exists(): time.sleep(.005)
result = json.loads(response.read_text())
response.unlink()
sys.exit(result['exit'])
"#;

/// Called by a ctor so Codex's injected `-c` arguments never reach libtest.
/// Each process is a copied synthetic executable in its own daemon HOME.
pub fn native_entry() {
    let Some(base) = std::env::var_os(MODE) else {
        return;
    };
    let base = PathBuf::from(base);
    let kind = std::env::var("DAD_LIVE_SEGMENTS_KIND").unwrap();
    if std::env::args_os().skip(1).any(|arg| arg == "--version") {
        println!(
            "{}",
            if kind == "codex" {
                "0.164.0"
            } else {
                "2.1.310"
            }
        );
        std::process::exit(0);
    }
    if std::env::args_os().skip(1).any(|arg| arg == "app-server") {
        // The wrapper's ordinary hooks/list probe must not start another
        // composer or overwrite the pane child's readiness acknowledgement.
        // A Codex pane's spawn environment carries the `hooks/list` answer
        // part 3's fixture seeds trust from (Q129b item 0), and this answers
        // it as `tests/fixtures/codex-synthetic/codex` does, so a pane whose
        // `codex` resolves here is trusted as one whose `codex` resolves to
        // that stand-in. Without it, this synthetic server advertises no
        // hook key or hash to trust.
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if let Some(id) = request.get("id") {
                let result = if request["method"] == "hooks/list" {
                    std::env::var("CODEX_HOOK_LIST_RESPONSE")
                        .ok()
                        .and_then(|canned| stand_in_hook_list(&canned))
                        .unwrap_or_else(|| json!({"hooks":[]}))
                } else {
                    json!({})
                };
                println!("{}", json!({"jsonrpc":"2.0","id":id,"result":result}));
                std::io::stdout().flush().unwrap();
                if request["method"] == "hooks/list" {
                    break;
                }
            }
        }
        std::process::exit(0);
    }
    let exe = std::env::current_exe().unwrap();
    let helper = PathBuf::from(std::env::var_os("VISUAL").unwrap());
    let generation = std::env::var("DOT_AGENT_DECK_EDITOR_GEN")
        .unwrap_or_else(|_| "0".into())
        .parse::<u64>()
        .unwrap();
    fs::create_dir_all(std::env::var_os("TMPDIR").unwrap()).unwrap();
    // The common ctor removes global endpoint names. Restore just the sandbox
    // values supplied in this child's own initial exec environment to its
    // helper children; never inspect another pane or print the capability.
    let initial = fs::read("/proc/self/environ").unwrap();
    let env: Vec<(String, String)> = initial
        .split(|b| *b == 0)
        .filter_map(|e| {
            let e = std::str::from_utf8(e).ok()?;
            let (k, v) = e.split_once('=')?;
            k.starts_with("DOT_AGENT_DECK_")
                .then(|| (k.to_owned(), v.to_owned()))
        })
        .collect();
    if std::env::var_os("DAD_LIVE_SEGMENTS_FAMILY_PARENT").is_some() {
        // This second copy remains the helper's real parent. Its F1 is
        // the agent's, but its PID is not A, so attribution is Unknown.
        let status = Command::new(&helper)
            .envs(env)
            .args(std::env::args_os().skip(2))
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(127));
    }
    let mut composer = Command::new("python3")
        .arg(exe.parent().unwrap().join("composer.py"))
        .arg(&base)
        .arg(common::contained_deck_binary())
        .arg(if kind == "codex" {
            "0.164.0"
        } else {
            "2.1.310"
        })
        .envs(env.clone())
        .env("DOT_AGENT_DECK_TEST_COMPOSER_EXTERNAL_EDIT", "1")
        .env("VISUAL", exe.parent().unwrap().join("broker"))
        .spawn()
        .unwrap();
    atomic(
        &base.with_extension("ready"),
        &json!({"pid":std::process::id(),"exe":exe,"generation":generation,"instance":helper.parent().unwrap(),
        "editors":{
            "VISUAL":std::env::var("VISUAL").ok(),
            "EDITOR":std::env::var("EDITOR").ok(),
            "DOT_AGENT_DECK_EDITOR_GEN":std::env::var("DOT_AGENT_DECK_EDITOR_GEN").ok(),
        }}),
    );
    let mut running: Option<Child> = None;
    let mut finished: Option<i32> = None;
    let mut invocation = 0;
    let mut parallel: Option<Child> = None;
    loop {
        if base.with_extension("native-stop").exists() {
            let _ = composer.kill();
            let _ = composer.wait();
            break;
        }
        if let Some(child) = parallel.as_mut()
            && let Some(status) = child.try_wait().unwrap()
        {
            atomic(
                &base.with_extension("parallel-result"),
                &json!({"code":status.code(),"signal":status.signal()}),
            );
            parallel = None;
        }
        if let Some(child) = running.as_mut()
            && let Some(status) = child.try_wait().unwrap()
        {
            let pid = child.id();
            finished = Some(status.code().unwrap_or(127));
            running = None;
            fs::write(base.with_extension("helper-exited"), "").unwrap();
            atomic(
                &base.with_extension(format!("helper-result-{invocation}")),
                &json!({"pid":pid,"code":status.code(),"signal":status.signal()}),
            );
            fs::write(
                base.with_extension(format!("helper-exited-{invocation}")),
                "",
            )
            .unwrap();
        }
        if let Some(exit) = finished
            && (!base.with_extension("hold-return").exists()
                || base.with_extension("resume-go").exists()
                || base
                    .with_extension(format!("resume-go-{invocation}"))
                    .exists())
        {
            atomic(&base.with_extension("invoked"), &json!({"exit":exit}));
            finished = None;
        }
        if let Ok(data) = fs::read(base.with_extension("invoke")) {
            assert!(running.is_none() && finished.is_none());
            fs::remove_file(base.with_extension("invoke")).unwrap();
            let req: Value = serde_json::from_slice(&data).unwrap();
            invocation += 1;
            let target = if base.with_extension("lost-ipc").exists() {
                // Genuine unreported external editor: deliberately omit the
                // installed helper; output fallback must notice its pair.
                exe.parent().unwrap().join("editor")
            } else {
                helper.clone()
            };
            let mut command = match fs::read_to_string(base.with_extension("helper-parent"))
                .unwrap_or_default()
                .as_str()
            {
                "tool" => {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "\"$@\"; code=$?; exit \"$code\"", "fixture-tool"]);
                    command.arg(&target);
                    command
                }
                "unknown" => {
                    let mut command = Command::new(&exe);
                    command
                        .arg("--live-segments-parent")
                        .env("DAD_LIVE_SEGMENTS_FAMILY_PARENT", "1");
                    command
                }
                _ => Command::new(&target),
            };
            command.envs(env.clone());
            for (suffix, variable) in [
                ("helper-report-pad", "DOT_AGENT_DECK_E2E_HELPER_PAD"),
                ("helper-report-sndbuf", "DOT_AGENT_DECK_E2E_HELPER_SNDBUF"),
            ] {
                if let Ok(value) = fs::read_to_string(base.with_extension(suffix)) {
                    command.env(variable, value.trim());
                }
            }
            if base.with_extension("lost-ipc").exists() {
                command.arg("fast").arg(&base);
            }
            if base.with_extension("wait-unknown").exists()
                || base
                    .with_extension(format!("wait-unknown-{invocation}"))
                    .exists()
            {
                command.env("DOT_AGENT_DECK_E2E_HELPER_WAIT_ERRNO", "10");
            }
            if base.with_extension("spawn-failed").exists()
                || base
                    .with_extension(format!("spawn-failed-{invocation}"))
                    .exists()
            {
                command.env("DOT_AGENT_DECK_E2E_HELPER_SPAWN_ERRNO", "2");
            }
            if base.with_extension("pause-helper").exists() {
                command
                    .env(
                        "DOT_AGENT_DECK_E2E_HELPER_PAUSE",
                        base.with_extension("helper-fifo"),
                    )
                    .env(
                        "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT",
                        fs::read_to_string(base.with_extension("helper-pause-at"))
                            .unwrap_or_else(|_| "start".into()),
                    );
            }
            command.env(
                "DOT_AGENT_DECK_E2E_HELPER_TRACE",
                base.with_extension("helper-trace"),
            );
            let child = command.arg(req["path"].as_str().unwrap()).spawn().unwrap();
            atomic(
                &base.with_extension("helper-started"),
                &json!({"pid":child.id(),"path":req["path"]}),
            );
            running = Some(child);
            fs::write(
                base.with_extension(format!("helper-started-{invocation}")),
                "",
            )
            .unwrap();
        }
        if let Ok(data) = fs::read(base.with_extension("command")) {
            fs::remove_file(base.with_extension("command")).unwrap();
            let cmd: Value = serde_json::from_slice(&data).unwrap();
            if cmd["action"] == "fetch" {
                // An independent ordinary CLI consumer, with this sandbox
                // pane's own environment; no input goes to its composer.
                let output = Command::new(common::contained_deck_binary())
                    .args(["inbox", "fetch"])
                    .envs(env.clone())
                    .output()
                    .unwrap();
                atomic(
                    &base.with_extension("response"),
                    &json!({
                        "ok":output.status.success(),
                        "stdout":String::from_utf8(output.stdout).unwrap(),
                        "stderr":String::from_utf8(output.stderr).unwrap(),
                    }),
                );
                continue;
            }
            let mut reply = json!({"ok":true});
            if cmd["action"] == "emit" {
                let bytes: Vec<u8> = serde_json::from_value(cmd["bytes"].clone()).unwrap();
                std::io::stdout().write_all(&bytes).unwrap();
                std::io::stdout().flush().unwrap();
            }
            if cmd["action"] == "input-pending" {
                // The composer is blocked in its editor broker until the
                // fixture releases the return. Inspect its actual tty queue
                // without consuming keys or allowing the return through.
                // Canonical-mode line discipline counts only complete lines;
                // callers expecting an exact count end their typing in '\n'.
                let mut pending: libc::c_int = 0;
                // SAFETY: stdin is this sandbox agent's slave terminal and
                // FIONREAD writes one c_int to the valid output pointer.
                assert_eq!(
                    unsafe { libc::ioctl(0, libc::FIONREAD, &mut pending) },
                    0,
                    "pending tty bytes: {}",
                    std::io::Error::last_os_error()
                );
                reply["pending_bytes"] = json!(pending);
            }
            if cmd["action"] == "invoke" {
                // This is real helper work by the native fixture, not an
                // injected daemon record. The path belongs to its HOME.
                let path = PathBuf::from(cmd["path"].as_str().unwrap());
                assert!(path.starts_with(std::env::var_os("HOME").unwrap()));
                atomic(&base.with_extension("invoke"), &json!({"path":path}));
            }
            if cmd["action"] == "parallel-helper" {
                // Keep the first real helper alive while a second helper
                // reaches the observer's actual record cap. Its parent is
                // this native stand-in, or an explicit tool shell.
                assert!(parallel.is_none());
                let path = PathBuf::from(cmd["path"].as_str().unwrap());
                assert!(path.starts_with(std::env::var_os("HOME").unwrap()));
                let mut command = if cmd["tool"] == true {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "\"$@\"; code=$?; exit \"$code\"", "parallel-tool"]);
                    command.arg(&helper);
                    command
                } else {
                    Command::new(&helper)
                };
                command.envs(env.clone()).env(
                    "DOT_AGENT_DECK_E2E_HELPER_TRACE",
                    base.with_extension("parallel-trace"),
                );
                if cmd["pause"] == true {
                    command
                        .env(
                            "DOT_AGENT_DECK_E2E_HELPER_PAUSE",
                            base.with_extension("helper-fifo"),
                        )
                        .env("DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT", "start");
                }
                let child = command.arg(path).spawn().unwrap();
                reply["pid"] = json!(child.id());
                parallel = Some(child);
            }
            atomic(&base.with_extension("response"), &reply);
        }
        if composer.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Some(mut child) = running {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(mut child) = parallel {
        let _ = child.kill();
        let _ = child.wait();
    }
    fs::write(base.with_extension("native-stopped"), "").unwrap();
    std::process::exit(0);
}
