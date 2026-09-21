//! `cargo xver` — CLAUDE.md rule 12's cross-version contract check, scripted.
//!
//! # What this is
//!
//! Rule 12 requires, for any change touching the daemon, the TUI↔daemon
//! protocol, orchestration or hooks, that you "build the branch, start a daemon
//! from the previous release with an agent under it, run the branch TUI against
//! that older daemon, and confirm the core flows still work end to end".
//!
//! **The requirement is the SCENARIO, not human fingers.** Rule 12 spells the
//! procedure out in keystrokes because a person was always going to run it, and
//! four of its paragraphs are about ways a person silently gets it wrong. A
//! driver that reproduces the same scenario and asserts the same tells
//! discharges the same obligation, and is checkable afterwards in a way a
//! recollection is not.
//!
//! # The scenario, and why each step is in it
//!
//! 1. Start the **previous release's** daemon in an isolated sandbox, capturing
//!    its pid.
//! 2. Attach the **previous release's** TUI and bring an orchestration up under
//!    it. *Without live agents the branch TUI silently SIGTERMs the old daemon
//!    and lazy-spawns its own — the build-version handshake behaving as designed
//!    — and every later check then passes against a daemon of the branch's own
//!    build.*
//! 3. Close that TUI with `Ctrl+D`, `Ctrl+C`, **Detach**. *`Ctrl+C` in
//!    `PaneInput` mode goes to the pane, not the deck, and kills a role;
//!    `Stop` shuts down the very daemon the test needs kept alive.*
//! 4. Attach the **branch** TUI. The build-version mismatch prompt naming the
//!    live agents is itself the proof the scenario was reached. Decline it.
//! 5. Exercise the flows: **delegate first, status hook last.** *Sending
//!    `agent-event --type running` before the delegate check makes the daemon
//!    classify that pane's agent type as `Pi`, which routes prompt delivery
//!    differently and makes a delivered delegate briefly look undelivered.*
//! 6. Tear down **by pid**.
//!
//! `docs/develop/cross-version-harness.md` is the operational page: how to run
//! it, what it covers, and what it does not.

mod proc;
mod pty;
mod report;
mod sandbox;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::time::Duration;

use clap::Parser;
use report::{Evidence, Verdict};
use sandbox::{EndpointMode, Sandbox};

/// How long to wait for the deck to paint its first frame, for the mismatch
/// prompt, and for an orchestration to come up. Generous because this box is
/// shared and a run competing with three dispatched units is the normal case.
const UI_TIMEOUT: Duration = Duration::from_secs(90);
/// How long to wait for a single keystroke's consequence.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the daemon's own state to catch up with a CLI call.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(45);
/// Settle pause between a key and the next one, where the deck has to redraw in
/// between and there is nothing specific to poll for.
const SETTLE: Duration = Duration::from_millis(400);

const ROLE_ORCHESTRATOR: &str = "orchestrator";
const ROLE_CODER: &str = "coder";
const ROLE_REVIEWER: &str = "reviewer";

/// CLAUDE.md rule 12's cross-version contract check, as a scripted PTY driver.
#[derive(Parser, Debug)]
#[command(
    name = "xtask-cross-version",
    about = "Reproduce and verify CLAUDE.md rule 12's cross-version manual test for a branch"
)]
struct Opts {
    /// The branch under test — the "new" side. Taken from `origin/<branch>` and
    /// checked out DETACHED, so nothing in the reusable worktree can commit,
    /// amend, rebase or push to it.
    #[arg(long)]
    branch: String,

    /// The previous release — the "old" side. Its published Linux binary is
    /// downloaded with `gh release download` and cached.
    #[arg(long, default_value = "v0.41.0")]
    previous: String,

    /// `owner/repo` the release asset comes from.
    #[arg(long, default_value = "vfarcic/dot-agent-deck")]
    repo: String,

    /// Use this binary as the old side instead of downloading a release.
    #[arg(long)]
    old_binary: Option<PathBuf>,

    /// The reusable, disk-backed worktree the branch is built in. Defaults to
    /// `<repo parent>/dot-agent-deck-xver`.
    #[arg(long)]
    worktree: Option<PathBuf>,

    /// `CARGO_TARGET_DIR` for that worktree, reused across branches so the
    /// cargo cache survives. Defaults to `<repo parent>/dot-agent-deck-xver-target`.
    #[arg(long)]
    target_dir: Option<PathBuf>,

    /// Where per-run sandboxes are created. Defaults to
    /// `<repo parent>/dot-agent-deck-xver-runs`.
    #[arg(long)]
    runs_root: Option<PathBuf>,

    /// Where release binaries are cached. Defaults to
    /// `<repo parent>/dot-agent-deck-xver-releases`.
    #[arg(long)]
    releases_dir: Option<PathBuf>,

    /// Where to write the markdown evidence report. Defaults to
    /// `.dot-agent-deck/xver-evidence/<branch slug>.md` in this checkout.
    #[arg(long)]
    evidence: Option<PathBuf>,

    /// How the run addresses the daemon.
    ///
    /// `sandbox-sockets` (the default) pins both socket overrides inside the
    /// sandbox. `resolved` sets neither, so both builds resolve their endpoint
    /// the way they would on a real host — required when the change under test
    /// IS endpoint resolution, because those overrides short-circuit it. Read
    /// the harness doc before using `resolved`: it touches process-global paths
    /// and two such runs cannot execute concurrently.
    #[arg(long, value_enum, default_value_t = ModeArg::SandboxSockets)]
    endpoint_mode: ModeArg,

    /// Unset `XDG_RUNTIME_DIR` for every process in the run.
    ///
    /// This is a whole failure mode of its own for a change that moves the
    /// endpoint path in the FALLBACK case only (issue #1121): on a normal
    /// desktop session `XDG_RUNTIME_DIR` is set, both builds resolve
    /// byte-identical endpoints, and the run exercises the arm the change did
    /// not touch. An option rather than a hardcode, because for every other
    /// kind of change the ordinary desktop configuration is the faithful one.
    #[arg(long)]
    unset_xdg_runtime_dir: bool,

    /// Turn the experimental feature flag ON for the run. Off by default and
    /// always pinned explicitly, because project-config discovery walks up from
    /// CWD and would otherwise read the operator's real `.dot-agent-deck.toml`.
    #[arg(long)]
    experimental: bool,

    /// Skip `cargo build` and use whatever is already at the target dir. For
    /// iterating on the harness itself.
    #[arg(long)]
    skip_build: bool,

    /// Keep the sandbox directory after the run. It is kept automatically on a
    /// failure.
    #[arg(long)]
    keep_sandbox: bool,

    /// Refuse to start when the runs root or the cargo target dir has less than
    /// this many GiB free (CLAUDE.md rule 14).
    #[arg(long, default_value_t = 100)]
    min_free_gib: u64,

    /// Cap every stand-in agent's lifetime, so one that escapes its process
    /// group self-exits rather than leaking to PID 1.
    #[arg(long, default_value_t = 1800)]
    max_agent_lifetime_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ModeArg {
    SandboxSockets,
    Resolved,
}

fn main() -> ExitCode {
    let opts = Opts::parse();
    match run(&opts) {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("\nxver: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Small process helpers
// ---------------------------------------------------------------------------

fn utc_now() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "<unknown>".to_string())
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run a plain command and require success, returning stdout.
fn must_run(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what} failed ({}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Invoke one of the two deck binaries with the run's environment.
fn deck(bin: &Path, env: &[(String, String)], cwd: &Path, args: &[&str]) -> Result<Output, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(cwd).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output()
        .map_err(|e| format!("run {} {args:?}: {e}", bin.display()))
}

/// One row of `daemon status --json`.
#[derive(Debug, Clone)]
struct StatusRow {
    pane_id: String,
    role: String,
    status: String,
}

/// `daemon status --json`, projected to what this harness asserts on.
///
/// Reads the daemon over `AttachRequest::ListAgents` and never lazily spawns
/// one, so an unreachable daemon is an `Err` here rather than a freshly-minted
/// empty daemon nobody asked for.
fn daemon_status(
    bin: &Path,
    env: &[(String, String)],
    cwd: &Path,
) -> Result<Vec<StatusRow>, String> {
    let out = deck(bin, env, cwd, &["daemon", "status", "--json"])?;
    if !out.status.success() {
        return Err(format!(
            "daemon status --json exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("daemon status --json is not JSON: {e}"))?;
    let agents = doc
        .get("agents")
        .and_then(|a| a.as_array())
        .ok_or_else(|| "daemon status --json has no `agents` array".to_string())?;
    Ok(agents
        .iter()
        .map(|a| StatusRow {
            pane_id: a
                .get("pane_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            role: a
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            status: a
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect())
}

/// Poll `daemon status` until `pred` holds over its rows.
fn wait_for_status(
    bin: &Path,
    env: &[(String, String)],
    cwd: &Path,
    timeout: Duration,
    pred: impl Fn(&[StatusRow]) -> bool,
) -> Result<Vec<StatusRow>, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let last = daemon_status(bin, env, cwd);
        if let Ok(rows) = &last
            && pred(rows)
        {
            return Ok(rows.clone());
        }
        if std::time::Instant::now() >= deadline {
            return match last {
                Ok(rows) => Err(format!(
                    "daemon status never satisfied the condition within {timeout:?}; last rows: {rows:?}"
                )),
                Err(e) => Err(format!(
                    "daemon status kept failing within {timeout:?}: {e}"
                )),
            };
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn count_in_file(path: &Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.matches(needle).count())
        .unwrap_or(0)
}

/// The last `n` lines of `s` — what a failure message should quote rather than
/// the whole scrollback.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

// ---------------------------------------------------------------------------
// Build inputs
// ---------------------------------------------------------------------------

fn repo_root() -> Result<PathBuf, String> {
    let out = must_run(
        Command::new("git").args(["rev-parse", "--show-toplevel"]),
        "git rev-parse --show-toplevel",
    )?;
    Ok(PathBuf::from(out.trim()))
}

/// Fetch (or reuse) the previous release's published Linux binary, and assert
/// it really reports that version.
fn old_binary(opts: &Opts, releases: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = &opts.old_binary {
        return Ok(explicit.clone());
    }
    let dir = releases.join(&opts.previous);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let bin = dir.join("dot-agent-deck-linux-amd64");
    if !bin.exists() {
        must_run(
            Command::new("gh").args([
                "release",
                "download",
                &opts.previous,
                "--repo",
                &opts.repo,
                "-p",
                "dot-agent-deck-linux-amd64",
                "-D",
                &dir.to_string_lossy(),
            ]),
            &format!("gh release download {}", opts.previous),
        )?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin)
            .map_err(|e| format!("stat {}: {e}", bin.display()))?
            .permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(&bin, perms);
    }
    let reported = must_run(Command::new(&bin).arg("--version"), "old binary --version")?;
    let want = opts.previous.trim_start_matches('v');
    if !reported.contains(want) {
        return Err(format!(
            "the downloaded {} binary reports {reported:?}, which does not contain {want:?} — \
             refusing to run a cross-version check against an unknown build",
            opts.previous
        ));
    }
    Ok(bin)
}

/// Point the reusable worktree at `origin/<branch>` and build it.
///
/// Detached, always: the eight branches this sweeps are being verified, not
/// changed, and a detached HEAD cannot commit to one by accident. It also side-
/// steps the case where the branch is already checked out in another worktree.
fn new_binary(
    opts: &Opts,
    worktree: &Path,
    target_dir: &Path,
) -> Result<(PathBuf, String), String> {
    if !worktree.join(".git").exists() {
        must_run(
            Command::new("git").args([
                "worktree",
                "add",
                "--detach",
                &worktree.to_string_lossy(),
                &format!("origin/{}", opts.branch),
            ]),
            "git worktree add",
        )?;
    }
    must_run(
        Command::new("git")
            .current_dir(worktree)
            .args(["fetch", "origin", &opts.branch]),
        "git fetch origin <branch>",
    )?;
    must_run(
        Command::new("git")
            .current_dir(worktree)
            .args(["checkout", "--detach", "FETCH_HEAD"]),
        "git checkout --detach FETCH_HEAD",
    )?;
    let sha = must_run(
        Command::new("git")
            .current_dir(worktree)
            .args(["rev-parse", "HEAD"]),
        "git rev-parse HEAD",
    )?
    .trim()
    .to_string();

    if !opts.skip_build {
        must_run(
            Command::new("cargo")
                .current_dir(worktree)
                .env("CARGO_TARGET_DIR", target_dir)
                .args(["build", "--locked", "--bin", "dot-agent-deck"]),
            "cargo build --locked --bin dot-agent-deck",
        )?;
    }
    let bin = target_dir.join("debug").join("dot-agent-deck");
    if !bin.exists() {
        return Err(format!("no branch binary at {}", bin.display()));
    }
    Ok((bin, sha))
}

/// One field of a `daemon hello` document.
fn hello_field<'a>(doc: &'a serde_json::Value, key: &str) -> &'a str {
    doc.get(key).and_then(|v| v.as_str()).unwrap_or("<absent>")
}

/// Compare the two builds' self-reported contracts before a single process is
/// started, and refuse the one comparison that makes the whole run vacuous.
///
/// The build ids being EQUAL is that case: the build-version handshake then
/// matches, no mismatch prompt is printed, the branch TUI attaches to a daemon
/// of its own build, and every later tell passes against a same-version run.
/// Catching it here rather than at the 90-second prompt wait is the difference
/// between a one-line refusal and a run that looks like it broke down.
///
/// A differing `server_version` is reported, not refused: that is a hard
/// protocol floor, so the pairing would be rejected at the handshake instead of
/// exercising semantics behind a stable wire. It is a legitimate thing to want
/// to observe, and the evidence file should say which of the two happened.
fn compare_hellos(old_raw: &str, new_raw: &str) -> Result<Vec<String>, String> {
    let old: serde_json::Value =
        serde_json::from_str(old_raw.trim()).map_err(|e| format!("old `daemon hello`: {e}"))?;
    let new: serde_json::Value =
        serde_json::from_str(new_raw.trim()).map_err(|e| format!("branch `daemon hello`: {e}"))?;
    let (ob, nb) = (
        hello_field(&old, "build_version"),
        hello_field(&new, "build_version"),
    );
    if ob == nb {
        return Err(format!(
            "both sides report build id {ob:?}, so the build-version handshake would MATCH and \
             no mismatch prompt would be printed. That is a same-version run wearing this \
             harness's clothes — refusing before anything is started. Check that --branch really \
             differs from --previous, and that the branch binary was rebuilt."
        ));
    }
    let mut notes = vec![format!(
        "build ids differ: old {ob}, branch {nb} — the handshake will take the mismatch path"
    )];
    let (op, np) = (
        old.get("server_version").cloned(),
        new.get("server_version").cloned(),
    );
    notes.push(if op == np {
        format!(
            "PROTOCOL_VERSION matches on both sides ({}), so this run exercises SEMANTICS behind \
             a stable wire — which is what rule 12 is for",
            op.map(|v| v.to_string())
                .unwrap_or_else(|| "<absent>".into())
        )
    } else {
        format!(
            "PROTOCOL_VERSION DIFFERS (old {:?}, branch {:?}). That is a hard floor, so the two \
             builds refuse each other at the handshake rather than interoperating; read a failure \
             below as that refusal rather than as a semantic break",
            op, np
        )
    });
    let breaks = |v: &serde_json::Value| {
        v.get("contract_breaks")
            .and_then(|b| b.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let (obr, nbr) = (breaks(&old), breaks(&new));
    notes.push(if obr == nbr {
        format!(
            "CONTRACT_BREAKS identical on both sides ({} entries)",
            obr.len()
        )
    } else {
        format!(
            "CONTRACT_BREAKS differ: old {obr:?}, branch {nbr:?}. The desktop's \
             `classify_handshake` refuses across any difference in that list, so a new app would \
             refuse this older daemon outright — a surface this harness does not exercise"
        )
    });
    Ok(notes)
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn run(opts: &Opts) -> Result<bool, String> {
    let root = repo_root()?;
    let parent = root
        .parent()
        .ok_or_else(|| "the repository root has no parent".to_string())?
        .to_path_buf();
    let worktree = opts
        .worktree
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver"));
    let target_dir = opts
        .target_dir
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver-target"));
    let runs_root = opts
        .runs_root
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver-runs"));
    let releases = opts
        .releases_dir
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver-releases"));

    let mut ev = Evidence {
        branch: opts.branch.clone(),
        previous: opts.previous.clone(),
        started_at: utc_now(),
        mode: match opts.endpoint_mode {
            ModeArg::SandboxSockets => {
                "sandbox-sockets (both socket overrides pinned inside the sandbox)".into()
            }
            ModeArg::Resolved => {
                "resolved (neither socket override set — both builds resolve their own endpoint)"
                    .into()
            }
        },
        xdg_runtime_dir: if opts.unset_xdg_runtime_dir {
            "UNSET for every process in the run".into()
        } else {
            match std::env::var("XDG_RUNTIME_DIR") {
                Ok(v) => format!("inherited from the host: `{v}`"),
                Err(_) => "not set on the host either".into(),
            }
        },
        ..Default::default()
    };

    println!("xver: preflight");
    ev.preflight.push(format!(
        "runs root: {}",
        sandbox::require_disk_backed("runs root", &runs_root, opts.min_free_gib)?
    ));
    ev.preflight.push(format!(
        "cargo target dir: {}",
        sandbox::require_disk_backed("cargo target dir", &target_dir, opts.min_free_gib)?
    ));
    let mode = match opts.endpoint_mode {
        ModeArg::SandboxSockets => EndpointMode::SandboxSockets,
        ModeArg::Resolved => EndpointMode::Resolved,
    };
    if mode == EndpointMode::Resolved {
        for note in sandbox::preflight_resolved_endpoints()? {
            ev.preflight.push(note);
        }
    }

    println!("xver: inputs");
    let old_bin = old_binary(opts, &releases)?;
    let (new_bin, head_sha) = new_binary(opts, &worktree, &target_dir)?;
    ev.old_binary = old_bin.clone();
    ev.new_binary = new_bin.clone();
    ev.head_sha = head_sha;
    ev.old_hello = must_run(
        Command::new(&old_bin).args(["daemon", "hello"]),
        "old daemon hello",
    )?;
    ev.new_hello = must_run(
        Command::new(&new_bin).args(["daemon", "hello"]),
        "new daemon hello",
    )?;
    println!("  · old {}", ev.old_hello.trim());
    println!("  · new {}", ev.new_hello.trim());
    for note in compare_hellos(&ev.old_hello, &ev.new_hello)? {
        ev.preflight.push(note);
    }

    let slug: String = opts
        .branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let sandbox_root = runs_root.join(format!("{slug}-{}", epoch_secs()));
    let sb = Sandbox::create(sandbox_root.clone())?;
    sandbox::write_project(&sb)?;
    ev.sandbox_root = sandbox_root.clone();

    let new_bin_dir = new_bin
        .parent()
        .ok_or_else(|| "the branch binary has no parent dir".to_string())?
        .to_path_buf();
    let env = sandbox::run_env(
        &sb,
        &new_bin_dir,
        mode,
        !opts.unset_xdg_runtime_dir,
        opts.experimental,
        opts.max_agent_lifetime_secs,
    );

    // Where the old daemon will listen, computed by the OLD build's own
    // resolution rule, so `ss` can be asked who owns it.
    let attach_endpoint = match mode {
        EndpointMode::SandboxSockets => sb.attach_socket(),
        EndpointMode::Resolved => match env.iter().find(|(k, _)| k == "XDG_RUNTIME_DIR") {
            Some((_, dir)) => PathBuf::from(dir).join("dot-agent-deck-attach.sock"),
            None => sandbox::legacy_endpoints()[1].clone(),
        },
    };
    ev.daemon_endpoint = attach_endpoint.display().to_string();

    let outcome = drive(
        opts,
        mode,
        &sb,
        &env,
        &old_bin,
        &new_bin,
        &attach_endpoint,
        &mut ev,
    );

    // Evidence is written whatever happened — a run that broke down partway is
    // a useful result and the file is where it is legible.
    let evidence_path = opts.evidence.clone().unwrap_or_else(|| {
        root.join(".dot-agent-deck")
            .join("xver-evidence")
            .join(format!("{slug}.md"))
    });
    if let Err(e) = &outcome {
        ev.step(format!("RUN ABORTED: {e}"));
        ev.tell(
            "aborted",
            "the run did not complete",
            Verdict::Fail,
            e.clone(),
        );
    }
    ev.write_to(&evidence_path)?;

    let passed = ev.passed() && ev.complete() && outcome.is_ok();
    if passed && !opts.keep_sandbox {
        let _ = std::fs::remove_dir_all(&sandbox_root);
        println!("xver: sandbox removed ({})", sandbox_root.display());
    } else {
        println!("xver: sandbox kept at {}", sandbox_root.display());
    }
    println!("xver: evidence written to {}", evidence_path.display());
    println!(
        "xver: {}",
        if passed {
            "PASS"
        } else if ev.passed() {
            "INCOMPLETE — a tell could not be measured"
        } else {
            "FAIL"
        }
    );
    Ok(passed)
}

/// Everything between "the sandbox exists" and "the sandbox daemon is gone".
///
/// Split out so the caller can write the evidence file on every path, including
/// the one where a step breaks down and returns `Err`.
#[allow(clippy::too_many_arguments)]
fn drive(
    opts: &Opts,
    mode: EndpointMode,
    sb: &Sandbox,
    env: &[(String, String)],
    old_bin: &Path,
    new_bin: &Path,
    attach_endpoint: &Path,
    ev: &mut Evidence,
) -> Result<(), String> {
    println!("xver: step 1 — start the previous release's daemon");
    let mut daemon_cmd = Command::new(old_bin);
    daemon_cmd
        .args(["daemon", "serve"])
        .current_dir(&sb.project)
        .env_clear();
    for (k, v) in env {
        daemon_cmd.env(k, v);
    }
    let daemon_out = std::fs::File::create(sb.artifacts.join("old-daemon.log"))
        .map_err(|e| format!("create old-daemon.log: {e}"))?;
    let daemon_err = daemon_out
        .try_clone()
        .map_err(|e| format!("clone old-daemon.log handle: {e}"))?;
    daemon_cmd.stdout(daemon_out).stderr(daemon_err);
    let child = daemon_cmd
        .spawn()
        .map_err(|e| format!("spawn the sandbox daemon: {e}"))?;
    let mut daemon = proc::SandboxProcess::adopt(
        child,
        old_bin.display().to_string(),
        "sandbox daemon".to_string(),
    );
    ev.daemon_pid = daemon.pid;
    ev.step(format!(
        "started the {} daemon as pid {} (cmdline gate: `{}`)",
        opts.previous,
        daemon.pid,
        old_bin.display()
    ));

    // Everything below can fail; the teardown must still happen, and must still
    // be pid-scoped. Run the body and keep its result.
    let body = drive_inner(
        opts,
        mode,
        sb,
        env,
        old_bin,
        new_bin,
        attach_endpoint,
        ev,
        &daemon,
    );

    println!("xver: teardown");
    // Which of the process-global legacy addresses THIS run's daemon is
    // listening on — asked before it is signalled, because afterwards there is
    // no listener to attribute. The alternative, deleting both paths because the
    // preflight found them absent, would remove a stranger's socket in the one
    // case that matters: this run's daemon never bound them and something else
    // has since.
    let owned_legacy: Vec<PathBuf> = if mode == EndpointMode::Resolved {
        sandbox::legacy_endpoints()
            .into_iter()
            .filter(|p| proc::listeners_on(p).is_some_and(|pids| pids.contains(&daemon.pid)))
            .collect()
    } else {
        Vec::new()
    };
    let label = daemon.label.clone();
    match daemon.terminate(Duration::from_secs(20)) {
        Ok(()) => ev.step(format!(
            "SIGTERMed the {label} by pid {} after verifying its command line still contained \
             the sandbox binary path; never a pattern match",
            daemon.pid
        )),
        Err(proc::SignalRefusal::Gone) => {
            ev.step(format!("{label} pid {} was already gone", daemon.pid))
        }
        Err(proc::SignalRefusal::NotOurs { cmdline }) => ev.step(format!(
            "REFUSED to signal pid {} ({label}): its command line is now `{cmdline}`, which does \
             not contain `{}`. The pid was recycled; nothing was signalled.",
            daemon.pid,
            old_bin.display()
        )),
    }
    if mode == EndpointMode::Resolved {
        let mut removed = Vec::new();
        let mut left = Vec::new();
        for path in sandbox::legacy_endpoints() {
            if !path.exists() {
                continue;
            }
            if owned_legacy.contains(&path) {
                let _ = std::fs::remove_file(&path);
                removed.push(path.display().to_string());
            } else {
                left.push(path.display().to_string());
            }
        }
        ev.step(format!(
            "legacy /tmp endpoints: removed {:?} (this run's daemon was the listener); left {:?} \
             alone (not attributable to this run's pid — a later run's preflight will refuse \
             until someone looks)",
            removed, left
        ));
    }
    body
}

#[allow(clippy::too_many_arguments)]
fn drive_inner(
    opts: &Opts,
    mode: EndpointMode,
    sb: &Sandbox,
    env: &[(String, String)],
    old_bin: &Path,
    new_bin: &Path,
    attach_endpoint: &Path,
    ev: &mut Evidence,
    daemon: &proc::SandboxProcess,
) -> Result<(), String> {
    let three_roles = |rows: &[StatusRow]| {
        [ROLE_ORCHESTRATOR, ROLE_CODER, ROLE_REVIEWER]
            .iter()
            .all(|r| rows.iter().any(|row| row.role.contains(*r)))
    };

    // The old binary asking its own daemon is the version-correct way to find
    // out whether it is up, in both endpoint modes.
    wait_for_status(old_bin, env, &sb.project, DAEMON_TIMEOUT, |_| true)
        .map_err(|e| format!("the sandbox daemon never answered `daemon status`: {e}"))?;
    ev.step("the sandbox daemon answered `daemon status`");
    let listeners_at_start = proc::listeners_on(attach_endpoint);

    println!("xver: step 2 — bring an orchestration up under it, with the OLD TUI");
    let mut old_tui = pty::PtyDeck::spawn(pty::PtySpec {
        label: "old-tui",
        bin: old_bin,
        args: &[],
        cwd: &sb.project,
        env,
        cols: 200,
        rows: 55,
        stream_log: sb.artifacts.join("old-tui.stream.txt"),
    })?;
    if !old_tui.wait_for_grid_string("No active sessions", UI_TIMEOUT) {
        return Err(format!(
            "{}: never reached an empty dashboard.\n=== grid ===\n{}",
            old_tui.label,
            old_tui.grid()
        ));
    }
    open_orchestration(&old_tui)?;
    let rows = wait_for_status(old_bin, env, &sb.project, UI_TIMEOUT, three_roles).map_err(|e| {
        format!(
            "the three role panes never came up under the sandbox daemon: {e}\n=== old TUI grid ===\n{}",
            old_tui.grid()
        )
    })?;
    ev.step(format!(
        "the old TUI brought up {} role panes under the old daemon: {}",
        rows.len(),
        rows.iter()
            .map(|r| format!("{} ({})", r.role, r.pane_id))
            .collect::<Vec<_>>()
            .join(", ")
    ));

    println!("xver: step 3 — Ctrl+D, Ctrl+C, Detach (never Stop)");
    old_tui.send(b"\x04"); // Ctrl+D — leave PaneInput. Without this Ctrl+C goes
    std::thread::sleep(SETTLE); // to the focused PANE and kills a role.
    old_tui.send(b"\x03"); // Ctrl+C — the quit dialog
    if !old_tui.wait_for_grid_string("Quit dot-agent-deck?", STEP_TIMEOUT) {
        return Err(format!(
            "Ctrl+D then Ctrl+C never opened the quit dialog in the old TUI.\n=== grid ===\n{}",
            old_tui.grid()
        ));
    }
    old_tui.send(b"\r"); // Enter on the default option, which is Detach (index 0).
    match old_tui.wait_for_exit(STEP_TIMEOUT) {
        Some(_) => {}
        None => {
            return Err(format!(
                "the old TUI did not exit after choosing Detach.\n=== grid ===\n{}",
                old_tui.grid()
            ));
        }
    }
    old_tui.shutdown();
    drop(old_tui);

    let after_detach = wait_for_status(old_bin, env, &sb.project, DAEMON_TIMEOUT, three_roles)
        .map_err(|e| {
            format!(
                "after the detach the daemon no longer lists all three roles — the Ctrl+C trap \
                 (it goes to the PANE in PaneInput mode) or a `Stop` instead of `Detach`: {e}"
            )
        })?;
    if !proc::is_alive(daemon.pid) {
        return Err(format!(
            "the sandbox daemon (pid {}) died during the detach — `Detach` must leave it running",
            daemon.pid
        ));
    }
    ev.step(format!(
        "after Detach the daemon (pid {}) is still alive and still lists {} role panes",
        daemon.pid,
        after_detach.len()
    ));

    println!("xver: step 4 — attach the BRANCH TUI and decline the mismatch prompt");
    let new_tui = pty::PtyDeck::spawn(pty::PtySpec {
        label: "new-tui",
        bin: new_bin,
        args: &[],
        cwd: &sb.project,
        env,
        cols: 200,
        rows: 55,
        stream_log: sb.artifacts.join("new-tui.stream.txt"),
    })?;
    let saw_prompt = new_tui.wait_for_stream_string("Daemon version mismatch", UI_TIMEOUT);
    let prompt_excerpt = extract_prompt(&new_tui.stream_text());
    if !saw_prompt {
        ev.excerpt(
            "branch TUI stream (no mismatch prompt)",
            tail(&new_tui.stream_text(), 60),
        );
        return Err(format!(
            "the branch TUI never printed the build-version mismatch prompt within {UI_TIMEOUT:?}. \
             That prompt appearing IS the proof the scenario was reached: either the branch TUI \
             did not find the old daemon at all (and has lazy-spawned its own — check the \
             `Attach protocol listening` count below), or the two builds report the same build \
             id. old={} new={}",
            ev.old_hello.trim(),
            ev.new_hello.trim()
        ));
    }
    ev.excerpt(
        "build-version mismatch prompt, as the branch TUI printed it",
        prompt_excerpt.clone(),
    );
    let named_roles: Vec<&str> = [ROLE_ORCHESTRATOR, ROLE_CODER, ROLE_REVIEWER]
        .into_iter()
        .filter(|r| prompt_excerpt.contains(r))
        .collect();
    ev.step(format!(
        "the branch TUI printed the build-version mismatch prompt naming {} of the live roles ({})",
        named_roles.len(),
        named_roles.join(", ")
    ));
    // Any key other than `s`/`S` declines and keeps the existing daemon.
    // Accepting would SIGTERM the old daemon and replace it, which destroys the
    // entire point of the run.
    new_tui.send(b"n");
    if !new_tui.wait_for_grid(UI_TIMEOUT, |g| {
        g.contains("XVER_") || g.contains(ROLE_ORCHESTRATOR)
    }) {
        return Err(format!(
            "{}: after declining the prompt it never rendered the orchestration.\n=== grid ===\n{}",
            new_tui.label,
            new_tui.grid()
        ));
    }
    ev.step("declined the prompt (`n`); the branch TUI attached to the OLD daemon unchanged");

    println!("xver: step 5 — delegate first, hooks last");
    focus_role(&new_tui, ROLE_ORCHESTRATOR)?;
    let nonce = epoch_secs();
    let delegate_sentinel = format!("XVER-DELEGATE-{nonce}");
    type_into_pane(
        &new_tui,
        &format!(
            "dot-agent-deck delegate --to {ROLE_CODER} --to {ROLE_REVIEWER} --task \"{delegate_sentinel} list the files in this directory\""
        ),
    );

    // Delivery is asserted on the PAYLOAD, in the target pane, not on the CLI's
    // exit code. `coder` is `cat`, so whatever the daemon wrote into its PTY is
    // echoed straight back onto the screen.
    focus_role(&new_tui, ROLE_CODER)?;
    let pointer = format!("worker-task-{ROLE_CODER}.md");
    let delivered = new_tui.wait_for_grid(UI_TIMEOUT, |g| {
        g.contains(&pointer) || g.contains(&delegate_sentinel)
    });
    let coder_grid = new_tui.grid();
    ev.excerpt("`coder` role pane after the delegate", coder_grid.clone());
    let task_file = sb
        .project
        .join(".dot-agent-deck")
        .join(format!("worker-task-{ROLE_CODER}.md"));
    let task_file_note = match std::fs::read_to_string(&task_file) {
        Ok(body) if body.contains(&delegate_sentinel) => {
            format!("{} carries the sentinel", task_file.display())
        }
        Ok(_) => format!(
            "{} exists but does NOT carry the sentinel",
            task_file.display()
        ),
        Err(e) => format!("{} unreadable: {e}", task_file.display()),
    };
    ev.tell(
        "tell-3",
        "a delegate still routed",
        if delivered {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "delegated `{delegate_sentinel}` from the orchestrator pane to `{ROLE_CODER}` and \
             `{ROLE_REVIEWER}`, typed into the orchestrator's own PTY through the BRANCH TUI's \
             pane-input path to the OLD daemon.\n\
             looked for `{pointer}` or `{delegate_sentinel}` on the `{ROLE_CODER}` pane: {}\n\
             {task_file_note}",
            if delivered { "found" } else { "NOT FOUND" }
        ),
    );

    // Hooks. `work-done` first, `agent-event` last: a bare `AgentEvent` makes
    // the daemon classify that pane's agent type as `Pi`, which routes prompt
    // delivery differently and makes a delivered delegate briefly look
    // undelivered.
    focus_role(&new_tui, ROLE_REVIEWER)?;
    let work_done_sentinel = format!("XVER-WORKDONE-{nonce}");
    type_into_pane(
        &new_tui,
        &format!("dot-agent-deck work-done --task \"{work_done_sentinel}\""),
    );
    focus_role(&new_tui, ROLE_ORCHESTRATOR)?;
    let feedback = format!("Worker {ROLE_REVIEWER} has completed their task");
    let work_done_arrived = new_tui.wait_for_grid(UI_TIMEOUT, |g| g.contains(&feedback));
    ev.excerpt(
        "orchestrator role pane after the worker's `work-done`",
        new_tui.grid(),
    );

    focus_role(&new_tui, ROLE_REVIEWER)?;
    type_into_pane(&new_tui, "dot-agent-deck agent-event --type running");
    let status_rows = wait_for_status(new_bin, env, &sb.project, DAEMON_TIMEOUT, |rows| {
        rows.iter()
            .any(|r| r.role.contains(ROLE_REVIEWER) && !r.status.is_empty() && r.status != "Idle")
    });
    let (status_ok, status_note) = match &status_rows {
        Ok(rows) => {
            let row = rows.iter().find(|r| r.role.contains(ROLE_REVIEWER));
            (
                true,
                format!(
                    "`daemon status --json`, asked by the BRANCH binary of the OLD daemon, reports \
                     the `{ROLE_REVIEWER}` pane as `{}`",
                    row.map(|r| r.status.clone()).unwrap_or_default()
                ),
            )
        }
        Err(e) => (false, format!("the status never changed: {e}")),
    };
    ev.tell(
        "tell-4",
        "hooks (work-done, status) still arrived",
        if work_done_arrived && status_ok {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "work-done: issued `dot-agent-deck work-done --task \"{work_done_sentinel}\"` from \
             inside the `{ROLE_REVIEWER}` pane; the daemon's feedback line \"{feedback}\" {} in \
             the orchestrator's pane.\n\
             status: issued `dot-agent-deck agent-event --type running` from inside the \
             `{ROLE_REVIEWER}` pane, LAST as rule 12 requires. {status_note}",
            if work_done_arrived {
                "appeared"
            } else {
                "did NOT appear"
            }
        ),
    );

    // --- tells 1 and 2, measured last so they cover the whole run ------------
    let listening = count_in_file(&sb.log, "Attach protocol listening");
    ev.tell(
        "tell-1",
        "exactly one `Attach protocol listening` line for the whole run",
        if listening == 1 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "{} matched in {}. Two would mean the branch TUI lazy-spawned its own daemon and this \
             was a meaningless same-version test — the tell for BOTH the no-agents cause and the \
             30-second idle-window cause.",
            match listening {
                1 => "1 line".to_string(),
                n => format!("{n} lines"),
            },
            sb.log.display()
        ),
    );

    let listeners_at_end = proc::listeners_on(attach_endpoint);
    let exe_now = proc::exe_path(daemon.pid);
    let same_exe = exe_now.as_deref() == Some(old_bin);
    let listener_verdict = match (&listeners_at_start, &listeners_at_end) {
        (Some(a), Some(b)) => Some(a == b && a.contains(&daemon.pid)),
        _ => None,
    };
    let (verdict, listener_note) = match listener_verdict {
        Some(true) if same_exe => (Verdict::Pass, "matched at both ends".to_string()),
        Some(false) => (
            Verdict::Fail,
            format!(
                "CHANGED: {listeners_at_start:?} at the start, {listeners_at_end:?} at the end"
            ),
        ),
        Some(true) => (
            Verdict::Fail,
            format!(
                "the listener pid matched but /proc/{}/exe is now {:?}, not the old binary",
                daemon.pid, exe_now
            ),
        ),
        None => (
            Verdict::NotChecked,
            "`ss -xlp` was unavailable or unparseable on this host, so who owns the endpoint was \
             not measured"
                .to_string(),
        ),
    };
    ev.tell(
        "tell-2",
        "the same daemon pid and the same build id served the run end to end",
        verdict,
        format!(
            "pid {} was alive at the start and {} at the end.\n\
             /proc/{}/exe -> {:?} (the {} binary is {})\n\
             the mismatch prompt the branch TUI printed reported the daemon's build id and its \
             own, so both sides' build ids were observed over the wire — see the excerpt.\n\
             `ss -xlp` on {}: {listener_note}",
            daemon.pid,
            if proc::is_alive(daemon.pid) {
                "alive"
            } else {
                "GONE"
            },
            daemon.pid,
            exe_now,
            opts.previous,
            old_bin.display(),
            attach_endpoint.display(),
        ),
    );

    if mode == EndpointMode::Resolved {
        // The change that makes `resolved` mode necessary (issue #1121) claims
        // its compatibility read is READ-ONLY: a branch build that found the old
        // daemon at the legacy address must never bind, create or unlink
        // anything at its own spelling. Recorded as an observation rather than
        // as a fifth tell — it is specific to one change's claim, where the four
        // tells are what rule 12 asks of every change.
        let own_dir = sandbox::fallback_endpoint_dir(&sb.tmp);
        ev.step(format!(
            "the branch build's own endpoint directory {} {} while it was attached to the old \
             daemon at the legacy address",
            own_dir.display(),
            if own_dir.exists() {
                "EXISTS — the branch build created its own spelling"
            } else {
                "does not exist — the compatibility read stayed read-only"
            }
        ));
    }

    ev.excerpt(
        "sandbox deck.log (tail)",
        tail(&std::fs::read_to_string(&sb.log).unwrap_or_default(), 80),
    );
    Ok(())
}

/// Drive the production new-pane flow to open the fixture's single
/// orchestration against the deck's current directory.
///
/// With no `[[modes]]` in the fixture the mode-chip row is
/// `[No mode] [Orch: xver] [schedule]`, so ONE Right selects the orchestration;
/// selecting one hides the Command field, so the second Enter submits.
fn open_orchestration(deck: &pty::PtyDeck) -> Result<(), String> {
    deck.send(b"\x0e"); // Ctrl+N -> directory picker
    std::thread::sleep(SETTLE);
    deck.send(b" "); // Space -> confirm the current dir -> new-pane form
    if !deck.wait_for_grid_string("No mode", STEP_TIMEOUT) {
        return Err(format!(
            "the new-pane form never appeared.\n=== grid ===\n{}",
            deck.grid()
        ));
    }
    deck.send(b"\x1b[C"); // Right -> [Orch: xver]
    if !deck.wait_for_grid_string("xver", STEP_TIMEOUT) {
        return Err(format!(
            "the orchestration chip never became selected.\n=== grid ===\n{}",
            deck.grid()
        ));
    }
    std::thread::sleep(SETTLE);
    deck.send(b"\r"); // Mode -> Name
    std::thread::sleep(SETTLE);
    deck.send(b"\r"); // submit
    Ok(())
}

/// Give keyboard focus to `role`'s pane and leave the deck in `PaneInput` mode
/// on it.
///
/// `Ctrl+D` returns to Normal mode, a digit jumps to that role's card, and
/// `focus_deck` re-enters `PaneInput` on success — so one digit both selects the
/// pane and makes it the one keystrokes reach. `PaneLayout::Stacked` draws only
/// the focused role's pane and fuses its title into the box corner as
/// `┌<role>`, so that string on the settled grid is what confirms the jump
/// landed.
///
/// Cycles tabs first when the deck is not on the orchestration tab: after a
/// reattach the deck lands wherever the previous session left it, and a digit on
/// the Dashboard tab means something else.
fn focus_role(deck: &pty::PtyDeck, role: &str) -> Result<(), String> {
    let digit: u8 = match role {
        ROLE_ORCHESTRATOR => b'1',
        ROLE_CODER => b'2',
        ROLE_REVIEWER => b'3',
        other => return Err(format!("no card index known for role {other}")),
    };
    let expanded = format!("┌{role}");
    for attempt in 0..6 {
        deck.send(b"\x04"); // Ctrl+D -> Normal mode
        std::thread::sleep(SETTLE);
        deck.send(&[digit]);
        if deck.wait_for_grid_string(&expanded, STEP_TIMEOUT) {
            return Ok(());
        }
        // Not on the orchestration tab (or not yet rebuilt): step right one tab
        // and try again.
        deck.send(b"\x04");
        std::thread::sleep(SETTLE);
        deck.send(b"\x1b[C");
        std::thread::sleep(SETTLE);
        if attempt == 5 {
            return Err(format!(
                "could not focus the `{role}` role pane (looked for {expanded:?}).\n=== grid ===\n{}",
                deck.grid()
            ));
        }
    }
    unreachable!("the loop returns or errors on its last iteration")
}

/// Type `text` into the focused pane and submit it.
///
/// The submit CR is a separate write after a pause: a CR fused to the preceding
/// text is treated as newline-in-input by agent TUIs, and the deck's own
/// `SUBMIT_DELAY` exists for the same reason.
fn type_into_pane(deck: &pty::PtyDeck, text: &str) {
    deck.send(text.as_bytes());
    std::thread::sleep(Duration::from_millis(300));
    deck.send(b"\r");
    std::thread::sleep(SETTLE);
}

/// Pull the build-version mismatch prompt out of a deck's byte history.
///
/// The prompt is printed in raw mode before the deck takes the alternate
/// screen, so it exists only in the stream — by the time a frame has been
/// painted the grid no longer holds it.
fn extract_prompt(stream: &str) -> String {
    let Some(start) = stream.find("Daemon version mismatch") else {
        return String::new();
    };
    let rest = &stream[start..];
    let end = rest
        .find("keep current daemon")
        .map(|i| i + "keep current daemon".len())
        .unwrap_or(rest.len().min(600));
    rest[..end].replace('\r', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_prompt_takes_the_whole_prompt_and_drops_the_carriage_returns() {
        let stream = "junk before\r\n\
             ⚠  Daemon version mismatch  (2 agent(s) running)\r\n\
             \x20  running daemon:  0.41.0-gc19c7d7\r\n\
             \x20  this binary:     0.41.0-g19385813\r\n\
             \x20  [S] restart daemon and continue   [any other key] keep current daemon\r\n\
             junk after";
        let got = extract_prompt(stream);
        assert!(got.starts_with("Daemon version mismatch"), "{got}");
        assert!(got.ends_with("keep current daemon"), "{got}");
        assert!(!got.contains('\r'));
        assert!(got.contains("0.41.0-gc19c7d7") && got.contains("0.41.0-g19385813"));
    }

    #[test]
    fn extract_prompt_is_empty_when_no_prompt_was_printed() {
        assert!(extract_prompt("no prompt here").is_empty());
    }

    const OLD_HELLO: &str = r#"{"ok":true,"server_version":10,"build_version":"0.41.0-gaaaaaaa","contract_breaks":["x"]}"#;

    #[test]
    fn identical_build_ids_are_refused_before_anything_is_started() {
        let err = compare_hellos(OLD_HELLO, OLD_HELLO).expect_err("a same-version pairing");
        assert!(err.contains("same-version run"), "{err}");
    }

    #[test]
    fn a_matching_protocol_version_is_reported_as_semantics_behind_a_stable_wire() {
        let new = OLD_HELLO.replace("gaaaaaaa", "gbbbbbbb");
        let notes = compare_hellos(OLD_HELLO, &new).expect("differing build ids");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("SEMANTICS behind a stable wire")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("CONTRACT_BREAKS identical")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_differing_protocol_version_is_reported_rather_than_refused() {
        let new = OLD_HELLO
            .replace("gaaaaaaa", "gbbbbbbb")
            .replace("\"server_version\":10", "\"server_version\":11");
        let notes = compare_hellos(OLD_HELLO, &new).expect("a protocol difference is not fatal");
        assert!(
            notes.iter().any(|n| n.contains("PROTOCOL_VERSION DIFFERS")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_differing_contract_break_list_names_the_desktop_surface_this_does_not_cover() {
        let new = OLD_HELLO
            .replace("gaaaaaaa", "gbbbbbbb")
            .replace(r#"["x"]"#, r#"["x","y"]"#);
        let notes = compare_hellos(OLD_HELLO, &new).expect("differing build ids");
        assert!(
            notes.iter().any(|n| n.contains("classify_handshake")),
            "{notes:?}"
        );
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("only", 5), "only");
    }

    #[test]
    fn count_in_file_counts_occurrences_and_tolerates_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("xver-count-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let f = dir.join("log");
        std::fs::write(
            &f,
            "Attach protocol listening on a\nnoise\nAttach protocol listening on b\n",
        )
        .expect("write");
        assert_eq!(count_in_file(&f, "Attach protocol listening"), 2);
        assert_eq!(count_in_file(&dir.join("absent"), "x"), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
