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
//!    `PATH` outside the shim directory ([`resolve`]) — none is the shell's
//!    `command not found` and status 127;
//! 2. decides from its own environment — the agent's real one, after any
//!    script or launcher — what the agent needs ([`super::launch`]), unless
//!    this program was already shimmed further up its process tree
//!    ([`GUARD_ENV`]);
//! 3. replaces itself with the real program, those arguments in front of the
//!    user's own, so signals, the exit status and the terminal are the
//!    program's own.
//!
//! Anything that goes wrong in step 2 runs the program unchanged: the shim
//! never stops an agent from starting. A script that calls the agent by an
//! absolute path, or replaces `PATH` rather than adding to it, does not reach
//! the shim. There is no shim on Windows.

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
    let Some(plan) = plan(
        name,
        dir,
        args,
        path.as_deref(),
        guard.as_deref(),
        &super::config::process_env(),
        std::process::id(),
    ) else {
        eprintln!("{name}: command not found");
        return ExitCode::from(127);
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

/// The shim's plan for `name` called with `args`, with `PATH` = `path`,
/// [`GUARD_ENV`] = `guard` and the environment `env`; the program will have
/// process id `pid`. `None` when there is no real program to run.
fn plan(
    name: &str,
    dir: &Path,
    args: Vec<OsString>,
    path: Option<&OsStr>,
    guard: Option<&OsStr>,
    env: &EnvMap,
    pid: u32,
) -> Option<Plan> {
    let program = resolve(name, path, dir)?;
    let already = guard.is_some_and(|g| g.to_string_lossy().split(':').any(|n| n == name));
    let added = if already {
        Vec::new()
    } else {
        additions(name, dir, &args, env, pid)
    };
    Some(Plan {
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

/// The program `name` as a shell would find it on `path`, skipping the shim
/// directory `dir`: a file that, symlinks resolved, is in that directory —
/// so an entry that is the directory, or a symlink to it, or a link to one of
/// its scripts, is passed over. An empty entry is the current directory, as
/// for the shell.
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
        })
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

    fn executable(path: &Path) {
        crate::test_isolation::write_script(path, "#!/bin/sh\n").unwrap();
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

        fn plan(&self, name: &str, args: &[&str], env: &EnvMap) -> Option<Plan> {
            plan(
                name,
                &self.shim,
                os(args),
                Some(&self.path()),
                None,
                env,
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
                &f.env(&[]),
                1
            ),
            None
        );
        assert!(
            f.plan("codex", &[], &f.env(&[]))
                .is_some_and(|p| p.program == f.real.join("codex"))
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
        let unchanged = |name: &str, plan: Option<Plan>| {
            assert_eq!(
                plan,
                Some(Plan {
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
                &f.env(&[]),
                1
            ),
            Some(Plan {
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
            &f.env(&[]),
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
}
