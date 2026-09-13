//! Issue #1019 review: `scripts/reap-orphans.sh` SIGKILLs processes selected by
//! parsing `/proc`, and every property that makes that safe is a RUNTIME one.
//!
//! Nothing compiles a shell script, so the never-kill list, the two-part MCP
//! identification, the `/proc/<pid>/stat` field arithmetic and the pid-reuse
//! check are invisible to every gate this repository has. That is the argument
//! CLAUDE.md rule 5 records for `clean_tmp.rs` and `junit_strip.rs` — a deletion
//! or termination tool whose safety lives only at run time belongs in
//! `cargo test-fast`, where breaking it reddens the per-task gate rather than
//! surfacing as a process somebody needed being gone.
//!
//! The script reads through `PROC_ROOT` and signals through `REAP_KILL_CMD`,
//! both defaulting to production behaviour. So these tests build a synthetic
//! `/proc` in a `tempfile::tempdir()` and record signals to a file instead of
//! sending them: **no test here ever signals a real process**, which matters
//! because a synthetic pid can collide with a live one.
//!
//! Tests only. The rules live in the script; this is its gate.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn bash_present() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// One synthetic process. `comm` may contain spaces and parentheses on purpose —
/// that is the case the script's `stat_after_comm` exists for.
struct Proc {
    pid: u32,
    comm: &'static str,
    ppid: u32,
    /// Clock ticks since boot. Smaller = older.
    starttime: u64,
    cmdline: &'static str,
}

/// Build a synthetic PROC_ROOT. `uptime` is large so every process reads as old
/// enough to clear `--min-age`.
fn proc_root(dir: &Path, procs: &[Proc]) {
    fs::write(dir.join("uptime"), "9000000.00 1.00\n").unwrap();
    for p in procs {
        let d = dir.join(p.pid.to_string());
        fs::create_dir_all(&d).unwrap();
        // proc(5) numbers from 1 with pid=1 and comm=2, and the script splits on
        // the LAST ") " — so the remainder starts at state (field 3) and these
        // `fields` start at ppid (field 4). Original field N is therefore
        // fields[N - 4]: utime 14 -> 10, stime 15 -> 11, starttime 22 -> 18.
        // Getting this off by one silently moves starttime out of the field the
        // pid-reuse guard reads, which is a test that passes for the wrong reason.
        let mut fields = vec![p.ppid.to_string()];
        while fields.len() < 10 {
            fields.push("0".into());
        }
        fields.push("0".into()); // 14 utime  -> index 10
        fields.push("0".into()); // 15 stime  -> index 11
        while fields.len() < 18 {
            fields.push("0".into());
        }
        fields.push(p.starttime.to_string()); // 22 starttime -> index 18
        assert_eq!(fields.len(), 19, "starttime must land in proc field 22");
        fs::write(
            d.join("stat"),
            format!("{} ({}) S {}\n", p.pid, p.comm, fields.join(" ")),
        )
        .unwrap();
        fs::write(d.join("comm"), format!("{}\n", p.comm)).unwrap();
        fs::write(d.join("cmdline"), p.cmdline.replace(' ', "\0")).unwrap();
    }
}

struct Run {
    stdout: String,
    signals: String,
}

impl Run {
    fn selected(&self, pid: u32) -> bool {
        self.stdout
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("pid {pid} ")))
    }
}

fn run(procs: &[Proc], args: &[&str]) -> Run {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proc");
    fs::create_dir_all(&root).unwrap();
    proc_root(&root, procs);

    // Records "<signal> <pid>" instead of signalling. `-0` must report the
    // process as ALIVE so the escalation path is exercised.
    let log = tmp.path().join("signals.log");
    let killer = tmp.path().join("fake-kill");
    fs::write(
        &killer,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"-0\" ]; then exit 0; fi\n\
             echo \"$1 $2\" >> {}\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&killer, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = Command::new("bash")
        .arg(repo_root().join("scripts/reap-orphans.sh"))
        .args(args)
        .args(["--sample", "1"])
        .env("PROC_ROOT", &root)
        .env("REAP_KILL_CMD", &killer)
        .env("REAP_UID_SELF", format!("{}", nix_uid()))
        .output()
        .expect("bash should run once bash_present() said so");

    Run {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        signals: fs::read_to_string(&log).unwrap_or_default(),
    }
}

/// The synthetic tree is owned by whoever runs the tests, so the script's
/// own-processes-only check has to be told that uid.
fn nix_uid() -> u32 {
    let out = Command::new("id").arg("-u").output().expect("id -u");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn skip() -> bool {
    if !bash_present() {
        eprintln!("SKIP: bash not available; the reaper's safety rules are unverified here");
        return true;
    }
    if !cfg!(unix) {
        eprintln!("SKIP: not a unix host");
        return true;
    }
    false
}

const MCP: &str = "node /home/u/.npm/_npx/abc/node_modules/telegram-mcp-bot/dist/index.js";

/// The one that matters most. `dot-agent-deck daemon serve` is PPid 1 BY DESIGN
/// — it setsids away from the TUI — so it matches the orphan filter every time,
/// and its in-memory role maps exist nowhere else. Reaping it abandons every
/// running orchestration (CLAUDE.md rule 15).
#[test]
fn the_daemon_is_never_selected_even_though_it_is_always_an_orphan() {
    if skip() {
        return;
    }
    let r = run(
        &[Proc {
            pid: 4001,
            comm: "dot-agent-deck",
            ppid: 1,
            starttime: 100,
            cmdline: "dot-agent-deck daemon serve",
        }],
        &["--include-spin", "--include-stale"],
    );
    assert!(
        !r.selected(4001),
        "the daemon was selected for reaping:\n{}",
        r.stdout
    );
}

/// The never-kill list is matched against the executable NAME, never the whole
/// command line — the script's own comment records a live spinner being made
/// immune because its scratch path contained "dot-agent-deck". A path is not an
/// identity, and this pins the direction of that fix.
#[test]
fn a_path_containing_a_never_kill_name_does_not_confer_immunity() {
    if skip() {
        return;
    }
    let r = run(
        &[Proc {
            pid: 4002,
            comm: "node",
            ppid: 1,
            starttime: 100,
            cmdline: "node /home/u/code/dot-agent-deck/tmp/telegram-mcp-bot/index.js",
        }],
        &[],
    );
    assert!(
        r.selected(4002),
        "a path containing 'dot-agent-deck' exempted an MCP orphan:\n{}",
        r.stdout
    );
}

/// MCP identification is deliberately two-part: a node runtime AND a known
/// package. Either half alone reaps the wrong thing — the script's comment calls
/// a bare substring match "the same defect class in the opposite direction".
#[test]
fn mcp_identification_needs_both_the_runtime_and_the_package() {
    if skip() {
        return;
    }
    let r = run(
        &[
            Proc {
                pid: 4101,
                comm: "node",
                ppid: 1,
                starttime: 100,
                cmdline: MCP,
            },
            // runtime, no known package
            Proc {
                pid: 4102,
                comm: "node",
                ppid: 1,
                starttime: 100,
                cmdline: "node /srv/app/server.js",
            },
            // package name in the path, but not a node runtime
            Proc {
                pid: 4103,
                comm: "python3",
                ppid: 1,
                starttime: 100,
                cmdline: "python3 /opt/mcp-server-tool/run.py",
            },
        ],
        &[],
    );
    assert!(
        r.selected(4101),
        "a genuine orphaned MCP server was missed:\n{}",
        r.stdout
    );
    assert!(
        !r.selected(4102),
        "a plain node orphan was selected:\n{}",
        r.stdout
    );
    assert!(
        !r.selected(4103),
        "a non-node process was selected on its path:\n{}",
        r.stdout
    );
}

/// `/proc/<pid>/stat`'s comm is parenthesised and MAY CONTAIN SPACES — "npm exec
/// telegr" is real, and is exactly the shape this script targets. Positional awk
/// on the raw line returns the wrong field silently (measured: `$4` returned the
/// state 'S' instead of the ppid), so a process whose comm has a space would be
/// read as not-an-orphan and skipped.
#[test]
fn a_comm_containing_spaces_and_parens_still_parses() {
    if skip() {
        return;
    }
    let r = run(
        &[
            Proc {
                pid: 4201,
                comm: "npm exec telegr",
                ppid: 1,
                starttime: 100,
                cmdline: MCP,
            },
            // same shape but NOT an orphan: must be filtered on the real ppid,
            // which is only correct if the comm was parsed past.
            Proc {
                pid: 4202,
                comm: "npm exec telegr",
                ppid: 900,
                starttime: 100,
                cmdline: MCP,
            },
            Proc {
                pid: 4203,
                comm: "npm (weird) exec",
                ppid: 1,
                starttime: 100,
                cmdline: MCP,
            },
        ],
        &[],
    );
    assert!(
        r.selected(4201),
        "a spaced comm broke orphan detection:\n{}",
        r.stdout
    );
    assert!(
        !r.selected(4202),
        "a non-orphan was selected, so ppid was misread:\n{}",
        r.stdout
    );
    assert!(
        r.selected(4203),
        "a parenthesised comm broke parsing:\n{}",
        r.stdout
    );
}

/// Dry run is the default and must signal nothing, like `cargo xtask
/// clean-e2e-tmp`. Everything this script does is irreversible.
#[test]
fn the_default_is_a_dry_run_that_signals_nothing() {
    if skip() {
        return;
    }
    let r = run(
        &[Proc {
            pid: 4301,
            comm: "node",
            ppid: 1,
            starttime: 100,
            cmdline: MCP,
        }],
        &[],
    );
    assert!(r.selected(4301));
    assert!(
        r.stdout.contains("DRY RUN"),
        "no dry-run notice:\n{}",
        r.stdout
    );
    assert!(r.signals.is_empty(), "a dry run signalled: {:?}", r.signals);
}

/// With --apply, SIGTERM first and SIGKILL only for what survives it. A wedged
/// event loop cannot run its own handler, which is why the escalation exists.
#[test]
fn apply_sends_sigterm_then_escalates() {
    if skip() {
        return;
    }
    let r = run(
        &[Proc {
            pid: 4401,
            comm: "node",
            ppid: 1,
            starttime: 100,
            cmdline: MCP,
        }],
        &["--apply"],
    );
    assert!(
        r.signals.contains("-TERM 4401"),
        "no SIGTERM: {:?}",
        r.signals
    );
    assert!(
        r.signals.contains("-KILL 4401"),
        "no escalation: {:?}",
        r.signals
    );
}

/// The pid-reuse guard, and the reason SIGKILL is not sent on the number alone:
/// a pid alive five seconds later need not be the SAME process. `starttime` does
/// not carry over to a reused pid, so a changed one means "not my process" and
/// the kill must be abandoned — SIGKILL is unblockable, so getting this wrong
/// destroys an innocent bystander.
#[test]
fn a_reused_pid_is_not_killed() {
    if skip() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proc");
    fs::create_dir_all(&root).unwrap();
    let procs = [Proc {
        pid: 4501,
        comm: "node",
        ppid: 1,
        starttime: 100,
        cmdline: MCP,
    }];
    proc_root(&root, &procs);

    // The moment the script pauses before escalating, rewrite starttime — the
    // observable signature of the number having been handed to a new process.
    let log = tmp.path().join("signals.log");
    let killer = tmp.path().join("fake-kill");
    let stat = root.join("4501/stat");
    fs::write(
        &killer,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = \"-0\" ]; then exit 0; fi\n\
             echo \"$1 $2\" >> {log}\n\
             if [ \"$1\" = \"-TERM\" ]; then sed -i 's/ 100$/ 999999/' {stat}; fi\n\
             exit 0\n",
            log = log.display(),
            stat = stat.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&killer, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = Command::new("bash")
        .arg(repo_root().join("scripts/reap-orphans.sh"))
        .args(["--apply", "--sample", "1"])
        .env("PROC_ROOT", &root)
        .env("REAP_KILL_CMD", &killer)
        .env("REAP_UID_SELF", format!("{}", nix_uid()))
        .output()
        .expect("bash");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let signals = fs::read_to_string(&log).unwrap_or_default();

    assert!(signals.contains("-TERM 4501"), "no SIGTERM: {signals:?}");
    assert!(
        !signals.contains("-KILL 4501"),
        "SIGKILL was sent to a REUSED pid — this is the bystander case:\n{signals:?}"
    );
    assert!(
        stdout.contains("pid reused since selection"),
        "the skip was not reported:\n{stdout}"
    );
}

/// The two opt-in rules are opt-in. `spin` cannot tell a wedged agent from a
/// detached build, and the installed timer runs with --apply — so a default-on
/// CPU rule would reap deliberate work.
#[test]
fn the_cpu_and_age_rules_stay_opt_in() {
    if skip() {
        return;
    }
    let old = [Proc {
        pid: 4601,
        comm: "cargo",
        ppid: 1,
        starttime: 1,
        cmdline: "cargo build --release",
    }];
    assert!(
        !run(&old, &[]).selected(4601),
        "a stale orphan was reaped by default"
    );
    assert!(
        run(&old, &["--include-stale"]).selected(4601),
        "--include-stale did not enable the age rule"
    );
}
