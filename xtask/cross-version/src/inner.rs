//! The half of a run that executes INSIDE the namespace: the daemon, both TUIs
//! and every deck CLI call are started from here, each client only after the
//! pre-connect assertion has held.
//!
//! The outer half (`main.rs`) builds the inputs, creates the sandbox, starts
//! `bwrap` with this binary as its command and answers the host-side half of
//! each pre-connect check over [`crate::ctl`]. This half proves its own
//! namespace, drives rule 12's scenario, tears it down by verified identity,
//! and hands its evidence back as JSON.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::time::{Duration, Instant};

use crate::isolation::{self, Plan};
use crate::proc::{self, Identity, Mismatch, SandboxProcess, Terminated};
use crate::pty;
use crate::report::{Evidence, RunVerdict, Verdict};
use crate::sandbox::{self, EndpointMode, Sandbox};
use crate::{ctl, epoch_secs};

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
/// How long the daemon gets after its one SIGTERM. Well past its 3 s agent
/// grace (`AGENT_TERMINATE_GRACE`).
const DAEMON_GRACE: Duration = Duration::from_secs(20);

const ROLE_ORCHESTRATOR: &str = "orchestrator";
const ROLE_CODER: &str = "coder";
const ROLE_REVIEWER: &str = "reviewer";

/// Why the inner half stopped early. The distinction decides the verdict: a
/// scenario that broke down is a FAIL (the branch and the release did not
/// interoperate, or the scenario could not be stood up — the run log says
/// which), while an isolation check that failed or could not be evaluated voids
/// the run as INCOMPLETE, because nothing measured inside a namespace that did
/// not hold is a measurement of the branch.
#[derive(Debug)]
pub enum Abort {
    Scenario(String),
    Isolation(String),
}

impl From<String> for Abort {
    fn from(s: String) -> Self {
        Abort::Scenario(s)
    }
}

impl From<&str> for Abort {
    fn from(s: &str) -> Self {
        Abort::Scenario(s.to_string())
    }
}

fn iso(msg: impl Into<String>) -> Abort {
    Abort::Isolation(msg.into())
}

/// Entry point for `xtask-cross-version --inner-plan <path>`.
pub fn main(plan_path: &Path) -> ExitCode {
    let plan: Plan = match std::fs::read_to_string(plan_path)
        .map_err(|e| format!("read {}: {e}", plan_path.display()))
        .and_then(|s| serde_json::from_str(&s).map_err(|e| format!("parse plan: {e}")))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("xver (inner): {e}");
            return ExitCode::FAILURE;
        }
    };
    let sb = plan.sandbox();
    let mut ev = Evidence::default();
    let mut ctl = ctl::Client::new();
    match body(&plan, &sb, &mut ctl, &mut ev) {
        Ok(()) => {}
        Err(Abort::Isolation(m)) => {
            ev.step(format!(
                "RUN ABORTED by an isolation check, before the next client connected: {m}"
            ));
            ev.isolation_failed(m);
        }
        Err(Abort::Scenario(m)) => {
            ev.step(format!("RUN ABORTED: {m}"));
            ev.tell("aborted", "the run did not complete", Verdict::Fail, m);
        }
    }
    let json = match serde_json::to_string_pretty(&ev) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("xver (inner): serialise evidence: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(sb.inner_evidence(), json) {
        eprintln!("xver (inner): write {}: {e}", sb.inner_evidence().display());
        return ExitCode::FAILURE;
    }
    if ev.verdict() == RunVerdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn body(plan: &Plan, sb: &Sandbox, ctl: &mut ctl::Client, ev: &mut Evidence) -> Result<(), Abort> {
    println!("xver (inner): prove the namespace");
    let mnt = proc::namespace("self", "mnt").ok_or_else(|| iso("cannot read /proc/self/ns/mnt"))?;
    let pid_ns =
        proc::namespace("self", "pid").ok_or_else(|| iso("cannot read /proc/self/ns/pid"))?;
    let net_ns =
        proc::namespace("self", "net").ok_or_else(|| iso("cannot read /proc/self/ns/net"))?;
    ctl.request(&format!("ns {mnt} {pid_ns} {net_ns}"))
        .map_err(|e| iso(format!("the outer half rejected this namespace: {e}")))?;
    for note in isolation::check_namespace(plan, &mnt).map_err(iso)? {
        ev.isolated(note);
    }

    // This process's own environment is what bwrap handed over. It must be the
    // plan's, entry for entry: that is what proves `--clearenv` held and
    // nothing of the caller's reached inside. bwrap adds exactly one entry of
    // its own after `--chdir` — `PWD`, set to the chdir target — which is
    // accepted only with that value. It goes no further: every deck process
    // is spawned with `env_clear` and the plan's entries alone.
    let mut own: Vec<(String, String)> = std::env::vars().collect();
    let root = sb.root.display().to_string();
    if let Some(pos) = own.iter().position(|(k, _)| k == "PWD") {
        if own[pos].1 != root {
            return Err(iso(format!(
                "bwrap's `PWD` is {:?}, not its --chdir target {root:?}",
                own[pos].1
            )));
        }
        own.remove(pos);
        ev.isolated(format!(
            "bwrap added one entry of its own to the inner half's environment, `PWD={root}` \
             (its --chdir target); it is not passed to any deck process"
        ));
    }
    own.sort();
    let mut want = plan.env.clone();
    want.sort();
    if own != want {
        let own_keys: Vec<&String> = own.iter().map(|(k, _)| k).collect();
        let want_keys: Vec<&String> = want.iter().map(|(k, _)| k).collect();
        let extra: Vec<_> = own_keys.iter().filter(|k| !want_keys.contains(k)).collect();
        let missing: Vec<_> = want_keys.iter().filter(|k| !own_keys.contains(k)).collect();
        return Err(iso(format!(
            "the inner half's environment is not exactly the plan's: extra {extra:?}, missing \
             {missing:?} (values are not printed)"
        )));
    }
    sandbox::check_env(&plan.env, sb, plan.env_spec()).map_err(iso)?;
    ev.isolated(format!(
        "the inner half's own environment is exactly the plan's {} allowlisted entries — \
         `--clearenv` held, and nothing of the caller's environment reached inside",
        plan.env.len()
    ));
    sandbox::verify_project(sb).map_err(iso)?;
    ev.isolated(format!(
        "`{}` is a regular file owned by this uid, so the deck's cwd-ancestor config walk stops \
         at the sandbox; `{}` is a directory, so the project is a standalone repository",
        sandbox::project_config(sb).display(),
        sb.project.join(".git").display()
    ));
    sandbox::check_socket_path_lengths(&plan.matrix)?;

    let old_bin = sb.old_bin();
    let new_bin = sb.new_bin();
    // `daemon hello` is a static print that connects to nothing; it still runs
    // here, with the run's environment, so no deck process in a run ever sees
    // the caller's.
    ev.old_hello = output_ok(
        deck(&old_bin, &plan.env, &sb.project, &["daemon", "hello"])?,
        "old daemon hello",
    )?;
    ev.new_hello = output_ok(
        deck(&new_bin, &plan.env, &sb.project, &["daemon", "hello"])?,
        "branch daemon hello",
    )?;
    println!("  · old {}", ev.old_hello.trim());
    println!("  · new {}", ev.new_hello.trim());
    for note in compare_hellos(&ev.old_hello, &ev.new_hello)? {
        ev.preflight.push(note);
    }

    drive(plan, sb, ctl, &mnt, &old_bin, &new_bin, ev)
}

fn output_ok(out: Output, what: &str) -> Result<String, Abort> {
    if !out.status.success() {
        return Err(Abort::Scenario(format!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// The pre-connect assertion
// ---------------------------------------------------------------------------

/// Everything the pre-connect assertion needs, and the tally of how often it
/// held.
struct Guard<'a> {
    plan: &'a Plan,
    sb: &'a Sandbox,
    recorded_mnt: String,
    daemon: Identity,
    ctl: RefCell<&'a mut ctl::Client>,
    counts: RefCell<BTreeMap<String, usize>>,
}

impl Guard<'_> {
    /// Run immediately before every deck client process is spawned, and before
    /// every deck command is typed into a pane. The harness's own clients are
    /// spawned directly, with no shell between the last check and their
    /// `execve`, and with exactly the environment checked here. A command typed
    /// into a pane is different on both counts: the pane's stand-in shell starts
    /// it, with the environment the daemon gave that pane — the daemon's own,
    /// verified against the plan as part of its identity, plus the deck's pane
    /// variables — so the environment half of this check covers it only through
    /// the daemon's.
    ///
    /// Any failure aborts before the client exists. What a failure CANNOT do is
    /// let a client fall through to a host endpoint, and that does not depend on
    /// this check winning a race: `/tmp` and `/run/user/<uid>` are private
    /// mounts, and the client's environment names no host path, so even a
    /// client that finds the expected socket gone between this check and its
    /// connect — the TOCTOU window — can only resolve onward to another
    /// sandbox address. There it would lazy-spawn a daemon of its own inside
    /// the namespace, which tell 1's `Attach protocol listening` count and the
    /// next pre-connect's second-listener check both catch.
    fn preconnect(
        &self,
        label: &str,
        client_env: &[(String, String)],
    ) -> Result<Vec<String>, Abort> {
        let plan = self.plan;
        isolation::check_namespace(plan, &self.recorded_mnt).map_err(iso)?;
        if client_env != plan.env.as_slice() {
            return Err(iso(format!(
                "{label}: the client environment is not the plan's"
            )));
        }
        sandbox::check_env(client_env, self.sb, plan.env_spec())
            .map_err(|e| iso(format!("{label}: {e}")))?;

        match self.daemon.verify() {
            Ok(()) => {}
            Err(Mismatch::Gone) => {
                return Err(Abort::Scenario(format!(
                    "{label}: the sandbox daemon (pid {}) is gone",
                    self.daemon.pid
                )));
            }
            Err(m) => {
                return Err(iso(format!(
                    "{label}: the sandbox daemon no longer matches its recorded identity: {m}"
                )));
            }
        }

        let listeners = proc::unix_listeners().map_err(|e| iso(format!("{label}: {e}")))?;
        let held =
            proc::socket_inodes(self.daemon.pid).map_err(|e| iso(format!("{label}: {e}")))?;
        let mut details = Vec::new();
        for path in &plan.matrix.owned {
            let inodes = isolation::listening_inodes(&listeners, path);
            if inodes.is_empty() {
                return Err(Abort::Scenario(format!(
                    "{label}: nothing is listening at {} — refusing to start a client that would \
                     resolve onward from there",
                    path.display()
                )));
            }
            if let Some(stray) = inodes.iter().find(|i| !held.contains(i)) {
                return Err(Abort::Scenario(format!(
                    "{label}: {} has a listener (inode {stray}) that the sandbox daemon pid {} \
                     does not hold",
                    path.display(),
                    self.daemon.pid
                )));
            }
            let md = std::fs::symlink_metadata(path)
                .map_err(|e| iso(format!("{label}: lstat {}: {e}", path.display())))?;
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            if !md.file_type().is_socket() || md.uid() != plan.uid || md.mode() & 0o077 != 0 {
                return Err(iso(format!(
                    "{label}: {} is not an owner-only socket of uid {} (type socket={}, uid {}, \
                     mode {:o})",
                    path.display(),
                    plan.uid,
                    md.file_type().is_socket(),
                    md.uid(),
                    md.mode() & 0o7777
                )));
            }
            details.push(format!(
                "`{}` → listening inode(s) {inodes:?}, held by the sandbox daemon pid {} \
                 (`/proc/{}/fd`); socket, uid {}, mode {:o}",
                path.display(),
                self.daemon.pid,
                self.daemon.pid,
                md.uid(),
                md.mode() & 0o7777
            ));
        }
        for path in &plan.matrix.absent {
            let on_disk = std::fs::symlink_metadata(path).is_ok();
            let listening = !isolation::listening_inodes(&listeners, path).is_empty();
            if on_disk || listening {
                return Err(Abort::Scenario(format!(
                    "{label}: {} must be ABSENT before a client connects, but it {}",
                    path.display(),
                    if listening {
                        "has a listener"
                    } else {
                        "exists"
                    }
                )));
            }
        }
        details.push(format!(
            "absent, as required (no file, no listener): {}",
            plan.matrix
                .absent
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(stray) = listeners.iter().find(|l| !held.contains(&l.inode)) {
            let owners = proc::owners_of(stray.inode).unwrap_or_default();
            return Err(Abort::Scenario(format!(
                "{label}: a listener this run did not start: `{}` (inode {}), held by {owners:?} — \
                 a second daemon inside the namespace",
                stray.path, stray.inode
            )));
        }
        // A second daemon is only reachable if it LISTENS, and every listener
        // was just proven to be the daemon's — so that check, not a command
        // line count, is what rules one out. Command lines cannot tell them
        // apart anyway: with a lifetime cap set, the daemon forks (does not
        // exec) a reaper per pane (`arm_child_group_backstop`), which keeps the
        // daemon's exe, command line and environment and never listens.
        let forks: Vec<i32> = proc::pids()
            .map_err(|e| iso(format!("{label}: {e}")))?
            .into_iter()
            .filter(|p| {
                *p != self.daemon.pid && proc::cmdline(*p).is_some_and(|c| c == self.daemon.cmdline)
            })
            .collect();
        details.push(format!(
            "every listening Unix socket in the run's network namespace ({}) is held by the \
             sandbox daemon, so no second daemon is reachable; {} other process(es) share its \
             command line {forks:?}, which is what its per-pane lifetime-cap reapers look like \
             (forked, not exec'd) and none of them holds a listener the daemon does not",
            listeners.len(),
            forks.len()
        ));

        self.ctl
            .borrow_mut()
            .request(&format!("preconnect {label}"))
            .map_err(|e| iso(format!("{label}: the outer half's host checks failed: {e}")))?;
        details.push(
            "outer half: the host's endpoint candidates are byte-for-byte as at baseline (file \
             identity, listening inodes, owners) and no host listener lies under the sandbox"
                .to_string(),
        );
        *self
            .counts
            .borrow_mut()
            .entry(label.to_string())
            .or_default() += 1;
        Ok(details)
    }

    /// The first time a label passes, its details go into the evidence; after
    /// that only the count does, or a polling loop would flood the file.
    fn preconnect_logged(
        &self,
        label: &str,
        client_env: &[(String, String)],
        ev: &mut Evidence,
    ) -> Result<(), Abort> {
        let first = !self.counts.borrow().contains_key(label);
        let details = self.preconnect(label, client_env)?;
        if first {
            ev.isolated(format!(
                "pre-connect assertion held before the first `{label}` client:\n    - {}",
                details.join("\n    - ")
            ));
        }
        Ok(())
    }

    fn summary(&self) -> String {
        let counts = self.counts.borrow();
        let total: usize = counts.values().sum();
        format!(
            "the pre-connect assertion held {total} time(s), once before every client: {}",
            counts
                .iter()
                .map(|(k, v)| format!("{k} ×{v}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Small process helpers
// ---------------------------------------------------------------------------

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

/// `daemon status --json`, projected to what this harness asserts on, behind
/// the pre-connect assertion.
///
/// Reads the daemon over `AttachRequest::ListAgents` and never lazily spawns
/// one, so an unreachable daemon is an `Err` here rather than a freshly-minted
/// empty daemon nobody asked for.
fn daemon_status(
    g: &Guard<'_>,
    bin: &Path,
    label: &str,
    ev: &mut Evidence,
) -> Result<Result<Vec<StatusRow>, String>, Abort> {
    g.preconnect_logged(label, &g.plan.env, ev)?;
    let out = deck(
        bin,
        &g.plan.env,
        &g.sb.project,
        &["daemon", "status", "--json"],
    )?;
    if !out.status.success() {
        return Ok(Err(format!(
            "daemon status --json exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let doc: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(d) => d,
        Err(e) => return Ok(Err(format!("daemon status --json is not JSON: {e}"))),
    };
    let Some(agents) = doc.get("agents").and_then(|a| a.as_array()) else {
        return Ok(Err("daemon status --json has no `agents` array".to_string()));
    };
    let field = |a: &serde_json::Value, k: &str| {
        a.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    Ok(Ok(agents
        .iter()
        .map(|a| StatusRow {
            pane_id: field(a, "pane_id"),
            role: field(a, "role"),
            status: field(a, "status"),
        })
        .collect()))
}

/// Poll `daemon status` until `pred` holds over its rows. Every poll is a client
/// and passes the pre-connect assertion first.
fn wait_for_status(
    g: &Guard<'_>,
    bin: &Path,
    label: &str,
    timeout: Duration,
    ev: &mut Evidence,
    pred: impl Fn(&[StatusRow]) -> bool,
) -> Result<Result<Vec<StatusRow>, String>, Abort> {
    let deadline = Instant::now() + timeout;
    loop {
        let last = daemon_status(g, bin, label, ev)?;
        if let Ok(rows) = &last
            && pred(rows)
        {
            return Ok(Ok(rows.clone()));
        }
        if Instant::now() >= deadline {
            return Ok(match last {
                Ok(rows) => Err(format!(
                    "daemon status never satisfied the condition within {timeout:?}; last rows: {rows:?}"
                )),
                Err(e) => Err(format!(
                    "daemon status kept failing within {timeout:?}: {e}"
                )),
            });
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

/// Wait for the daemon to bind every endpoint the matrix says it owns, reading
/// the kernel's table rather than starting a client to ask — a client started
/// before the daemon is listening is exactly the one that lazy-spawns.
fn wait_for_listeners(daemon: &mut SandboxProcess, owned: &[PathBuf]) -> Result<(), Abort> {
    let deadline = Instant::now() + DAEMON_TIMEOUT;
    loop {
        if daemon.has_exited() {
            return Err(Abort::Scenario(format!(
                "the sandbox daemon (pid {}) exited before it bound its endpoints — see \
                 artifacts/old-daemon.log",
                daemon.pid()
            )));
        }
        let listeners = proc::unix_listeners().map_err(iso)?;
        let held = proc::socket_inodes(daemon.pid()).unwrap_or_default();
        let bound = owned.iter().all(|p| {
            let inodes = isolation::listening_inodes(&listeners, p);
            !inodes.is_empty() && inodes.iter().all(|i| held.contains(i))
        });
        if bound {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Abort::Scenario(format!(
                "the sandbox daemon never bound {owned:?} within {DAEMON_TIMEOUT:?}; the \
                 namespace's listeners were {listeners:?}"
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One line of `ss`-style evidence for the attach endpoint, from the kernel's
/// table: which inodes listen there and which pids hold them.
fn kernel_owner_line(path: &Path) -> String {
    match proc::unix_listeners() {
        Ok(ls) => {
            let inodes = isolation::listening_inodes(&ls, path);
            let owners: Vec<(u64, Vec<i32>)> = inodes
                .iter()
                .map(|i| (*i, proc::owners_of(*i).unwrap_or_default()))
                .collect();
            format!(
                "kernel table: {} → (inode, holders) {owners:?}",
                path.display()
            )
        }
        Err(e) => format!("kernel table unreadable: {e}"),
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn drive(
    plan: &Plan,
    sb: &Sandbox,
    ctl: &mut ctl::Client,
    mnt: &str,
    old_bin: &Path,
    new_bin: &Path,
    ev: &mut Evidence,
) -> Result<(), Abort> {
    println!("xver (inner): step 1 — start the previous release's daemon");
    let mut daemon_cmd = Command::new(old_bin);
    daemon_cmd
        .args(["daemon", "serve"])
        .current_dir(&sb.project)
        .env_clear();
    for (k, v) in &plan.env {
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
    let mut daemon =
        SandboxProcess::adopt(child, old_bin, "sandbox daemon".to_string()).map_err(iso)?;
    ev.daemon_pid = daemon.pid();
    ev.daemon_endpoint = plan.matrix.attach().display().to_string();

    // The recorded identity must be exactly what was launched, or it is not an
    // identity worth gating a signal on.
    let id = daemon.identity.clone();
    let want_cmdline = vec![
        old_bin.display().to_string(),
        "daemon".to_string(),
        "serve".to_string(),
    ];
    let want_env: BTreeMap<String, String> = plan.env.iter().cloned().collect();
    let problems: Vec<String> = [
        (id.exe != old_bin).then(|| format!("exe {:?}", id.exe)),
        (id.cmdline != want_cmdline).then(|| format!("cmdline {:?}", id.cmdline)),
        (id.cwd != sb.project).then(|| format!("cwd {:?}", id.cwd)),
        (id.environ != want_env).then(|| "environment differs from the plan".to_string()),
        (id.mnt_ns != mnt).then(|| format!("mount namespace {}", id.mnt_ns)),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !problems.is_empty() {
        let body = Err(iso(format!(
            "the sandbox daemon's recorded identity is not what was launched: {}",
            problems.join("; ")
        )));
        return teardown(daemon, None, sb, ev, body);
    }
    ev.step(format!(
        "started the {} daemon as pid {} inside the namespace",
        plan.previous,
        daemon.pid()
    ));
    ev.isolated(format!(
        "sandbox daemon identity, recorded at spawn and re-verified before every client and \
         before its one signal: {}",
        id.summary()
    ));
    ev.excerpt(
        "the sandbox daemon's environment, read back from `/proc/<pid>/environ` (every entry)",
        isolation::render_env(&id.environ),
    );

    if let Err(e) = wait_for_listeners(&mut daemon, &plan.matrix.owned) {
        return teardown(daemon, None, sb, ev, Err(e));
    }
    let guard = Guard {
        plan,
        sb,
        recorded_mnt: mnt.to_string(),
        daemon: id,
        ctl: RefCell::new(ctl),
        counts: RefCell::new(BTreeMap::new()),
    };
    let mut new_tui = None;
    let body = scenario(&guard, &mut daemon, old_bin, new_bin, ev, &mut new_tui);
    let summary = guard.summary();
    drop(guard);
    ev.isolated(summary);
    teardown(daemon, new_tui, sb, ev, body)
}

/// Stop the branch TUI, then the daemon, then anything left in the namespace —
/// each by verified identity — and return `body` unchanged unless teardown
/// itself found an isolation problem.
fn teardown(
    mut daemon: SandboxProcess,
    new_tui: Option<pty::PtyDeck>,
    sb: &Sandbox,
    ev: &mut Evidence,
    body: Result<(), Abort>,
) -> Result<(), Abort> {
    println!("xver (inner): teardown");
    if let Some(mut tui) = new_tui {
        ev.step(tui.shutdown());
    }
    let label = daemon.label.clone();
    let pid = daemon.pid();
    match daemon.terminate(DAEMON_GRACE) {
        Ok(Terminated::Graceful) => ev.step(format!(
            "sent ONE SIGTERM to the {label} (pid {pid}) after re-verifying its start time, exe, \
             exact cmdline, cwd, full environment and mount namespace; it exited within \
             {DAEMON_GRACE:?}"
        )),
        Ok(Terminated::Killed) => ev.step(format!(
            "the {label} (pid {pid}) outlived its SIGTERM by {DAEMON_GRACE:?}; its identity was \
             re-verified and it got SIGKILL"
        )),
        Err(Mismatch::Gone) => ev.step(format!("{label} pid {pid} was already gone")),
        Err(m) => ev.step(format!(
            "REFUSED to signal the {label} (pid {pid}): {m}. Nothing was signalled; the \
             namespace's exit reaps it."
        )),
    }
    let swept = sweep_survivors(sb, ev);
    match (body, swept) {
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Everything still alive in the namespace after the daemon is gone.
///
/// A stand-in that `setsid`s out of its group survives its parent, so this is a
/// census rather than an assumption. Each survivor is signalled individually,
/// and only after its identity is recorded and it carries this run's
/// `DAD_XVER_SANDBOX` marker; one that does not is left to the namespace's exit,
/// which kills every remaining process in it. Pid 1 is bwrap's own init and is
/// never signalled.
fn sweep_survivors(sb: &Sandbox, ev: &mut Evidence) -> Result<(), Abort> {
    let me = std::process::id() as i32;
    let marker = sb.root.display().to_string();
    let mut notes = Vec::new();
    for pid in proc::pids().map_err(iso)? {
        if pid == me || pid == 1 {
            continue;
        }
        if proc::state(pid) == Some('Z') {
            notes.push(format!("pid {pid}: zombie, reaped by the namespace's init"));
            continue;
        }
        let Ok(id) = Identity::capture(pid) else {
            notes.push(format!(
                "pid {pid}: identity unreadable; left to the namespace's exit"
            ));
            continue;
        };
        if id.environ.get("DAD_XVER_SANDBOX") != Some(&marker) {
            notes.push(format!(
                "pid {pid} ({:?}): no run marker in its environment; not signalled, left to the \
                 namespace's exit",
                id.cmdline
            ));
            continue;
        }
        // SAFETY: positive pid, identity captured one step ago, inside the
        // run's private PID namespace.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && id.verify().is_ok() {
            std::thread::sleep(Duration::from_millis(100));
        }
        if id.verify().is_ok() {
            // SAFETY: re-verified immediately above.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            notes.push(format!(
                "pid {pid} ({:?}): SIGTERM, then SIGKILL after re-verification",
                id.cmdline
            ));
        } else {
            notes.push(format!("pid {pid} ({:?}): SIGTERM", id.cmdline));
        }
    }
    let listeners = proc::unix_listeners().map_err(iso)?;
    ev.isolated(format!(
        "after teardown: {} survivor(s) in the PID namespace{}; {} listening socket(s) left in \
         the network namespace",
        notes.len(),
        if notes.is_empty() {
            String::new()
        } else {
            format!(" — {}", notes.join("; "))
        },
        listeners.len()
    ));
    Ok(())
}

/// Steps 2–5 and the four tells. The branch TUI is parked in `new_tui_slot` as
/// soon as it exists, so teardown can stop it by identity on success and on
/// failure alike rather than leaving it to a destructor.
fn scenario(
    g: &Guard<'_>,
    daemon: &mut SandboxProcess,
    old_bin: &Path,
    new_bin: &Path,
    ev: &mut Evidence,
    new_tui_slot: &mut Option<pty::PtyDeck>,
) -> Result<(), Abort> {
    let plan = g.plan;
    let sb = g.sb;
    let fail = |e: Abort| e;
    let three_roles = |rows: &[StatusRow]| {
        [ROLE_ORCHESTRATOR, ROLE_CODER, ROLE_REVIEWER]
            .iter()
            .all(|r| rows.iter().any(|row| row.role.contains(*r)))
    };

    // The old binary asking its own daemon is the version-correct way to find
    // out whether it is up, in every endpoint mode.
    daemon_status(g, old_bin, "old CLI: daemon status", ev)
        .map_err(fail)?
        .map_err(|e| {
            fail(format!("the sandbox daemon never answered `daemon status`: {e}").into())
        })?;
    ev.step("the sandbox daemon answered `daemon status`");
    let listeners_at_start = proc::listeners_on(plan.matrix.attach());
    let kernel_at_start = kernel_owner_line(plan.matrix.attach());

    println!("xver (inner): step 2 — bring an orchestration up under it, with the OLD TUI");
    g.preconnect_logged("old TUI", &plan.env, ev)
        .map_err(fail)?;
    let mut old_tui = pty::PtyDeck::spawn(pty::PtySpec {
        label: "old-tui",
        bin: old_bin,
        args: &[],
        cwd: &sb.project,
        env: &plan.env,
        cols: 200,
        rows: 55,
        stream_log: sb.artifacts.join("old-tui.stream.txt"),
    })
    .map_err(|e| fail(e.into()))?;
    if !old_tui.wait_for_grid_string("No active sessions", UI_TIMEOUT) {
        return Err(fail(
            format!(
                "{}: never reached an empty dashboard.\n=== grid ===\n{}",
                old_tui.label,
                old_tui.grid()
            )
            .into(),
        ));
    }
    open_orchestration(&old_tui).map_err(|e| fail(e.into()))?;
    let rows = wait_for_status(g, old_bin, "old CLI: daemon status", UI_TIMEOUT, ev, three_roles)
        .map_err(fail)?
        .map_err(|e| {
            fail(
                format!(
                    "the three role panes never came up under the sandbox daemon: {e}\n=== old TUI grid ===\n{}",
                    old_tui.grid()
                )
                .into(),
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

    println!("xver (inner): step 3 — Ctrl+D, Ctrl+C, Detach (never Stop)");
    old_tui.send(b"\x04"); // Ctrl+D — leave PaneInput. Without this Ctrl+C goes
    std::thread::sleep(SETTLE); // to the focused PANE and kills a role.
    old_tui.send(b"\x03"); // Ctrl+C — the quit dialog
    if !old_tui.wait_for_grid_string("Quit dot-agent-deck?", STEP_TIMEOUT) {
        return Err(fail(
            format!(
                "Ctrl+D then Ctrl+C never opened the quit dialog in the old TUI.\n=== grid ===\n{}",
                old_tui.grid()
            )
            .into(),
        ));
    }
    old_tui.send(b"\r"); // Enter on the default option, which is Detach (index 0).
    if old_tui.wait_for_exit(STEP_TIMEOUT).is_none() {
        return Err(fail(
            format!(
                "the old TUI did not exit after choosing Detach.\n=== grid ===\n{}",
                old_tui.grid()
            )
            .into(),
        ));
    }
    ev.step(old_tui.shutdown());
    drop(old_tui);

    let after_detach = wait_for_status(
        g,
        old_bin,
        "old CLI: daemon status",
        DAEMON_TIMEOUT,
        ev,
        three_roles,
    )
    .map_err(fail)?
    .map_err(|e| {
        fail(
            format!(
                "after the detach the daemon no longer lists all three roles — the Ctrl+C trap \
                     (it goes to the PANE in PaneInput mode) or a `Stop` instead of `Detach`: {e}"
            )
            .into(),
        )
    })?;
    if daemon.has_exited() {
        return Err(fail(
            format!(
                "the sandbox daemon (pid {}) died during the detach — `Detach` must leave it running",
                daemon.pid()
            )
            .into(),
        ));
    }
    ev.step(format!(
        "after Detach the daemon (pid {}) is still alive and still lists {} role panes",
        daemon.pid(),
        after_detach.len()
    ));

    println!("xver (inner): step 4 — attach the BRANCH TUI and decline the mismatch prompt");
    g.preconnect_logged("branch TUI", &plan.env, ev)
        .map_err(fail)?;
    let fallback_arm = plan.mode == EndpointMode::Resolved && !plan.keep_xdg_runtime_dir;
    if fallback_arm {
        ev.isolated(format!(
            "fallback arm: the branch TUI starts with no `XDG_RUNTIME_DIR` and no socket override \
             in its environment, its own primary fallback `{}` is absent (no file, no listener), \
             and the only deck endpoints listening anywhere in the namespace are the {} daemon's \
             legacy flat `{}` and `{}`. The only way the branch TUI can reach a daemon is the \
             compatibility read of the literal `/tmp` — the fallback arm issue #1121 changed.",
            sandbox::per_uid_dir(plan.uid).display(),
            plan.previous,
            plan.matrix.owned[0].display(),
            plan.matrix.owned[1].display()
        ));
    }
    let new_tui = new_tui_slot.insert(
        pty::PtyDeck::spawn(pty::PtySpec {
            label: "new-tui",
            bin: new_bin,
            args: &[],
            cwd: &sb.project,
            env: &plan.env,
            cols: 200,
            rows: 55,
            stream_log: sb.artifacts.join("new-tui.stream.txt"),
        })
        .map_err(|e| fail(e.into()))?,
    );
    with_branch_tui(
        g,
        daemon,
        new_tui,
        old_bin,
        new_bin,
        ev,
        listeners_at_start,
        kernel_at_start,
        fallback_arm,
    )
}

#[allow(clippy::too_many_arguments)]
fn with_branch_tui(
    g: &Guard<'_>,
    daemon: &mut SandboxProcess,
    new_tui: &pty::PtyDeck,
    old_bin: &Path,
    new_bin: &Path,
    ev: &mut Evidence,
    listeners_at_start: Option<Vec<i32>>,
    kernel_at_start: String,
    fallback_arm: bool,
) -> Result<(), Abort> {
    let plan = g.plan;
    let sb = g.sb;
    let saw_prompt = new_tui.wait_for_stream_string("Daemon version mismatch", UI_TIMEOUT);
    let prompt_excerpt = extract_prompt(&new_tui.stream_text());
    if !saw_prompt {
        ev.excerpt(
            "branch TUI stream (no mismatch prompt)",
            tail(&new_tui.stream_text(), 60),
        );
        return Err(Abort::Scenario(format!(
            "the branch TUI never printed the build-version mismatch prompt within {UI_TIMEOUT:?}. \
             That prompt appearing IS the proof the scenario was reached: either the branch TUI \
             did not find the old daemon at all (and has lazy-spawned its own — check the \
             `Attach protocol listening` count below), or the two builds report the same build \
             id. old={} new={}",
            ev.old_hello.trim(),
            ev.new_hello.trim()
        )));
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
        return Err(Abort::Scenario(format!(
            "{}: after declining the prompt it never rendered the orchestration.\n=== grid ===\n{}",
            new_tui.label,
            new_tui.grid()
        )));
    }
    ev.step("declined the prompt (`n`); the branch TUI attached to the OLD daemon unchanged");

    println!("xver (inner): step 5 — delegate first, hooks last");
    focus_role(new_tui, ROLE_ORCHESTRATOR)?;
    let nonce = epoch_secs();
    let delegate_sentinel = format!("XVER-DELEGATE-{nonce}");
    // The pane's shell is about to start the branch CLI, which is a client.
    g.preconnect_logged("pane: dot-agent-deck delegate", &plan.env, ev)?;
    type_into_pane(
        new_tui,
        &format!(
            "dot-agent-deck delegate --to {ROLE_CODER} --to {ROLE_REVIEWER} --task \"{delegate_sentinel} list the files in this directory\""
        ),
    );

    // Delivery is asserted on the PAYLOAD, in the target pane, not on the CLI's
    // exit code. `coder` is `cat`, so whatever the daemon wrote into its PTY is
    // echoed straight back onto the screen.
    focus_role(new_tui, ROLE_CODER)?;
    let pointer = format!("worker-task-{ROLE_CODER}.md");
    let delivered = new_tui.wait_for_grid(UI_TIMEOUT, |g| {
        g.contains(&pointer) || g.contains(&delegate_sentinel)
    });
    ev.excerpt("`coder` role pane after the delegate", new_tui.grid());
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
    focus_role(new_tui, ROLE_REVIEWER)?;
    let work_done_sentinel = format!("XVER-WORKDONE-{nonce}");
    g.preconnect_logged("pane: dot-agent-deck work-done", &plan.env, ev)?;
    type_into_pane(
        new_tui,
        &format!("dot-agent-deck work-done --task \"{work_done_sentinel}\""),
    );
    focus_role(new_tui, ROLE_ORCHESTRATOR)?;
    let feedback = format!("Worker {ROLE_REVIEWER} has completed their task");
    let work_done_arrived = new_tui.wait_for_grid(UI_TIMEOUT, |g| g.contains(&feedback));
    ev.excerpt(
        "orchestrator role pane after the worker's `work-done`",
        new_tui.grid(),
    );

    focus_role(new_tui, ROLE_REVIEWER)?;
    g.preconnect_logged("pane: dot-agent-deck agent-event", &plan.env, ev)?;
    type_into_pane(new_tui, "dot-agent-deck agent-event --type running");
    let status_rows = wait_for_status(
        g,
        new_bin,
        "branch CLI: daemon status",
        DAEMON_TIMEOUT,
        ev,
        |rows| {
            rows.iter().any(|r| {
                r.role.contains(ROLE_REVIEWER) && !r.status.is_empty() && r.status != "Idle"
            })
        },
    )?;
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

    let attach = plan.matrix.attach();
    let listeners_at_end = proc::listeners_on(attach);
    let kernel_at_end = kernel_owner_line(attach);
    let exe_now = proc::exe_path(daemon.pid());
    let same_exe = exe_now.as_deref() == Some(old_bin);
    let alive = !daemon.has_exited();
    let listener_verdict = match (&listeners_at_start, &listeners_at_end) {
        (Some(a), Some(b)) => Some(a == b && a.contains(&daemon.pid())),
        _ => None,
    };
    let (verdict, listener_note) = match listener_verdict {
        Some(true) if same_exe && alive => (Verdict::Pass, "matched at both ends".to_string()),
        Some(false) => (
            Verdict::Fail,
            format!(
                "CHANGED: {listeners_at_start:?} at the start, {listeners_at_end:?} at the end"
            ),
        ),
        Some(true) => (
            Verdict::Fail,
            format!(
                "the listener pid matched but the daemon is {} and /proc/{}/exe is {:?}, not the \
                 old binary",
                if alive { "alive" } else { "GONE" },
                daemon.pid(),
                exe_now
            ),
        ),
        None => (
            Verdict::NotChecked,
            "`ss -xlp` was unavailable or unparseable inside the namespace, so who owns the \
             endpoint was not measured"
                .to_string(),
        ),
    };
    ev.tell(
        "tell-2",
        "the same daemon pid and the same build id served the run end to end",
        verdict,
        format!(
            "pid {} was alive at the start and {} at the end (pids are the run's private PID \
             namespace's).\n\
             /proc/{}/exe -> {:?} (the {} binary is {})\n\
             the mismatch prompt the branch TUI printed reported the daemon's build id and its \
             own, so both sides' build ids were observed over the wire — see the excerpt.\n\
             `ss -xlp` on {}, inside the run's network namespace: {listener_note}\n\
             at the start, {kernel_at_start}\n\
             at the end, {kernel_at_end}",
            daemon.pid(),
            if alive { "alive" } else { "GONE" },
            daemon.pid(),
            exe_now,
            plan.previous,
            old_bin.display(),
            attach.display(),
        ),
    );

    if plan.mode == EndpointMode::Resolved {
        // The change that makes `resolved` mode necessary (issue #1121) claims
        // its compatibility read is READ-ONLY: a branch build that found the old
        // daemon at the legacy address must never bind, create or unlink
        // anything at its own spelling. Recorded as an observation rather than
        // as a fifth tell — it is specific to one change's claim, where the four
        // tells are what rule 12 asks of every change.
        let own_dir = sandbox::per_uid_dir(plan.uid);
        ev.step(format!(
            "the branch build's own endpoint directory {} (inside the namespace; `{}` outside) {} \
             while it was attached to the old daemon",
            own_dir.display(),
            sb.fallback_tmp
                .join(own_dir.file_name().unwrap_or_default())
                .display(),
            if own_dir.exists() {
                "EXISTS — the branch build created its own spelling"
            } else {
                "does not exist — the compatibility read stayed read-only"
            }
        ));
    }
    if fallback_arm {
        let reached = ev
            .tells
            .iter()
            .filter(|t| t.id == "tell-1" || t.id == "tell-2")
            .all(|t| t.verdict == Verdict::Pass);
        ev.isolated(if reached {
            format!(
                "fallback arm confirmed: with the matrix above holding before the branch TUI \
                 started, tell 1 (no second daemon) and tell 2 (the same listener on `{}` at both \
                 ends) passing mean the branch TUI reached the {} daemon through the legacy flat \
                 address",
                attach.display(),
                plan.previous
            )
        } else {
            "fallback arm NOT confirmed: tell 1 or tell 2 did not pass, so which daemon the branch \
             TUI reached is not established"
                .to_string()
        });
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

/// One field of a `daemon hello` document.
fn hello_field<'a>(doc: &'a serde_json::Value, key: &str) -> &'a str {
    doc.get(key).and_then(|v| v.as_str()).unwrap_or("<absent>")
}

/// Compare the two builds' self-reported contracts before a single deck
/// process is started, and refuse the one comparison that makes the whole run
/// vacuous.
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
pub fn compare_hellos(old_raw: &str, new_raw: &str) -> Result<Vec<String>, String> {
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
             harness's clothes — refusing before any deck process is started. Check that \
             --branch really differs from --previous, and that the branch binary was rebuilt."
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

    #[test]
    fn a_plain_string_error_is_a_scenario_abort_not_an_isolation_one() {
        assert!(matches!(Abort::from("x".to_string()), Abort::Scenario(_)));
        assert!(matches!(iso("y"), Abort::Isolation(_)));
    }
}
