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
use crate::sandbox::{self, Direction, EndpointMatrix, EndpointMode, Sandbox};
use crate::{ctl, epoch_secs, probes};

/// How long to wait for the deck to paint its first frame, for the mismatch
/// prompt, and for an orchestration to come up. Generous because this box is
/// shared and a run competing with three dispatched units is the normal case.
pub(crate) const UI_TIMEOUT: Duration = Duration::from_secs(90);
/// How long to wait for a single keystroke's consequence.
pub(crate) const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the daemon's own state to catch up with a CLI call.
pub(crate) const DAEMON_TIMEOUT: Duration = Duration::from_secs(45);
/// Settle pause between a key and the next one, where the deck has to redraw in
/// between and there is nothing specific to poll for.
pub(crate) const SETTLE: Duration = Duration::from_millis(400);
/// How long the daemon gets after its one SIGTERM. Well past its 3 s agent
/// grace (`AGENT_TERMINATE_GRACE`).
pub(crate) const DAEMON_GRACE: Duration = Duration::from_secs(20);

pub(crate) const ROLE_ORCHESTRATOR: &str = "orchestrator";
pub(crate) const ROLE_CODER: &str = "coder";
pub(crate) const ROLE_REVIEWER: &str = "reviewer";

/// Why the inner half stopped early. The distinction decides the verdict: a
/// scenario that broke down is a FAIL (the branch and the release did not
/// interoperate, or the scenario could not be stood up — the run log says
/// which), while an isolation check that failed or could not be evaluated voids
/// the run as INCOMPLETE, because nothing measured inside a namespace that did
/// not hold is a measurement of the branch.
#[derive(Debug)]
pub(crate) enum Abort {
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

pub(crate) fn iso(msg: impl Into<String>) -> Abort {
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
    sandbox::check_env(&plan.env, sb, plan.env_spec(), &plan.extra_env).map_err(iso)?;
    ev.isolated(format!(
        "the inner half's own environment is exactly the plan's {} allowlisted entries — \
         `--clearenv` held, and nothing of the caller's environment reached inside",
        plan.env.len()
    ));
    sandbox::verify_project(sb, &plan.fixture).map_err(iso)?;
    ev.isolated(format!(
        "`{}` is a regular file owned by this uid, so the deck's cwd-ancestor config walk stops \
         at the sandbox; `{}` is a directory, so the project is a standalone repository",
        sandbox::project_config(sb).display(),
        sb.project.join(".git").display()
    ));
    for m in &plan.matrices {
        sandbox::check_socket_path_lengths(m)?;
    }

    let old_bin = sb.old_bin();
    let new_bin = sb.new_bin(plan.direction);
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

    let cast = Cast::for_plan(plan, sb);
    drive(plan, sb, ctl, &mnt, &cast, ev)
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

/// A daemon this run did not start but has measured and identified: in a
/// reverse run, the one the previous release's client lazy-spawns when it
/// cannot find the branch daemon. Recorded so the pre-connect assertion can
/// account for its listeners by identity rather than refuse every later client,
/// and so teardown can stop it by that identity.
#[derive(Clone, Debug)]
pub struct SecondDaemon {
    pub identity: Identity,
    /// The endpoints its listeners are bound to.
    pub paths: Vec<PathBuf>,
}

/// Everything the pre-connect assertion needs, and the tally of how often it
/// held.
pub(crate) struct Guard<'a> {
    pub plan: &'a Plan,
    pub sb: &'a Sandbox,
    recorded_mnt: String,
    pub daemon: Identity,
    /// The endpoint matrix selected once the daemon was up (see
    /// [`EndpointMatrix::candidates`]).
    pub matrix: EndpointMatrix,
    pub second: RefCell<Option<SecondDaemon>>,
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
    ///
    /// A [`SecondDaemon`] the run has already measured and identified is the
    /// one exception to "every listener is the daemon's": its listeners are
    /// accepted only at the paths it was recorded on, and only while its own
    /// full identity still verifies.
    pub(crate) fn preconnect(
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
        sandbox::check_env(client_env, self.sb, plan.env_spec(), &plan.extra_env)
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
        let second = self.second.borrow().clone();
        let second_held = match &second {
            Some(s) => {
                s.identity.verify().map_err(|m| {
                    iso(format!(
                        "{label}: the recorded second daemon (pid {}) no longer matches its \
                         identity: {m}",
                        s.identity.pid
                    ))
                })?;
                proc::socket_inodes(s.identity.pid).map_err(|e| iso(format!("{label}: {e}")))?
            }
            None => Default::default(),
        };
        let mut details = Vec::new();
        for path in &self.matrix.owned {
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
        let second_paths: Vec<PathBuf> =
            second.as_ref().map(|s| s.paths.clone()).unwrap_or_default();
        for path in self
            .matrix
            .absent
            .iter()
            .filter(|p| !second_paths.contains(p))
        {
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
            self.matrix
                .absent
                .iter()
                .filter(|p| !second_paths.contains(p))
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(stray) = listeners.iter().find(|l| {
            !held.contains(&l.inode)
                && !(second_held.contains(&l.inode)
                    && second_paths.iter().any(|p| p.to_string_lossy() == l.path))
        }) {
            let owners = proc::owners_of(stray.inode).unwrap_or_default();
            return Err(Abort::Scenario(format!(
                "{label}: a listener this run did not start: `{}` (inode {}), held by {owners:?} — \
                 a second daemon inside the namespace",
                stray.path, stray.inode
            )));
        }
        if let Some(s) = &second {
            details.push(format!(
                "the recorded second daemon (pid {}) still verifies and its listeners are only at \
                 {:?}",
                s.identity.pid, s.paths
            ));
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
             sandbox daemon{}, so no other daemon is reachable; {} other process(es) share its \
             command line {forks:?}, which is what its per-pane lifetime-cap reapers look like \
             (forked, not exec'd) and none of them holds a listener the daemon does not",
            listeners.len(),
            if second.is_some() {
                " or by the recorded second daemon at its recorded paths"
            } else {
                ""
            },
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
    pub(crate) fn preconnect_logged(
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

/// One row of `daemon status --json`, in the shape every build since
/// `schema_version` 2 prints: absent keys read as empty strings.
#[derive(Debug, Clone)]
pub(crate) struct StatusRow {
    pub agent_id: String,
    pub pane_id: String,
    pub role: String,
    pub status: String,
    pub cwd: String,
    /// `active_tool.name`.
    pub tool: String,
}

/// `daemon status --json`, projected to what this harness asserts on, behind
/// the pre-connect assertion.
///
/// Reads the daemon over `AttachRequest::ListAgents` and never lazily spawns
/// one, so an unreachable daemon is an `Err` here rather than a freshly-minted
/// empty daemon nobody asked for.
pub(crate) fn daemon_status(
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
            agent_id: field(a, "agent_id"),
            pane_id: field(a, "pane_id"),
            role: field(a, "role"),
            status: field(a, "status"),
            cwd: field(a, "cwd"),
            tool: a
                .get("active_tool")
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect()))
}

/// Poll `daemon status` until `pred` holds over its rows. Every poll is a client
/// and passes the pre-connect assertion first.
pub(crate) fn wait_for_status(
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

pub(crate) fn count_in_file(path: &Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.matches(needle).count())
        .unwrap_or(0)
}

/// The last `n` lines of `s` — what a failure message should quote rather than
/// the whole scrollback.
pub(crate) fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Wait for the daemon to bind every endpoint one of the candidate matrices
/// says it owns, reading the kernel's table rather than starting a client to
/// ask — a client started before the daemon is listening is exactly the one
/// that lazy-spawns. Returns the candidate the daemon bound.
///
/// With one candidate this is "wait for the predicted pair". With two (a
/// reverse `resolved` run, see [`EndpointMatrix::candidates`]) it is "wait for
/// either pair", and the other pair is then held ABSENT by every pre-connect
/// assertion from here on, exactly as a predicted matrix would hold it.
fn wait_for_listeners(
    daemon: &mut SandboxProcess,
    candidates: &[EndpointMatrix],
    log_name: &str,
) -> Result<EndpointMatrix, Abort> {
    let deadline = Instant::now() + DAEMON_TIMEOUT;
    loop {
        if daemon.has_exited() {
            return Err(Abort::Scenario(format!(
                "the sandbox daemon (pid {}) exited before it bound its endpoints — see \
                 artifacts/{log_name}",
                daemon.pid()
            )));
        }
        let listeners = proc::unix_listeners().map_err(iso)?;
        let held = proc::socket_inodes(daemon.pid()).unwrap_or_default();
        let bound = candidates.iter().find(|m| {
            m.owned.iter().all(|p| {
                let inodes = isolation::listening_inodes(&listeners, p);
                !inodes.is_empty() && inodes.iter().all(|i| held.contains(i))
            })
        });
        if let Some(m) = bound {
            return Ok(m.clone());
        }
        if Instant::now() >= deadline {
            return Err(Abort::Scenario(format!(
                "the sandbox daemon never bound any of {:?} within {DAEMON_TIMEOUT:?}; the \
                 namespace's listeners were {listeners:?}",
                candidates.iter().map(|m| &m.owned).collect::<Vec<_>>()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One line of `ss`-style evidence for the attach endpoint, from the kernel's
/// table: which inodes listen there and which pids hold them.
pub(crate) fn kernel_owner_line(path: &Path) -> String {
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
// Who plays which part
// ---------------------------------------------------------------------------

/// Which build plays which part in a run.
///
/// Forward is rule 12's pairing: the previous release serves the daemon and
/// stands the orchestration up under it; the branch attaches second, declines
/// the prompt, and is the CLI every pane command and the final status query
/// use. Reverse swaps the two builds and changes nothing else about the
/// scenario — which is the point: the same steps and the same tells, with the
/// branch's own daemon code now the code under test.
pub(crate) struct Cast {
    pub direction: Direction,
    /// Serves the daemon, and is the TUI and CLI that stand the orchestration
    /// up under it.
    pub daemon_bin: PathBuf,
    /// `old` or `branch`, for labels.
    pub daemon_side: &'static str,
    /// `v0.41.0` or `branch`, for prose.
    pub daemon_build: String,
    /// The TUI that attaches second and declines the prompt, and the CLI every
    /// pane command and the final status query use.
    pub client_bin: PathBuf,
    pub client_side: &'static str,
    pub client_build: String,
    /// What is typed into a pane to run the client CLI. Forward: the bare name,
    /// which `PATH` resolves to the staged branch build. Reverse: the old
    /// build's absolute path, so the evidence names exactly which binary sent
    /// each stimulus (a bare name there would reach the same build, staged on
    /// `PATH`, under a second path).
    pub pane_cli: String,
    pub pane_cli_label: &'static str,
}

impl Cast {
    pub fn for_plan(plan: &Plan, sb: &Sandbox) -> Self {
        match plan.direction {
            Direction::Forward => Cast {
                direction: Direction::Forward,
                daemon_bin: sb.old_bin(),
                daemon_side: "old",
                daemon_build: plan.previous.clone(),
                client_bin: sb.new_bin(Direction::Forward),
                client_side: "branch",
                client_build: "branch".to_string(),
                pane_cli: "dot-agent-deck".to_string(),
                pane_cli_label: "dot-agent-deck",
            },
            Direction::Reverse => Cast {
                direction: Direction::Reverse,
                daemon_bin: sb.new_bin(Direction::Reverse),
                daemon_side: "branch",
                daemon_build: "branch".to_string(),
                client_bin: sb.old_bin(),
                client_side: "old",
                client_build: plan.previous.clone(),
                pane_cli: sb.old_bin().display().to_string(),
                pane_cli_label: "old dot-agent-deck",
            },
        }
    }

    /// The PTY label a side's TUI gets — also its stream file's name.
    fn tui_label(side: &str) -> &'static str {
        if side == "old" { "old-tui" } else { "new-tui" }
    }
}

/// The key that declines the build-version mismatch prompt.
///
/// Established per build rather than assumed symmetric, because the reverse
/// run puts the OLD build's prompt in front of it. Both sides render the same
/// prompt — `src/build_version_handshake.rs` is byte-identical between v0.41.0
/// and every branch this sweeps, and the published v0.41.0 binary's strings
/// carry `[S] restart daemon and continue   [any other key] keep current
/// daemon` — and both implement it in `interactive_prompt`: `s`/`S` without
/// Ctrl is the ONLY affirmative key and every other key declines. The run
/// itself is the behavioural check: the prompt is recorded as the client
/// printed it, and tells 1 and 2 then prove the client stayed attached to the
/// same daemon rather than restarting it.
const DECLINE_KEY: &[u8] = b"n";

/// Every role the run's fixture defines, in card order.
pub(crate) fn roles(plan: &Plan) -> Vec<String> {
    let mut v: Vec<String> = [ROLE_ORCHESTRATOR, ROLE_CODER, ROLE_REVIEWER]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let sb = plan.sandbox();
    v.extend(plan.probe.extra_roles(&sb).into_iter().map(|r| r.name));
    v
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn drive(
    plan: &Plan,
    sb: &Sandbox,
    ctl: &mut ctl::Client,
    mnt: &str,
    cast: &Cast,
    ev: &mut Evidence,
) -> Result<(), Abort> {
    println!(
        "xver (inner): step 1 — start the {} daemon",
        cast.daemon_build
    );
    probes::before_daemon(plan, sb).map_err(Abort::Scenario)?;
    let log_name = format!("{}-daemon.log", cast.daemon_side);
    let mut daemon_cmd = Command::new(&cast.daemon_bin);
    daemon_cmd
        .args(["daemon", "serve"])
        .current_dir(&sb.project)
        .env_clear();
    for (k, v) in &plan.env {
        daemon_cmd.env(k, v);
    }
    let daemon_out = std::fs::File::create(sb.artifacts.join(&log_name))
        .map_err(|e| format!("create {log_name}: {e}"))?;
    let daemon_err = daemon_out
        .try_clone()
        .map_err(|e| format!("clone {log_name} handle: {e}"))?;
    daemon_cmd.stdout(daemon_out).stderr(daemon_err);
    let child = daemon_cmd
        .spawn()
        .map_err(|e| format!("spawn the sandbox daemon: {e}"))?;
    let mut daemon = SandboxProcess::adopt(child, &cast.daemon_bin, "sandbox daemon".to_string())
        .map_err(iso)?;
    ev.daemon_pid = daemon.pid();

    // The recorded identity must be exactly what was launched, or it is not an
    // identity worth gating a signal on.
    let id = daemon.identity.clone();
    let want_cmdline = vec![
        cast.daemon_bin.display().to_string(),
        "daemon".to_string(),
        "serve".to_string(),
    ];
    let want_env: BTreeMap<String, String> = plan.env.iter().cloned().collect();
    let problems: Vec<String> = [
        (id.exe != cast.daemon_bin).then(|| format!("exe {:?}", id.exe)),
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
        return teardown(daemon, None, None, sb, ev, body);
    }
    ev.step(format!(
        "started the {} daemon as pid {} inside the namespace",
        cast.daemon_build,
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

    let matrix = match wait_for_listeners(&mut daemon, &plan.matrices, &log_name) {
        Ok(m) => m,
        Err(e) => return teardown(daemon, None, None, sb, ev, Err(e)),
    };
    ev.daemon_endpoint = matrix.attach().display().to_string();
    if plan.matrices.len() > 1 {
        ev.isolated(format!(
            "the {} daemon bound `{}` and `{}` — {} — out of {} candidate layouts; from here on \
             every other candidate ({}) must be absent before any client connects",
            cast.daemon_build,
            matrix.owned[0].display(),
            matrix.owned[1].display(),
            if matrix.owns_per_uid(plan.uid) {
                "the post-#1121 per-uid directory"
            } else {
                "the pre-#1121 flat fallback"
            },
            plan.matrices.len(),
            matrix
                .absent
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let guard = Guard {
        plan,
        sb,
        recorded_mnt: mnt.to_string(),
        daemon: id,
        matrix,
        second: RefCell::new(None),
        ctl: RefCell::new(ctl),
        counts: RefCell::new(BTreeMap::new()),
    };
    let mut attach_slot = None;
    let body = scenario(&guard, &mut daemon, cast, ev, &mut attach_slot);
    let summary = guard.summary();
    let second = guard.second.borrow().clone();
    drop(guard);
    ev.isolated(summary);
    teardown(daemon, attach_slot, second, sb, ev, body)
}

/// Stop the attached TUI, then any second daemon the run measured, then the
/// daemon, then anything left in the namespace — each by verified identity —
/// and return `body` unchanged unless teardown itself found an isolation
/// problem.
fn teardown(
    mut daemon: SandboxProcess,
    attach_tui: Option<pty::PtyDeck>,
    second: Option<SecondDaemon>,
    sb: &Sandbox,
    ev: &mut Evidence,
    body: Result<(), Abort>,
) -> Result<(), Abort> {
    println!("xver (inner): teardown");
    if let Some(mut tui) = attach_tui {
        ev.step(tui.shutdown());
    }
    if let Some(s) = second {
        let pid = s.identity.pid;
        match proc::terminate_identity(&s.identity, DAEMON_GRACE) {
            Ok(Terminated::Graceful) => ev.step(format!(
                "sent ONE SIGTERM to the second daemon (pid {pid}) — the one the old client \
                 lazy-spawned — after re-verifying its start time, exe, exact cmdline, cwd, full \
                 environment and mount namespace; it exited within {DAEMON_GRACE:?}"
            )),
            Ok(Terminated::Killed) => ev.step(format!(
                "the second daemon (pid {pid}) outlived its SIGTERM by {DAEMON_GRACE:?}; its \
                 identity was re-verified and it got SIGKILL"
            )),
            Err(Mismatch::Gone) => ev.step(format!("second daemon pid {pid} was already gone")),
            Err(m) => ev.step(format!(
                "REFUSED to signal the second daemon (pid {pid}): {m}. Nothing was signalled; \
                 the namespace's exit reaps it."
            )),
        }
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

/// Steps 2–5 and the four tells, in either direction. The attached TUI is
/// parked in `attach_slot` as soon as it exists, so teardown can stop it by
/// identity on success and on failure alike rather than leaving it to a
/// destructor.
fn scenario(
    g: &Guard<'_>,
    daemon: &mut SandboxProcess,
    cast: &Cast,
    ev: &mut Evidence,
    attach_slot: &mut Option<pty::PtyDeck>,
) -> Result<(), Abort> {
    let plan = g.plan;
    let sb = g.sb;
    let all_roles = roles(plan);
    let every_role = |rows: &[StatusRow]| {
        all_roles
            .iter()
            .all(|r| rows.iter().any(|row| row.role.contains(r.as_str())))
    };
    let daemon_cli = format!("{} CLI: daemon status", cast.daemon_side);

    if let Some(blocked) = probes::daemon_layout_precondition(plan, &g.matrix) {
        // The probe cannot reach the arm it was written for with the daemon
        // this branch builds; say so rather than run something else.
        ev.tell(
            "probe",
            "the probe's precondition",
            Verdict::NotChecked,
            blocked,
        );
        return Ok(());
    }

    // The daemon's own build asking its own daemon is the version-correct way to
    // find out whether it is up, in every endpoint mode.
    daemon_status(g, &cast.daemon_bin, &daemon_cli, ev)?.map_err(|e| {
        Abort::Scenario(format!(
            "the sandbox daemon never answered `daemon status`: {e}"
        ))
    })?;
    ev.step("the sandbox daemon answered `daemon status`");
    let listeners_at_start = proc::listeners_on(g.matrix.attach());
    let kernel_at_start = kernel_owner_line(g.matrix.attach());

    println!(
        "xver (inner): step 2 — bring an orchestration up under it, with the {} TUI",
        cast.daemon_side.to_uppercase()
    );
    g.preconnect_logged(&format!("{} TUI", cast.daemon_side), &plan.env, ev)?;
    let setup_label = Cast::tui_label(cast.daemon_side);
    let mut setup_tui = pty::PtyDeck::spawn(pty::PtySpec {
        label: setup_label,
        bin: &cast.daemon_bin,
        args: &[],
        cwd: &sb.project,
        env: &plan.env,
        cols: 200,
        rows: 55,
        stream_log: sb.artifacts.join(format!("{setup_label}.stream.txt")),
    })?;
    if !setup_tui.wait_for_grid_string("No active sessions", UI_TIMEOUT) {
        return Err(Abort::Scenario(format!(
            "{}: never reached an empty dashboard.\n=== grid ===\n{}",
            setup_tui.label,
            setup_tui.grid()
        )));
    }
    open_orchestration(&setup_tui)?;
    let rows = wait_for_status(g, &cast.daemon_bin, &daemon_cli, UI_TIMEOUT, ev, every_role)?
        .map_err(|e| {
            Abort::Scenario(format!(
                "the {} role panes never came up under the sandbox daemon: {e}\n=== {} TUI grid \
                 ===\n{}",
                all_roles.len(),
                cast.daemon_side,
                setup_tui.grid()
            ))
        })?;
    ev.step(format!(
        "the {} TUI brought up {} role panes under the {} daemon: {}",
        cast.daemon_side,
        rows.len(),
        cast.daemon_side,
        rows.iter()
            .map(|r| format!("{} ({})", r.role, r.pane_id))
            .collect::<Vec<_>>()
            .join(", ")
    ));

    println!("xver (inner): step 3 — Ctrl+D, Ctrl+C, Detach (never Stop)");
    setup_tui.send(b"\x04"); // Ctrl+D — leave PaneInput. Without this Ctrl+C goes
    std::thread::sleep(SETTLE); // to the focused PANE and kills a role.
    setup_tui.send(b"\x03"); // Ctrl+C — the quit dialog
    if !setup_tui.wait_for_grid_string("Quit dot-agent-deck?", STEP_TIMEOUT) {
        return Err(Abort::Scenario(format!(
            "Ctrl+D then Ctrl+C never opened the quit dialog in the {} TUI.\n=== grid ===\n{}",
            cast.daemon_side,
            setup_tui.grid()
        )));
    }
    setup_tui.send(b"\r"); // Enter on the default option, which is Detach (index 0).
    if setup_tui.wait_for_exit(STEP_TIMEOUT).is_none() {
        return Err(Abort::Scenario(format!(
            "the {} TUI did not exit after choosing Detach.\n=== grid ===\n{}",
            cast.daemon_side,
            setup_tui.grid()
        )));
    }
    ev.step(setup_tui.shutdown());
    drop(setup_tui);

    let after_detach = wait_for_status(
        g,
        &cast.daemon_bin,
        &daemon_cli,
        DAEMON_TIMEOUT,
        ev,
        every_role,
    )?
    .map_err(|e| {
        Abort::Scenario(format!(
            "after the detach the daemon no longer lists every role — the Ctrl+C trap (it \
                 goes to the PANE in PaneInput mode) or a `Stop` instead of `Detach`: {e}"
        ))
    })?;
    if daemon.has_exited() {
        return Err(Abort::Scenario(format!(
            "the sandbox daemon (pid {}) died during the detach — `Detach` must leave it running",
            daemon.pid()
        )));
    }
    ev.step(format!(
        "after Detach the daemon (pid {}) is still alive and still lists {} role panes",
        daemon.pid(),
        after_detach.len()
    ));

    println!(
        "xver (inner): step 4 — attach the {} TUI and decline the mismatch prompt",
        cast.client_side.to_uppercase()
    );
    g.preconnect_logged(&format!("{} TUI", cast.client_side), &plan.env, ev)?;
    let fallback_arm = plan.mode == EndpointMode::Resolved && !plan.keep_xdg_runtime_dir;
    if fallback_arm && cast.direction == Direction::Forward {
        ev.isolated(format!(
            "fallback arm: the branch TUI starts with no `XDG_RUNTIME_DIR` and no socket override \
             in its environment, its own primary fallback `{}` is absent (no file, no listener), \
             and the only deck endpoints listening anywhere in the namespace are the {} daemon's \
             legacy flat `{}` and `{}`. The only way the branch TUI can reach a daemon is the \
             compatibility read of the literal `/tmp` — the fallback arm issue #1121 changed.",
            sandbox::per_uid_dir(plan.uid).display(),
            plan.previous,
            g.matrix.owned[0].display(),
            g.matrix.owned[1].display()
        ));
    }
    if fallback_arm && cast.direction == Direction::Reverse {
        ev.isolated(format!(
            "fallback arm, reverse: the {} TUI starts with no `XDG_RUNTIME_DIR` and no socket \
             override in its environment. The branch daemon listens at `{}` and `{}`; every \
             other candidate — {} — is absent (no file, no listener). Whatever the old TUI \
             reaches next is decided by its own resolution of the no-XDG, no-override fallback \
             and nothing else.",
            cast.client_build,
            g.matrix.owned[0].display(),
            g.matrix.owned[1].display(),
            g.matrix
                .absent
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let attach_label = Cast::tui_label(cast.client_side);
    let attach_tui = attach_slot.insert(pty::PtyDeck::spawn(pty::PtySpec {
        label: attach_label,
        bin: &cast.client_bin,
        args: &[],
        cwd: &sb.project,
        env: &plan.env,
        cols: 200,
        rows: 55,
        stream_log: sb.artifacts.join(format!("{attach_label}.stream.txt")),
    })?);
    with_attached_tui(
        g,
        daemon,
        cast,
        attach_tui,
        ev,
        listeners_at_start,
        kernel_at_start,
        fallback_arm,
    )
}

#[allow(clippy::too_many_arguments)]
fn with_attached_tui(
    g: &Guard<'_>,
    daemon: &mut SandboxProcess,
    cast: &Cast,
    tui: &mut pty::PtyDeck,
    ev: &mut Evidence,
    listeners_at_start: Option<Vec<i32>>,
    kernel_at_start: String,
    fallback_arm: bool,
) -> Result<(), Abort> {
    let plan = g.plan;
    let sb = g.sb;
    let all_roles = roles(plan);
    let saw_prompt = tui.wait_for_stream_string("Daemon version mismatch", UI_TIMEOUT);
    let prompt_excerpt = extract_prompt(&tui.stream_text());
    if !saw_prompt {
        ev.excerpt(
            format!("{} TUI stream (no mismatch prompt)", cast.client_side),
            tail(&tui.stream_text(), 60),
        );
        if cast.direction == Direction::Reverse {
            // In reverse, a client that never prints the prompt may simply
            // not have FOUND the branch daemon. That is a measurable outcome
            // with its own status, not a harness error — so measure it.
            return classify_undiscovered(g, daemon, cast, tui, ev);
        }
        return Err(Abort::Scenario(format!(
            "the {} TUI never printed the build-version mismatch prompt within {UI_TIMEOUT:?}. \
             That prompt appearing IS the proof the scenario was reached: either it did not find \
             the {} daemon at all (and has lazy-spawned its own — check the `Attach protocol \
             listening` count below), or the two builds report the same build id. old={} new={}",
            cast.client_side,
            cast.daemon_side,
            ev.old_hello.trim(),
            ev.new_hello.trim()
        )));
    }
    ev.excerpt(
        format!(
            "build-version mismatch prompt, as the {} TUI printed it",
            cast.client_side
        ),
        prompt_excerpt.clone(),
    );
    let named_roles: Vec<&str> = all_roles
        .iter()
        .map(String::as_str)
        .filter(|r| prompt_excerpt.contains(r))
        .collect();
    ev.step(format!(
        "the {} TUI printed the build-version mismatch prompt naming {} of the live roles ({})",
        cast.client_side,
        named_roles.len(),
        named_roles.join(", ")
    ));
    // Any key other than `s`/`S` declines and keeps the existing daemon (see
    // DECLINE_KEY). Accepting would SIGTERM the daemon under test and replace
    // it, which destroys the entire point of the run.
    tui.send(DECLINE_KEY);
    if !tui.wait_for_grid(UI_TIMEOUT, |g| {
        g.contains("XVER_") || g.contains(ROLE_ORCHESTRATOR)
    }) {
        return Err(Abort::Scenario(format!(
            "{}: after declining the prompt it never rendered the orchestration.\n=== grid ===\n{}",
            tui.label,
            tui.grid()
        )));
    }
    ev.step(format!(
        "declined the prompt (`{}`); the {} TUI attached to the {} daemon unchanged",
        String::from_utf8_lossy(DECLINE_KEY),
        cast.client_side,
        cast.daemon_side.to_uppercase()
    ));

    println!("xver (inner): step 5 — delegate first, hooks last");
    focus_role(tui, plan, ROLE_ORCHESTRATOR)?;
    let nonce = epoch_secs();
    let delegate_sentinel = format!("XVER-DELEGATE-{nonce}");
    // The pane's shell is about to start the client CLI, which is a client.
    g.preconnect_logged(
        &format!("pane: {} delegate", cast.pane_cli_label),
        &plan.env,
        ev,
    )?;
    type_into_pane(
        tui,
        &format!(
            "{} delegate --to {ROLE_CODER} --to {ROLE_REVIEWER} --task \"{delegate_sentinel} list the files in this directory\"",
            cast.pane_cli
        ),
    );

    // Delivery is asserted on the PAYLOAD, in the target pane, not on the CLI's
    // exit code. `coder` is `cat`, so whatever the daemon wrote into its PTY is
    // echoed straight back onto the screen.
    focus_role(tui, plan, ROLE_CODER)?;
    let pointer = format!("worker-task-{ROLE_CODER}.md");
    let delivered = tui.wait_for_grid(UI_TIMEOUT, |g| {
        g.contains(&pointer) || g.contains(&delegate_sentinel)
    });
    ev.excerpt("`coder` role pane after the delegate", tui.grid());
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
             `{ROLE_REVIEWER}` with the {} CLI (`{}`), typed into the orchestrator's own PTY \
             through the {} TUI's pane-input path to the {} daemon.\n\
             looked for `{pointer}` or `{delegate_sentinel}` on the `{ROLE_CODER}` pane: {}\n\
             {task_file_note}",
            cast.client_build,
            cast.pane_cli,
            cast.client_side.to_uppercase(),
            cast.daemon_side.to_uppercase(),
            if delivered { "found" } else { "NOT FOUND" }
        ),
    );

    // Hooks. `work-done` first, `agent-event` last: a bare `AgentEvent` makes
    // the daemon classify that pane's agent type as `Pi`, which routes prompt
    // delivery differently and makes a delivered delegate briefly look
    // undelivered.
    focus_role(tui, plan, ROLE_REVIEWER)?;
    let work_done_sentinel = format!("XVER-WORKDONE-{nonce}");
    g.preconnect_logged(
        &format!("pane: {} work-done", cast.pane_cli_label),
        &plan.env,
        ev,
    )?;
    type_into_pane(
        tui,
        &format!(
            "{} work-done --task \"{work_done_sentinel}\"",
            cast.pane_cli
        ),
    );
    focus_role(tui, plan, ROLE_ORCHESTRATOR)?;
    let feedback = format!("Worker {ROLE_REVIEWER} has completed their task");
    let work_done_arrived = tui.wait_for_grid(UI_TIMEOUT, |g| g.contains(&feedback));
    ev.excerpt(
        "orchestrator role pane after the worker's `work-done`",
        tui.grid(),
    );

    focus_role(tui, plan, ROLE_REVIEWER)?;
    g.preconnect_logged(
        &format!("pane: {} agent-event", cast.pane_cli_label),
        &plan.env,
        ev,
    )?;
    type_into_pane(
        tui,
        &format!("{} agent-event --type running", cast.pane_cli),
    );
    let client_cli = format!("{} CLI: daemon status", cast.client_side);
    let status_rows = wait_for_status(
        g,
        &cast.client_bin,
        &client_cli,
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
                    "`daemon status --json`, asked by the {} binary of the {} daemon, reports the \
                     `{ROLE_REVIEWER}` pane as `{}`",
                    cast.client_side.to_uppercase(),
                    cast.daemon_side.to_uppercase(),
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
            "work-done: issued `{} work-done --task \"{work_done_sentinel}\"` from inside the \
             `{ROLE_REVIEWER}` pane; the daemon's feedback line \"{feedback}\" {} in the \
             orchestrator's pane.\n\
             status: issued `{} agent-event --type running` from inside the `{ROLE_REVIEWER}` \
             pane, LAST as rule 12 requires. {status_note}",
            cast.pane_cli,
            if work_done_arrived {
                "appeared"
            } else {
                "did NOT appear"
            },
            cast.pane_cli
        ),
    );

    // --- the branch-specific stimulus, reverse only --------------------------
    if cast.direction == Direction::Reverse {
        probes::run(&mut probes::Ctx {
            g,
            daemon: &mut *daemon,
            cast,
            tui: &mut *tui,
            ev: &mut *ev,
            phase: probes::Phase::BeforeTells,
        })?;
    }

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
            "{} matched in {}. {}",
            match listening {
                1 => "1 line".to_string(),
                n => format!("{n} lines"),
            },
            sb.log.display(),
            match cast.direction {
                Direction::Forward =>
                    "Two would mean the branch TUI lazy-spawned its own daemon and this was a \
                     meaningless same-version test — the tell for BOTH the no-agents cause and \
                     the 30-second idle-window cause.",
                Direction::Reverse =>
                    "Two would mean a daemon other than the branch's started: the old client \
                     lazy-spawned its own (it did not find the branch daemon, or a 30-second idle \
                     window swallowed it), and every tell after that point was measured against \
                     a daemon of the old build.",
            }
        ),
    );

    let attach = g.matrix.attach();
    let listeners_at_end = proc::listeners_on(attach);
    let kernel_at_end = kernel_owner_line(attach);
    let exe_now = proc::exe_path(daemon.pid());
    let same_exe = exe_now.as_deref() == Some(cast.daemon_bin.as_path());
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
                 {} binary",
                if alive { "alive" } else { "GONE" },
                daemon.pid(),
                exe_now,
                cast.daemon_build
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
             the mismatch prompt the {} TUI printed reported the daemon's build id and its \
             own, so both sides' build ids were observed over the wire — see the excerpt.\n\
             `ss -xlp` on {}, inside the run's network namespace: {listener_note}\n\
             at the start, {kernel_at_start}\n\
             at the end, {kernel_at_end}",
            daemon.pid(),
            if alive { "alive" } else { "GONE" },
            daemon.pid(),
            exe_now,
            cast.daemon_build,
            cast.daemon_bin.display(),
            cast.client_side,
            attach.display(),
        ),
    );

    if plan.mode == EndpointMode::Resolved && cast.direction == Direction::Forward {
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
    if fallback_arm && cast.direction == Direction::Forward {
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

    // --- a stimulus that ends the daemon, after tells 1 and 2 ------------------
    if cast.direction == Direction::Reverse {
        probes::run(&mut probes::Ctx {
            g,
            daemon: &mut *daemon,
            cast,
            tui: &mut *tui,
            ev: &mut *ev,
            phase: probes::Phase::AfterTells,
        })?;
    }

    ev.excerpt(
        "sandbox deck.log (tail)",
        tail(&std::fs::read_to_string(&sb.log).unwrap_or_default(), 80),
    );
    Ok(())
}

/// The reverse run's attached client never printed the mismatch prompt:
/// establish, by measurement, whether that is because it did not FIND the
/// branch daemon — and whether its fallback damaged anything.
///
/// Positive evidence is required for the classification. The old client must
/// have left a listener of its own in the namespace (the daemon it lazy-spawned),
/// whose holder is identified by its full identity as the old build running
/// `daemon serve` with this run's marker; without that, a missing prompt is an
/// unexplained scenario failure, not a discovery result. Then three collateral
/// checks, each a tell, decide between "the old client cannot discover the
/// branch daemon — and nothing else happened" and a FAIL:
///
/// 1. the branch daemon is untouched — same identity, still holding every
///    endpoint the matrix says it owns;
/// 2. it still runs every role of the orchestration;
/// 3. the old client's own daemon runs NONE of those roles. The old TUI restores
///    the saved session against an empty daemon, and a restored orchestration
///    re-spawns every role under a fresh orchestration id — which would be two
///    daemons running the same orchestration, not merely two daemons.
fn classify_undiscovered(
    g: &Guard<'_>,
    daemon: &mut SandboxProcess,
    cast: &Cast,
    tui: &mut pty::PtyDeck,
    ev: &mut Evidence,
) -> Result<(), Abort> {
    let plan = g.plan;
    let sb = g.sb;
    let all_roles = roles(plan);
    println!(
        "xver (inner): the {} TUI printed no mismatch prompt — measuring what it reached instead",
        cast.client_side
    );
    ev.step(format!(
        "the {} TUI printed NO build-version mismatch prompt within {UI_TIMEOUT:?}: it did not \
         reach the {} daemon, which holds {} live role(s). Measuring what it reached instead.",
        cast.client_side,
        cast.daemon_side,
        all_roles.len()
    ));

    // Who listens now, besides the daemon under test?
    let listeners = proc::unix_listeners().map_err(iso)?;
    let held = proc::socket_inodes(daemon.pid()).map_err(iso)?;
    let strays: Vec<proc::UnixListener> = listeners
        .iter()
        .filter(|l| !held.contains(&l.inode))
        .cloned()
        .collect();
    if strays.is_empty() {
        return Err(Abort::Scenario(format!(
            "the {} TUI printed no mismatch prompt and started no daemon of its own: nothing in \
             the namespace listens except the {} daemon, so the missing prompt is unexplained.\n\
             === {} TUI grid ===\n{}",
            cast.client_side,
            cast.daemon_side,
            cast.client_side,
            tui.grid()
        )));
    }
    let mut holders: Vec<i32> = Vec::new();
    for l in &strays {
        for pid in proc::owners_of(l.inode).map_err(iso)? {
            if !holders.contains(&pid) {
                holders.push(pid);
            }
        }
    }
    // Forked lifetime-cap reapers inherit the listening descriptors, so the
    // holder set is a daemon and its forks: the daemon is the one whose parent
    // is not itself a holder.
    let roots: Vec<i32> = holders
        .iter()
        .copied()
        .filter(|p| proc::ppid(*p).is_none_or(|pp| !holders.contains(&pp)))
        .collect();
    let [root] = roots[..] else {
        return Err(iso(format!(
            "listeners the run did not start ({strays:?}) are held by {holders:?}, which is not \
             one daemon and its forks (roots {roots:?})"
        )));
    };
    let second_id = Identity::capture(root)
        .map_err(|e| iso(format!("the second listener's holder pid {root}: {e}")))?;
    let want_cmdline = vec![
        cast.client_bin.display().to_string(),
        "daemon".to_string(),
        "serve".to_string(),
    ];
    let marker_ok =
        second_id.environ.get("DAD_XVER_SANDBOX") == Some(&sb.root.display().to_string());
    if second_id.exe != cast.client_bin || second_id.cmdline != want_cmdline || !marker_ok {
        return Err(iso(format!(
            "a listener the run did not start is held by an unidentified process: {}",
            second_id.summary()
        )));
    }
    let second_paths: Vec<PathBuf> = strays.iter().map(|l| PathBuf::from(&l.path)).collect();
    *g.second.borrow_mut() = Some(SecondDaemon {
        identity: second_id.clone(),
        paths: second_paths.clone(),
    });
    ev.isolated(format!(
        "second daemon recorded by its full identity — the {} build running `daemon serve` with \
         this run's marker, lazy-spawned by the {} TUI — listening at {:?}: {}",
        cast.client_build,
        cast.client_side,
        second_paths,
        second_id.summary()
    ));

    // What the user saw. Give the old TUI time to settle — a session restore,
    // if it happens, spawns panes after the first frame.
    std::thread::sleep(Duration::from_secs(5));
    ev.excerpt(
        format!(
            "what the {} TUI showed instead of the prompt",
            cast.client_side
        ),
        tui.grid(),
    );

    // Collateral 1: the daemon under test is untouched.
    let untouched = match daemon.identity.verify() {
        Ok(()) if !daemon.has_exited() => {
            let now = proc::unix_listeners().map_err(iso)?;
            let held_now = proc::socket_inodes(daemon.pid()).map_err(iso)?;
            g.matrix.owned.iter().all(|p| {
                let inodes = isolation::listening_inodes(&now, p);
                !inodes.is_empty() && inodes.iter().all(|i| held_now.contains(i))
            })
        }
        _ => false,
    };
    ev.tell(
        "collateral-1",
        format!("the {} daemon is untouched by the old client's fallback", cast.daemon_side),
        if untouched { Verdict::Pass } else { Verdict::Fail },
        format!(
            "pid {} {}; its recorded identity {}; it {} every endpoint the matrix says it owns ({})",
            daemon.pid(),
            if daemon.has_exited() { "has EXITED" } else { "is alive" },
            if daemon.identity.verify().is_ok() { "still verifies" } else { "NO LONGER verifies" },
            if untouched { "still holds" } else { "does NOT hold" },
            g.matrix
                .owned
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );

    // Collateral 2: its roles survived.
    let every_role = |rows: &[StatusRow]| {
        all_roles
            .iter()
            .all(|r| rows.iter().any(|row| row.role.contains(r.as_str())))
    };
    let daemon_cli = format!(
        "{} CLI: daemon status (after the old client's fallback)",
        cast.daemon_side
    );
    let own = if untouched {
        wait_for_status(
            g,
            &cast.daemon_bin,
            &daemon_cli,
            STEP_TIMEOUT,
            ev,
            every_role,
        )?
    } else {
        Err("the daemon under test is not intact, so it was not asked".to_string())
    };
    ev.tell(
        "collateral-2",
        format!("the {} daemon still runs every role", cast.daemon_side),
        if own.is_ok() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        match &own {
            Ok(rows) => format!(
                "`daemon status --json`, asked by the {} binary (which resolves the {} daemon's \
                 own endpoint), lists {}",
                cast.daemon_side,
                cast.daemon_side,
                rows.iter()
                    .map(|r| format!("{} ({}, {})", r.role, r.pane_id, r.status))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(e) => e.clone(),
        },
    );

    // Collateral 3: the old client's own daemon runs none of them. Poll for a
    // while rather than ask once: a restored orchestration spawns its roles
    // after the old TUI's first frame.
    let client_cli = format!(
        "{} CLI: daemon status (the {} TUI's own daemon)",
        cast.client_side, cast.client_side
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen: Vec<StatusRow> = Vec::new();
    // The LAST poll's error, if the last poll failed.
    let mut last_err: Option<String>;
    loop {
        match daemon_status(g, &cast.client_bin, &client_cli, ev)? {
            Ok(rows) => {
                for r in rows {
                    if !seen
                        .iter()
                        .any(|s| s.agent_id == r.agent_id && s.pane_id == r.pane_id)
                    {
                        seen.push(r);
                    }
                }
                last_err = None;
            }
            Err(e) => last_err = Some(e),
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    let duplicated: Vec<&StatusRow> = seen
        .iter()
        .filter(|r| all_roles.iter().any(|role| r.role.contains(role.as_str())))
        .collect();
    let (verdict3, detail3) = match (&last_err, duplicated.is_empty()) {
        (Some(e), _) if seen.is_empty() => (
            Verdict::NotChecked,
            format!(
                "the {} CLI could not ask its own daemon what it runs: {e}",
                cast.client_side
            ),
        ),
        (_, true) => (
            Verdict::Pass,
            format!(
                "for 15 s, `daemon status --json` asked by the {} binary — which resolves the {} \
                 daemon's flat address — listed {}",
                cast.client_side,
                cast.client_side,
                if seen.is_empty() {
                    "no agents at all".to_string()
                } else {
                    seen.iter()
                        .map(|r| format!("{} ({}, role `{}`)", r.agent_id, r.pane_id, r.role))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        ),
        (_, false) => (
            Verdict::Fail,
            format!(
                "the {} TUI's OWN daemon now runs {} of the {} daemon's roles: {}. The {} TUI \
                 restored the saved session against its fresh, empty daemon and re-spawned the \
                 orchestration there — so two daemons now run the same orchestration's roles, \
                 not merely two daemons on one host",
                cast.client_build,
                duplicated.len(),
                cast.daemon_side,
                duplicated
                    .iter()
                    .map(|r| format!(
                        "{} ({}, role `{}`, {})",
                        r.agent_id, r.pane_id, r.role, r.status
                    ))
                    .collect::<Vec<_>>()
                    .join(", "),
                cast.client_side
            ),
        ),
    };
    ev.tell(
        "collateral-3",
        format!(
            "the {} TUI's own daemon runs none of the {} daemon's roles",
            cast.client_side, cast.daemon_side
        ),
        verdict3,
        detail3,
    );
    ev.excerpt(
        format!("the {} TUI after 15 s", cast.client_side),
        tui.grid(),
    );
    // The process tree under each daemon, read from /proc rather than from
    // either daemon's own report: separate role processes under the second
    // daemon are what "two daemons run the same orchestration" means, and a
    // reader should not have to take a status reply's word for it.
    let tree = |pid: i32| {
        let d = proc::descendants(pid);
        let lines: Vec<String> = d
            .iter()
            .map(|(p, c)| format!("  pid {p}: {}", c.join(" ")))
            .collect();
        (d.len(), lines.join("\n"))
    };
    let (n_daemon, t_daemon) = tree(daemon.pid());
    let (n_second, t_second) = tree(second_id.pid);
    ev.excerpt(
        "process tree under each daemon, from /proc (a census; nothing here is signalled by it)",
        format!(
            "the {} daemon, pid {} — {n_daemon} descendant(s):\n{t_daemon}\n\nthe second \
             daemon ({} build), pid {} — {n_second} descendant(s):\n{t_second}",
            cast.daemon_build,
            daemon.pid(),
            cast.client_build,
            second_id.pid
        ),
    );

    let attach_lines: Vec<String> = std::fs::read_to_string(&sb.log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("Attach protocol listening"))
        .map(str::to_string)
        .collect();
    ev.discovery = Some(format!(
        "The {client} TUI (`{client_bin}`), started with the same environment as every other \
         client of the run, printed no build-version mismatch prompt and never reached the \
         {daemon} daemon (pid {dpid}) listening at {owned}.\n\
         Instead it lazy-spawned a daemon of its own build — pid {spid}, `{sexe}` \
         `daemon serve`, identified by its full identity — which bound {spaths}. The old TUI's \
         screen, recorded in the excerpts below, carries no warning about any of it.\n\
         `Attach protocol listening` lines in the run's log: {n} — {lines}\n\
         The collateral tells below decide whether that fallback left the {daemon} daemon and \
         its orchestration alone.",
        client = cast.client_build,
        client_bin = cast.client_bin.display(),
        daemon = cast.daemon_build,
        dpid = daemon.pid(),
        owned = g
            .matrix
            .owned
            .iter()
            .map(|p| format!("`{}`", p.display()))
            .collect::<Vec<_>>()
            .join(" and "),
        spid = second_id.pid,
        sexe = second_id.exe.display(),
        spaths = second_paths
            .iter()
            .map(|p| format!("`{}`", p.display()))
            .collect::<Vec<_>>()
            .join(" and "),
        n = attach_lines.len(),
        lines = attach_lines
            .iter()
            .map(|l| format!("`{}`", l.trim()))
            .collect::<Vec<_>>()
            .join("; "),
    ));
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
/// landed. The digit is the role's position in the fixture, which is the card
/// order; none of those keys is written to any pane's PTY — they are the deck's
/// own shortcuts — so focusing a pane is not "typing into" it.
///
/// Cycles tabs first when the deck is not on the orchestration tab: after a
/// reattach the deck lands wherever the previous session left it, and a digit on
/// the Dashboard tab means something else.
pub(crate) fn focus_role(deck: &pty::PtyDeck, plan: &Plan, role: &str) -> Result<(), String> {
    let index = roles(plan)
        .iter()
        .position(|r| r == role)
        .ok_or_else(|| format!("no card index known for role {role}"))?;
    let digit = u8::try_from(index + 1)
        .ok()
        .filter(|d| *d <= 9)
        .map(|d| b'0' + d)
        .ok_or_else(|| format!("role {role} is card {} — past the digit keys", index + 1))?;
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
pub(crate) fn type_into_pane(deck: &pty::PtyDeck, text: &str) {
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

#[cfg(test)]
mod reverse_tests {
    use super::*;

    fn plan(direction: Direction, probe: crate::probe::Probe) -> Plan {
        serde_json::from_value(serde_json::json!({
            "root": "/srv/runs/r1", "uid": 1000, "user": "op",
            "mode": "SandboxSockets", "keep_xdg_runtime_dir": true, "experimental": false,
            "max_lifetime_secs": 1, "previous": "v0.41.0", "direction": direction,
            "probe": probe, "fixture": "", "extra_env": [], "env": [],
            "matrices": [], "masks": [], "masked_home": null, "outer_mnt_ns": "mnt:[1]"
        }))
        .expect("plan")
    }

    #[test]
    fn the_cast_swaps_every_part_in_reverse() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let fwd = Cast::for_plan(&plan(Direction::Forward, crate::probe::Probe::Generic), &sb);
        assert_eq!(fwd.daemon_bin, sb.old_bin());
        assert_eq!(fwd.client_bin, sb.new_bin(Direction::Forward));
        assert_eq!(
            fwd.pane_cli, "dot-agent-deck",
            "forward types the bare name, as it always did"
        );
        let rev = Cast::for_plan(&plan(Direction::Reverse, crate::probe::Probe::Generic), &sb);
        assert_eq!(rev.daemon_bin, sb.new_bin(Direction::Reverse));
        assert_eq!(rev.client_bin, sb.old_bin());
        assert_eq!(rev.pane_cli, sb.old_bin().display().to_string());
        assert_eq!((rev.daemon_side, rev.client_side), ("branch", "old"));
        assert_eq!(Cast::tui_label("old"), "old-tui");
        assert_eq!(Cast::tui_label("branch"), "new-tui");
    }

    #[test]
    fn the_decline_key_is_never_the_one_affirmative_key() {
        assert!(!DECLINE_KEY.eq_ignore_ascii_case(b"s"));
        assert_eq!(DECLINE_KEY.len(), 1);
    }

    #[test]
    fn a_probe_role_gets_the_next_card_after_the_fixtures_three() {
        let p = plan(Direction::Reverse, crate::probe::Probe::CrossPaneSessionKey);
        assert_eq!(
            roles(&p),
            vec!["orchestrator", "coder", "reviewer", "alpha", "beta"]
        );
        assert_eq!(
            roles(&plan(Direction::Forward, crate::probe::Probe::Generic)).len(),
            3
        );
    }
}
