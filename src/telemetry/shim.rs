//! The `PATH` shim that switches on Codex's and Devin's own telemetry at the
//! moment they really start, however they are started.
//!
//! With telemetry on, the daemon writes a small `sh` script per agent —
//! `codex` and `devin` — into a directory of its own ([`install`]) and puts
//! that directory first on the `PATH` of every agent it starts. A pane that
//! runs `codex`, a script that runs it by name (`devbox run codex-big`), or
//! another agent in the pane that runs it, all reach the script, which
//! `exec`s the deck's hidden entrypoint ([`SUBCOMMAND`], [`run_if_invoked`]).
//! That entrypoint:
//!
//! 1. finds the real program: the first executable file of that name on
//!    `PATH` that is not a deck shim script, in the shim directory or any
//!    other ([`resolve`]) — none is the shell's `command not found` and
//!    status 127;
//! 2. decides from its own environment — the agent's real one, after any
//!    script or launcher — what the agent needs ([`super::launch`]), unless
//!    this program was already shimmed further up its process tree
//!    ([`GUARD_ENV`]), or the real program is a shell script of the user's
//!    that may change that environment before the agent reads it
//!    ([`launcher_is_certain`]), or the environment holds a value the
//!    decision cannot read exactly;
//! 3. replaces itself with the real program, those arguments in front of the
//!    user's own, so signals, the exit status and the terminal are the
//!    program's own.
//!
//! Anything that goes wrong in step 2 runs the program unchanged: the shim
//! never stops an agent from starting. The one refusal is a process tree that
//! has passed through the shim [`MAX_HOPS`] times, which would otherwise keep
//! re-entering it. A script that calls the agent by an absolute path,
//! replaces `PATH`, or puts a directory holding the real agent in front of
//! the shim's, does not reach the shim. There is no shim on Windows.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::config::EnvMap;

/// The deck's hidden subcommand the shim scripts run. Never in the help: it
/// is handled before the command line is parsed.
pub const SUBCOMMAND: &str = "__telemetry-shim";

/// The programs the shim stands in front of.
pub const PROGRAMS: [&str; 2] = ["codex", "devin"];

/// The shim directory's name inside the deck's telemetry directory.
pub const DIR_NAME: &str = "bin";

/// The deck's run id, in the environment of an agent the deck started with
/// the shim on `PATH`: what a Devin's per-run configuration is named for, so
/// the daemon removes it when the run ends.
pub const RUN_ID_ENV: &str = "DOT_AGENT_DECK_TELEMETRY_RUN_ID";

/// The programs already shimmed in this process tree, `:`-separated: set on
/// the program the shim runs, so a program that runs itself again by name
/// (a wrapper of the user's called `codex`, say) gets the deck's additions
/// once.
pub const GUARD_ENV: &str = "DOT_AGENT_DECK_TELEMETRY_SHIMMED";

/// How many times one process tree may pass through the shim — the entries
/// in [`GUARD_ENV`] — before the shim refuses to run the program again: far
/// more than agents starting agents need, and a bound on anything that keeps
/// bringing a launch back to the shim.
pub const MAX_HOPS: usize = 16;

/// The interpreters, by file name, of the scripts the shim does not add to:
/// shells, in which a user's wrapper named for the agent may set its
/// configuration home before running it.
const SHELLS: [&str; 12] = [
    "sh", "bash", "dash", "zsh", "ksh", "mksh", "fish", "ash", "busybox", "csh", "tcsh", "yash",
];

/// How much of a candidate program the shim reads to tell what it is.
const HEAD_LEN: u64 = 16 * 1024;

/// Write the shim scripts into `config_dir`'s [`DIR_NAME`], each running the
/// deck at `deck`, and return that directory: owner-only, as are the scripts.
/// Each script is written beside its final name and renamed onto it, so an
/// agent starting meanwhile runs the old one or the new one, never half of
/// one. Refused for a directory `PATH` cannot carry (one with a `:`).
#[cfg(unix)]
pub fn install(config_dir: &Path, deck: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = config_dir.join(DIR_NAME);
    let Some(dir_str) = dir.to_str().filter(|d| !d.contains(':')) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the shim directory's path cannot go on PATH",
        ));
    };
    crate::platform::fsperm::ensure_owner_only_dir(config_dir)?;
    crate::platform::fsperm::ensure_owner_only_dir(&dir)?;
    for name in PROGRAMS {
        let staged = dir.join(format!(".{name}.{}", std::process::id()));
        let _ = std::fs::remove_file(&staged);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&staged)?;
        file.write_all(script(deck, dir_str, name).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staged, dir.join(name))?;
    }
    Ok(dir)
}

/// The shim script for `name`: run the deck at `deck` as the shim in `dir`,
/// with every argument passed on as it came.
fn script(deck: &str, dir: &str, name: &str) -> String {
    let quote = super::launch::shell_quote;
    format!(
        "#!/bin/sh\n\
         # Written by dot-agent-deck: switches on this agent's own telemetry, then runs it.\n\
         exec {} {SUBCOMMAND} {name} {} -- \"$@\"\n",
        quote(deck),
        quote(dir),
    )
}

/// The deck's entrypoint when this process is a shim — its first argument is
/// [`SUBCOMMAND`] — run before anything else reads the command line, which
/// holds the agent's arguments, not the deck's. `None` for any other
/// invocation. Returns only when the program could not be run.
pub fn run_if_invoked() -> Option<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(SUBCOMMAND)) {
        return None;
    }
    let (Some(name), Some(dir), Some(dashes)) = (args.next(), args.next(), args.next()) else {
        eprintln!("dot-agent-deck: {SUBCOMMAND}: expected <program> <dir> -- <args>");
        return Some(ExitCode::from(2));
    };
    let name = name.to_string_lossy().into_owned();
    if dashes != "--" || !PROGRAMS.contains(&name.as_str()) {
        eprintln!("dot-agent-deck: {SUBCOMMAND}: expected <program> <dir> -- <args>");
        return Some(ExitCode::from(2));
    }
    Some(run(&name, Path::new(&dir), args.collect()))
}

/// What the shim runs: the real program, and its arguments.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    program: PathBuf,
    args: Vec<OsString>,
}

#[cfg(unix)]
fn run(name: &str, dir: &Path, args: Vec<OsString>) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let path = std::env::var_os("PATH");
    let guard = std::env::var_os(GUARD_ENV);
    let env = super::config::process_env();
    let plan = match plan(
        name,
        dir,
        args,
        path.as_deref(),
        guard.as_deref(),
        exact_env().then_some(&env),
        std::process::id(),
    ) {
        Ok(plan) => plan,
        Err(Refused::NotFound) => {
            eprintln!("{name}: command not found");
            return ExitCode::from(127);
        }
        Err(Refused::TooManyHops) => {
            eprintln!(
                "{name}: not started: dot-agent-deck's telemetry shim was reached {MAX_HOPS} times in this process tree, so something keeps running it again instead of {name}; check the {name} entries on PATH"
            );
            return ExitCode::from(126);
        }
    };
    let e = std::process::Command::new(&plan.program)
        .arg0(name)
        .args(&plan.args)
        .env(GUARD_ENV, guarded(guard.as_deref(), name))
        .exec();
    eprintln!("{name}: {e}");
    ExitCode::from(if e.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    })
}

#[cfg(not(unix))]
fn run(name: &str, _dir: &Path, _args: Vec<OsString>) -> ExitCode {
    eprintln!("{name}: the telemetry shim runs on Unix only");
    ExitCode::from(127)
}

/// Whether the decision can read every variable it uses exactly as the agent
/// will: [`super::config::process_env`] leaves out a value that is not UTF-8,
/// which for a configuration home or an `OTEL_*` variable would have the
/// decision read another configuration than the agent's.
#[cfg(unix)]
fn exact_env() -> bool {
    std::env::vars_os().all(|(key, value)| {
        value.to_str().is_some()
            || !key.to_str().is_some_and(|k| {
                k.starts_with("OTEL_") || super::launch::CONFIG_HOME_ENV_KEYS.contains(&k)
            })
    })
}

/// Why the shim runs no program.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
    /// No real program of that name on `PATH`.
    NotFound,
    /// The process tree has passed through the shim [`MAX_HOPS`] times.
    TooManyHops,
}

/// The shim's plan for `name` called with `args`, with `PATH` = `path`,
/// [`GUARD_ENV`] = `guard` and the environment `env` — `None` when it cannot
/// be read exactly, which adds nothing; the program will have process id
/// `pid`.
fn plan(
    name: &str,
    dir: &Path,
    args: Vec<OsString>,
    path: Option<&OsStr>,
    guard: Option<&OsStr>,
    env: Option<&EnvMap>,
    pid: u32,
) -> Result<Plan, Refused> {
    let shimmed = guard.map(|g| g.to_string_lossy().into_owned());
    let shimmed: Vec<&str> = shimmed
        .as_deref()
        .map(|g| g.split(':').filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    if shimmed.len() >= MAX_HOPS {
        return Err(Refused::TooManyHops);
    }
    let program = resolve(name, path, dir).ok_or(Refused::NotFound)?;
    let added = match env {
        Some(env) if !shimmed.contains(&name) && launcher_is_certain(&program) => {
            additions(name, dir, &args, env, pid)
        }
        _ => Vec::new(),
    };
    Ok(Plan {
        program,
        args: added.into_iter().map(OsString::from).chain(args).collect(),
    })
}

/// The arguments the deck puts in front of `name`'s own `args`, or none —
/// whatever goes wrong deciding them, a panic included, is "none".
fn additions(name: &str, dir: &Path, args: &[OsString], env: &EnvMap, pid: u32) -> Vec<String> {
    let decide = || match name {
        "codex" => super::launch::codex_args(args, env),
        "devin" => {
            let (Some(run_id), Some(config_dir)) =
                (super::config::non_empty(env, RUN_ID_ENV), dir.parent())
            else {
                return Vec::new();
            };
            let run = super::launch::DevinRun {
                run_id,
                config_dir,
                pid,
            };
            super::launch::devin_args(args, env, &run).unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let decided = std::panic::catch_unwind(std::panic::AssertUnwindSafe(decide));
    std::panic::set_hook(quiet);
    decided.unwrap_or_default()
}

/// `guard` with `name` added.
fn guarded(guard: Option<&OsStr>, name: &str) -> OsString {
    match guard.filter(|g| !g.is_empty()) {
        Some(g) => {
            let mut out = g.to_os_string();
            out.push(":");
            out.push(name);
            out
        }
        None => OsString::from(name),
    }
}

/// The program `name` as a shell would find it on `path`, skipping the
/// deck's shims: a file that, symlinks resolved, is in the shim directory
/// `dir` — so an entry that is the directory, or a symlink to it, or a link
/// to one of its scripts, is passed over — and any file that reads as a shim
/// script ([`is_shim_script`]), so another deck's shim directory, or a copy
/// or a hard link of a script, is passed over too. An empty entry is the
/// current directory, as for the shell.
fn resolve(name: &str, path: Option<&OsStr>, dir: &Path) -> Option<PathBuf> {
    let shim = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    std::env::split_paths(path?)
        .map(|entry| {
            if entry.as_os_str().is_empty() {
                PathBuf::from(".").join(name)
            } else {
                entry.join(name)
            }
        })
        .find(|candidate| {
            std::fs::canonicalize(candidate).is_ok_and(|c| c.parent() != Some(shim.as_path()))
                && is_executable_file(candidate)
                && !head(candidate).is_some_and(|h| is_shim_script(&h))
        })
}

/// The first [`HEAD_LEN`] bytes of the file at `path`, or `None` when it
/// cannot be read.
fn head(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(HEAD_LEN)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// Whether `head`, the start of a file, is a shim script as [`script`] writes
/// them, whichever deck and directory it names.
fn is_shim_script(head: &[u8]) -> bool {
    let needle = format!(" {SUBCOMMAND} ");
    head.starts_with(b"#!")
        && head.split(|b| *b == b'\n').any(|line| {
            line.starts_with(b"exec ") && line.windows(needle.len()).any(|w| w == needle.as_bytes())
        })
}

/// Whether the shim can be sure that `program`, the real program it is about
/// to run, is the agent, not a user's script that may set the agent's
/// configuration home, or anything else the decision reads, before running
/// it — in which case the decision would have read the wrong configuration.
/// Sure of a native executable and of a script for an interpreter that is not
/// a shell (Codex's npm launcher is a Node script); not of a shell script,
/// unless it is one Nix generated ([`is_nix_store_path`]), nor of a file it
/// cannot read or tell apart.
fn launcher_is_certain(program: &Path) -> bool {
    let (Some(head), Ok(canonical)) = (head(program), std::fs::canonicalize(program)) else {
        return false;
    };
    match launcher_kind(&head) {
        Some(Launcher::Native | Launcher::Interpreted) => true,
        Some(Launcher::Shell) => is_nix_store_path(&canonical),
        None => false,
    }
}

/// What a program is, from `head`, the start of its file.
#[derive(Debug, PartialEq, Eq)]
enum Launcher {
    /// An ELF or Mach-O executable.
    Native,
    /// A script whose interpreter is a shell ([`SHELLS`]).
    Shell,
    /// A script for another interpreter.
    Interpreted,
}

/// What `head` is the start of, or `None` when it cannot tell: no `#!` line it
/// can read, or `env` given anything but a program and `-S`, or a quoted one
/// — a variable it sets is a change the decision did not see.
fn launcher_kind(head: &[u8]) -> Option<Launcher> {
    const MAGICS: [[u8; 4]; 6] = [
        [0x7f, b'E', b'L', b'F'],
        [0xfe, 0xed, 0xfa, 0xce],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
    ];
    if MAGICS.iter().any(|m| head.starts_with(m)) {
        return Some(Launcher::Native);
    }
    let line = head.strip_prefix(b"#!")?;
    let line = std::str::from_utf8(line.split(|b| *b == b'\n').next()?).ok()?;
    let mut words = line.split_whitespace();
    let base = |word: &str| word.rsplit('/').next().unwrap_or(word).to_string();
    let mut interpreter = base(words.next()?);
    if interpreter == "env" {
        let mut words = words.peekable();
        if let Some(split) = words.peek().and_then(|w| w.strip_prefix("-S")) {
            let split = split.to_string();
            words.next();
            if !split.is_empty() {
                interpreter = split;
            } else {
                interpreter = words.next()?.to_string();
            }
        } else {
            interpreter = words.next()?.to_string();
        }
        if interpreter.starts_with('-') || interpreter.contains(['=', '"', '\'', '\\']) {
            return None;
        }
        interpreter = base(&interpreter);
    }
    if interpreter.is_empty() {
        return None;
    }
    Some(if SHELLS.contains(&interpreter.as_str()) {
        Launcher::Shell
    } else {
        Launcher::Interpreted
    })
}

/// Whether `canonical`, a program's path with symlinks resolved, is in the Nix
/// store, whose scripts Nix generated (devbox's launchers among them) rather
/// than the user wrote.
fn is_nix_store_path(canonical: &Path) -> bool {
    canonical.starts_with("/nix/store/")
}

/// Whether `path` is, symlinks followed, a regular file this process may
/// execute.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a NUL-terminated path that outlives the call.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const RUN: &str = "0123456789abcdef0123456789abcdef";

    /// A stand-in for a real agent: a Node script, as npm installs Codex.
    fn executable(path: &Path) {
        crate::test_isolation::write_script(path, "#!/usr/bin/env node\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// A telemetry directory holding a shim directory with both scripts, and
    /// a directory with the real programs.
    struct Fixture {
        _root: tempfile::TempDir,
        telemetry: PathBuf,
        shim: PathBuf,
        real: PathBuf,
        home: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let telemetry = root.path().join("telemetry");
        let shim = install(&telemetry, "/opt/deck/dot-agent-deck").unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        for name in PROGRAMS {
            executable(&real.join(name));
        }
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        Fixture {
            telemetry,
            shim,
            real,
            home,
            _root: root,
        }
    }

    impl Fixture {
        fn path(&self) -> OsString {
            path_of(&[&self.shim, &self.real])
        }

        fn env(&self, pairs: &[(&str, &str)]) -> EnvMap {
            let mut all = vec![
                ("HOME", self.home.to_str().unwrap()),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                (RUN_ID_ENV, RUN),
            ];
            all.extend_from_slice(pairs);
            env(&all)
        }

        fn plan(&self, name: &str, args: &[&str], env: &EnvMap) -> Result<Plan, Refused> {
            plan(
                name,
                &self.shim,
                os(args),
                Some(&self.path()),
                None,
                Some(env),
                42,
            )
        }
    }

    #[test]
    fn the_shim_directory_and_its_scripts_are_owner_only() {
        let f = fixture();
        assert_eq!(f.shim, f.telemetry.join(DIR_NAME));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&f.shim), 0o700);
        for name in PROGRAMS {
            assert_eq!(mode(&f.shim.join(name)), 0o700, "{name}");
        }
        assert_eq!(
            std::fs::read_dir(&f.shim).unwrap().count(),
            PROGRAMS.len(),
            "no staged file is left behind"
        );
        // A second daemon start rewrites them in place.
        install(&f.telemetry, "/other/deck").unwrap();
        let text = std::fs::read_to_string(f.shim.join("codex")).unwrap();
        assert!(
            text.contains("exec '/other/deck' __telemetry-shim codex '"),
            "{text}"
        );
        let colon = f.telemetry.parent().unwrap().join("a:b");
        assert!(install(&colon, "/d").is_err(), "PATH cannot carry it");
        assert!(!colon.exists());
    }

    /// The script hands every argument to the deck exactly as it came: run it
    /// with a "deck" that prints its argv.
    #[test]
    fn the_script_passes_the_arguments_through_untouched() {
        let root = tempfile::tempdir().unwrap();
        let deck = root.path().join("it's a deck");
        crate::test_isolation::write_script(
            &deck,
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&deck, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = install(&root.path().join("t"), deck.to_str().unwrap()).unwrap();
        let out = std::process::Command::new(dir.join("codex"))
            .envs(crate::test_isolation::unreachable_endpoints())
            .args(["a b", "", "$HOME", "--", "'q'"])
            .output()
            .unwrap();
        let lines: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            lines,
            [
                SUBCOMMAND,
                "codex",
                dir.to_str().unwrap(),
                "--",
                "a b",
                "",
                "$HOME",
                "--",
                "'q'"
            ]
        );
    }

    #[test]
    fn resolution_skips_the_shim_directory() {
        let f = fixture();
        assert_eq!(
            resolve("codex", Some(&f.path()), &f.shim),
            Some(f.real.join("codex"))
        );
        // Through a symlink to the shim directory, and a link to a script.
        let root = f.telemetry.parent().unwrap();
        let linked_dir = root.join("linked");
        std::os::unix::fs::symlink(&f.shim, &linked_dir).unwrap();
        let linked_file = root.join("linked-file");
        std::fs::create_dir(&linked_file).unwrap();
        std::os::unix::fs::symlink(f.shim.join("codex"), linked_file.join("codex")).unwrap();
        let path = path_of(&[&linked_dir, &linked_file, &f.shim, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
    }

    #[test]
    fn resolution_takes_the_first_executable_file_or_nothing() {
        let f = fixture();
        let root = f.telemetry.parent().unwrap();
        let plain = root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("codex"), "not executable").unwrap();
        let dir = root.join("dir");
        std::fs::create_dir_all(dir.join("codex")).unwrap();
        let earlier = root.join("earlier");
        std::fs::create_dir(&earlier).unwrap();
        executable(&earlier.join("codex"));
        let path = path_of(&[&f.shim, &plain, &dir, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
        let path = path_of(&[&f.shim, &earlier, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(earlier.join("codex"))
        );
        assert_eq!(
            resolve("codex", Some(&path_of(&[&f.shim, &plain])), &f.shim),
            None
        );
        assert_eq!(resolve("codex", None, &f.shim), None);
    }

    #[test]
    fn a_missing_program_has_no_plan() {
        let f = fixture();
        let only_shim = path_of(&[&f.shim]);
        assert_eq!(
            plan(
                "codex",
                &f.shim,
                Vec::new(),
                Some(&only_shim),
                None,
                Some(&f.env(&[])),
                1
            ),
            Err(Refused::NotFound)
        );
        assert!(
            f.plan("codex", &[], &f.env(&[]))
                .is_ok_and(|p| p.program == f.real.join("codex"))
        );
    }

    #[test]
    fn codex_gets_its_exporters_in_front_of_its_own_arguments() {
        let f = fixture();
        let plan = f
            .plan("codex", &["exec", "--model", "m"], &f.env(&[]))
            .unwrap();
        assert_eq!(plan.program, f.real.join("codex"));
        let args: Vec<_> = plan.args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("otel.trace_exporter="), "{args:?}");
        assert_eq!(args[2], "-c");
        assert!(args[3].starts_with("otel.metrics_exporter="), "{args:?}");
        assert_eq!(args[4..], ["exec", "--model", "m"]);
    }

    /// Every way the decision declines runs the program with its own
    /// arguments, unchanged.
    #[test]
    fn a_declined_launch_runs_the_program_unchanged() {
        let f = fixture();
        let codex_home = f.home.join("alt");
        std::fs::create_dir(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            "[otel]\nexporter = \"none\"\n",
        )
        .unwrap();
        let args = ["--yolo", "a b"];
        let unchanged = |name: &str, plan: Result<Plan, Refused>| {
            assert_eq!(
                plan,
                Ok(Plan {
                    program: f.real.join(name),
                    args: os(&args),
                })
            );
        };
        for pairs in [
            &[("OTEL_SDK_DISABLED", "true")][..],
            &[
                ("OTEL_TRACES_EXPORTER", "none"),
                ("OTEL_METRICS_EXPORTER", "none"),
            ][..],
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "")][..],
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://u:p@c.example")][..],
            &[("CODEX_HOME", codex_home.to_str().unwrap())][..],
        ] {
            unchanged("codex", f.plan("codex", &args, &f.env(pairs)));
        }
        for pairs in [
            &[("OTEL_SDK_DISABLED", "true")][..],
            &[(RUN_ID_ENV, "")][..],
            &[(RUN_ID_ENV, "../../etc")][..],
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c?token=1")][..],
        ] {
            unchanged("devin", f.plan("devin", &args, &f.env(pairs)));
        }
        // The guard: this program was shimmed further up its process tree.
        let guard = OsString::from("devin:codex");
        assert_eq!(
            plan(
                "codex",
                &f.shim,
                os(&args),
                Some(&f.path()),
                Some(&guard),
                Some(&f.env(&[])),
                1
            ),
            Ok(Plan {
                program: f.real.join("codex"),
                args: os(&args),
            })
        );
        // A failure inside the decision: Devin's file cannot be written.
        std::fs::write(f.telemetry.join(format!("devin-{RUN}-42.json")), "taken").unwrap();
        unchanged("devin", f.plan("devin", &args, &f.env(&[])));
    }

    #[test]
    fn the_guard_names_only_what_was_shimmed() {
        let f = fixture();
        let codex_shimmed = OsString::from("codex");
        let devin = plan(
            "devin",
            &f.shim,
            Vec::new(),
            Some(&f.path()),
            Some(&codex_shimmed),
            Some(&f.env(&[])),
            1,
        )
        .unwrap();
        assert_eq!(devin.args[0], "--config", "a Devin run by Codex is shimmed");
        assert_eq!(guarded(None, "codex"), "codex");
        assert_eq!(guarded(Some(OsStr::new("")), "codex"), "codex");
        assert_eq!(guarded(Some(&codex_shimmed), "devin"), "codex:devin");
    }

    #[test]
    fn devin_gets_a_config_named_for_the_run_and_the_process() {
        let f = fixture();
        let plan = f.plan("devin", &["--yolo"], &f.env(&[])).unwrap();
        let config = f.telemetry.join(format!("devin-{RUN}-42.json"));
        assert_eq!(
            plan.args,
            [
                OsString::from("--config"),
                config.clone().into_os_string(),
                OsString::from("--yolo")
            ]
        );
        assert!(config.is_file());
        super::super::launch::remove_run_config_files(&f.telemetry, RUN);
        assert!(!config.exists(), "the daemon removes it with the run");
        assert!(f.shim.join("devin").exists(), "and leaves the shim alone");
    }
    /// Two deck installations on one `PATH`, and a shim script copied or
    /// hard-linked outside its directory: each is passed over, so the real
    /// program is found and nothing runs the shim again.
    #[test]
    fn resolution_skips_every_decks_shim_scripts() {
        let f = fixture();
        let root = f.telemetry.parent().unwrap();
        let other = install(&root.join("other-telemetry"), "/other/deck").unwrap();
        let path = path_of(&[&f.shim, &other, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
        assert_eq!(
            resolve("codex", Some(&path), &other),
            Some(f.real.join("codex")),
            "and from the other installation's side"
        );
        let copied = root.join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::copy(f.shim.join("codex"), copied.join("codex")).unwrap();
        let linked = root.join("hard-linked");
        std::fs::create_dir(&linked).unwrap();
        std::fs::hard_link(f.shim.join("codex"), linked.join("codex")).unwrap();
        let path = path_of(&[&copied, &linked, &f.shim, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
        assert!(is_shim_script(script("/d", "/x", "devin").as_bytes()));
        assert!(!is_shim_script(b"#!/bin/sh\nexec codex \"$@\"\n"));
    }

    /// A process tree that keeps coming back to the shim is refused once it
    /// has passed through it [`MAX_HOPS`] times, rather than run again.
    #[test]
    fn the_shim_refuses_a_launch_that_keeps_coming_back() {
        let f = fixture();
        let hops = |n: usize| OsString::from(vec!["codex"; n].join(":"));
        let run = |guard: &OsString| {
            plan(
                "codex",
                &f.shim,
                Vec::new(),
                Some(&f.path()),
                Some(guard),
                Some(&f.env(&[])),
                1,
            )
        };
        assert!(run(&hops(MAX_HOPS - 1)).is_ok());
        assert_eq!(run(&hops(MAX_HOPS)), Err(Refused::TooManyHops));
    }

    /// What the real program is decides whether the shim may add to it.
    #[test]
    fn the_launcher_kind_comes_from_the_start_of_the_file() {
        let cases: [(&[u8], Option<Launcher>); 18] = [
            (b"\x7fELF\x02\x01", Some(Launcher::Native)),
            (b"\xcf\xfa\xed\xfe....", Some(Launcher::Native)),
            (b"\xca\xfe\xba\xbe....", Some(Launcher::Native)),
            (
                b"#!/usr/bin/env node\n// codex",
                Some(Launcher::Interpreted),
            ),
            (b"#!/usr/local/bin/node", Some(Launcher::Interpreted)),
            (
                b"#!/usr/bin/env -S node --no-warnings\n",
                Some(Launcher::Interpreted),
            ),
            (b"#!/usr/bin/env python3\n", Some(Launcher::Interpreted)),
            (b"#!/bin/sh\nexport CODEX_HOME=/x\n", Some(Launcher::Shell)),
            (b"#! /bin/bash -e\n", Some(Launcher::Shell)),
            (b"#!/usr/bin/env dash\n", Some(Launcher::Shell)),
            (b"#!/usr/bin/env -S zsh -f\n", Some(Launcher::Shell)),
            (
                b"#!/nix/store/abc-bash-5.2/bin/bash -e\n",
                Some(Launcher::Shell),
            ),
            (b"#!/usr/bin/env CODEX_HOME=/x node\n", None),
            (b"#!/usr/bin/env -i node\n", None),
            (b"#!/usr/bin/env -S \"bash -e\"\n", None),
            (b"#!\n", None),
            (b"echo no interpreter line\n", None),
            (b"", None),
        ];
        for (head, kind) in cases {
            assert_eq!(
                launcher_kind(head),
                kind,
                "{}",
                String::from_utf8_lossy(head)
            );
        }
        assert!(is_nix_store_path(Path::new(
            "/nix/store/0abc-codex-0.1/bin/codex"
        )));
        assert!(!is_nix_store_path(Path::new("/home/u/nix/store/codex")));
        assert!(!is_nix_store_path(Path::new("/nix/storefront/codex")));
    }

    /// A `codex` that is a shell script of the user's — which may set
    /// `CODEX_HOME` to a configuration that opts out before running the real
    /// one — runs unchanged; so do a file the shim cannot read and an
    /// environment the decision cannot read exactly. A Node launcher, as npm
    /// installs Codex, gets the additions.
    #[test]
    fn only_a_launcher_the_shim_is_sure_of_gets_the_additions() {
        let f = fixture();
        let args = ["exec"];
        let gets = |plan: Result<Plan, Refused>| plan.unwrap().args.len() > args.len();
        assert!(gets(f.plan("codex", &args, &f.env(&[]))), "a Node launcher");
        let wrapper = f.real.join("codex");
        crate::test_isolation::write_script(
            &wrapper,
            "#!/bin/sh\nexport CODEX_HOME=/opted/out\nexec /opt/codex \"$@\"\n",
        )
        .unwrap();
        assert!(
            !gets(f.plan("codex", &args, &f.env(&[]))),
            "a shell wrapper"
        );
        crate::test_isolation::write_script(f.real.join("devin"), "#!/usr/bin/env bash\n").unwrap();
        assert!(!gets(f.plan("devin", &args, &f.env(&[]))), "Devin's too");
        executable(&wrapper);
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o111)).unwrap();
        if std::fs::File::open(&wrapper).is_err() {
            assert!(!gets(f.plan("codex", &args, &f.env(&[]))), "unreadable");
        }
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let inexact = plan("codex", &f.shim, os(&args), Some(&f.path()), None, None, 1);
        assert_eq!(
            inexact,
            Ok(Plan {
                program: wrapper,
                args: os(&args)
            }),
            "an environment it cannot read exactly"
        );
    }
}
