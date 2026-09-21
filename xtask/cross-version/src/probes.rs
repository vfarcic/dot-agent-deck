//! The reverse probes' stimuli and assertions — what each branch-specific probe
//! (see `probe.rs`) does to the branch daemon, and what it then requires.
//!
//! Every stimulus is issued by the previous release's CLI from INSIDE a pane the
//! branch daemon spawned, so it carries that pane's genuine identity and
//! capability rather than anything the harness copied — the probe spec's rule,
//! and the reason no stimulus here connects to a socket itself. Each one is
//! preceded by the same pre-connect assertion as every other client of the run,
//! so a probe cannot reach a daemon other than the one under test.
//!
//! A probe's result is a tell with id `probe-<name>`: PASS or FAIL on what the
//! branch daemon then did, or NOT CHECKED — which makes the run INCOMPLETE —
//! when the probe's own anti-vacuity condition did not hold and a pass would
//! have measured nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::inner::{
    self, Abort, Cast, DAEMON_GRACE, DAEMON_TIMEOUT, Guard, ROLE_ORCHESTRATOR, ROLE_REVIEWER,
    STEP_TIMEOUT, StatusRow, UI_TIMEOUT, focus_role, type_into_pane,
};
use crate::isolation::Plan;
use crate::probe::{self, Probe, ROLE_ALPHA, ROLE_BETA, ROLE_LATEBOOT};
use crate::proc::SandboxProcess;
use crate::pty;
use crate::report::{Evidence, Verdict};
use crate::sandbox::{self, EndpointMatrix, Sandbox};
use crate::stub;

/// When in the run a stimulus goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// After tells 3 and 4, before tells 1 and 2 — so the one-listener and
    /// same-daemon tells cover the probe's own clients too.
    BeforeTells,
    /// After every tell. Only for a stimulus that ends the daemon.
    AfterTells,
}

/// What a stimulus runs against.
pub struct Ctx<'a, 'g> {
    pub g: &'a Guard<'g>,
    pub daemon: &'a mut SandboxProcess,
    pub cast: &'a Cast,
    pub tui: &'a mut pty::PtyDeck,
    pub ev: &'a mut Evidence,
    pub phase: Phase,
}

/// Run the plan's probe, if it has a stimulus in this phase.
pub fn run(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let probe = ctx.g.plan.probe;
    match (probe, ctx.phase) {
        (Probe::TeardownInventory, Phase::AfterTells) => teardown_inventory(ctx),
        (Probe::LateSessionStart, Phase::BeforeTells) => late_session_start(ctx),
        (Probe::LogEscaping, Phase::BeforeTells) => log_escaping(ctx),
        (Probe::PasteEnvelope, Phase::BeforeTells) => paste_envelope(ctx),
        (Probe::CrossPaneSessionKey, Phase::BeforeTells) => cross_pane_session_key(ctx),
        (Probe::SignalAck, Phase::BeforeTells) => signal_ack(ctx),
        (Probe::GitEnv, Phase::BeforeTells) => git_env(ctx),
        // DiscoveryFallback's measurement is the attach itself (see
        // `inner::classify_undiscovered`); Generic has none.
        _ => Ok(()),
    }
}

/// A probe whose arm the daemon this branch builds cannot reach, named — or
/// `None`. Only `DiscoveryFallback` has one: it measures an old client against
/// a daemon bound in the post-#1121 per-uid directory, and a branch daemon that
/// bound the flat pair is not that daemon.
pub fn daemon_layout_precondition(plan: &Plan, matrix: &EndpointMatrix) -> Option<String> {
    (plan.probe == Probe::DiscoveryFallback && !matrix.owns_per_uid(plan.uid)).then(|| {
        format!(
            "the discovery-fallback probe needs a branch daemon bound in the post-#1121 per-uid \
             directory, and this branch's daemon bound `{}` and `{}` — it does not carry #1121's \
             layout, so the probe would measure nothing it was written for",
            matrix.owned[0].display(),
            matrix.owned[1].display()
        )
    })
}

// ---------------------------------------------------------------------------
// Outer-half preparation
// ---------------------------------------------------------------------------

/// What a probe needs on disk before the namespace starts, created by the
/// outer half with a clean environment. Returns notes for the evidence file.
///
/// * A dispatch probe gets `$S/config/config.toml` holding ONE key,
///   `default_command` — the global config `dispatch --single` reads its unit's
///   command from (`DOT_AGENT_DECK_CONFIG` already points there, and without
///   the file the unit's command falls back to a bare `claude`).
/// * `GitEnv` gets its decoy: a second standalone repository under `$S` with a
///   sentinel commit of its own, created BEFORE any hostile variable exists.
pub fn prepare_outer(sb: &Sandbox, probe: Probe) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    if let Some(cmd) = probe.dispatch_command(sb) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = sb.config_file();
        let body = format!(
            "# Written by `cargo xver` for the {} probe: the command `dispatch --single` runs.\n\
             default_command = {}\n",
            probe.name(),
            probe::toml_string(&cmd)
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        notes.push(format!(
            "dispatch unit command, in `{}` (the only key there): `{cmd}`",
            path.display()
        ));
    }
    if probe == Probe::GitEnv {
        let decoy = probe::decoy_repo(sb);
        std::fs::create_dir(&decoy).map_err(|e| format!("create {}: {e}", decoy.display()))?;
        std::fs::write(
            decoy.join("DECOY-SENTINEL"),
            "xver-1181 decoy working tree\n",
        )
        .map_err(|e| format!("write the decoy sentinel: {e}"))?;
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "DECOY-SENTINEL"],
            vec![
                "-c",
                "user.name=xver",
                "-c",
                "user.email=xver@invalid.example",
                "commit",
                "--quiet",
                "-m",
                "xver-1181 decoy sentinel commit",
            ],
        ] {
            let st = sandbox::sandbox_git(sb, &decoy)
                .args(&args)
                .status()
                .map_err(|e| format!("git {args:?} in the decoy: {e}"))?;
            if !st.success() {
                return Err(format!("git {args:?} in the decoy failed: {st}"));
            }
        }
        notes.push(format!(
            "#1190 decoy: a standalone repository at `{}` with its own sentinel commit, created \
             with an empty environment before any hostile variable existed",
            decoy.display()
        ));
    }
    Ok(notes)
}

/// Anything a probe records inside the namespace BEFORE the daemon starts.
///
/// `GitEnv` snapshots the decoy here, so the run can tell a change made by the
/// dispatch from one made earlier — by the daemon's startup or by the
/// orchestration opening — which is a different call site and is reported as an
/// observation rather than attributed to the dispatch.
pub fn before_daemon(plan: &Plan, sb: &Sandbox) -> Result<(), String> {
    if plan.probe == Probe::GitEnv {
        let snap = snapshot(sb, &probe::decoy_repo(sb));
        let json = serde_json::to_string_pretty(&snap).map_err(|e| format!("{e}"))?;
        std::fs::write(decoy_baseline(sb), json)
            .map_err(|e| format!("write the decoy baseline: {e}"))?;
    }
    Ok(())
}

fn decoy_baseline(sb: &Sandbox) -> PathBuf {
    sb.artifacts.join("decoy-before-daemon.json")
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Type a hook payload into the focused pane as the client CLI would receive it
/// from a producer: `printf '%s' '<json>' | <cli> hook --agent claude-code`.
/// `json` must not contain a single quote.
fn type_hook(ctx: &mut Ctx<'_, '_>, json: &str) -> Result<(), Abort> {
    debug_assert!(!json.contains('\''));
    ctx.g.preconnect_logged(
        &format!("pane: {} hook", ctx.cast.pane_cli_label),
        &ctx.g.plan.env,
        ctx.ev,
    )?;
    type_into_pane(
        ctx.tui,
        &format!(
            "printf '%s' '{json}' | {} hook --agent claude-code",
            ctx.cast.pane_cli
        ),
    );
    Ok(())
}

fn log_text(sb: &Sandbox) -> String {
    std::fs::read_to_string(&sb.log).unwrap_or_default()
}

/// Poll `pred` over the sandbox log until it holds or `timeout` passes.
fn wait_for_log(sb: &Sandbox, timeout: Duration, pred: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred(&log_text(sb)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn client_status_label(cast: &Cast) -> String {
    format!("{} CLI: daemon status", cast.client_side)
}

fn render_rows(rows: &[StatusRow]) -> String {
    rows.iter()
        .map(|r| {
            format!(
                "{{agent {} · pane {} · role `{}` · status `{}` · tool `{}` · cwd `{}`}}",
                r.agent_id, r.pane_id, r.role, r.status, r.tool, r.cwd
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

/// Type an old-CLI `dispatch` from the orchestrator pane and wait for the unit
/// to appear in the daemon's agent list at its worktree. Returns the unit's
/// worktree and its status row, if it appeared.
fn dispatch_unit(
    ctx: &mut Ctx<'_, '_>,
    unit: &str,
    task_arg: &str,
) -> Result<(PathBuf, Result<StatusRow, String>), Abort> {
    let plan = ctx.g.plan;
    let sb = ctx.g.sb;
    focus_role(ctx.tui, plan, ROLE_ORCHESTRATOR)?;
    ctx.g.preconnect_logged(
        &format!("pane: {} dispatch", ctx.cast.pane_cli_label),
        &plan.env,
        ctx.ev,
    )?;
    type_into_pane(
        ctx.tui,
        &format!(
            "{} dispatch {unit} --single --task {task_arg}",
            ctx.cast.pane_cli
        ),
    );
    // `<project parent>/<project>-dispatch-<slug>` (`dispatch.rs`).
    let worktree = sb.root.join(format!("project-dispatch-{unit}"));
    let want = worktree.display().to_string();
    let rows = inner::wait_for_status(
        ctx.g,
        &ctx.cast.client_bin,
        &client_status_label(ctx.cast),
        DAEMON_TIMEOUT,
        ctx.ev,
        |rows| rows.iter().any(|r| r.cwd == want),
    )?;
    Ok((
        worktree,
        rows.map(|rows| {
            rows.into_iter()
                .find(|r| r.cwd == want)
                .expect("the predicate found it")
        }),
    ))
}

/// `git worktree list --porcelain` of the sandbox project, with a clean
/// environment.
fn worktree_list(sb: &Sandbox, repo: &Path) -> String {
    sandbox::sandbox_git(sb, repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_else(|e| format!("<git worktree list failed: {e}>"))
}

// ---------------------------------------------------------------------------
// #1161 / issue #1109 — teardown inventory
// ---------------------------------------------------------------------------

/// An old-TUI `Stop` — `Ctrl+D`, `Ctrl+C`, `Down`, `Enter`, `y` — sends the
/// header-only `KIND_SHUTDOWN` frame, and the branch daemon must log ONE
/// `shutdown-frame` inventory naming what it destroys before it exits.
///
/// `Stop` is the stimulus here and nowhere else, and only after the pre-connect
/// assertion has re-proven — immediately before the keys — that the TUI's
/// daemon is the recorded branch daemon holding the recorded endpoints, inside
/// the namespace. It runs after tells 1 and 2 because it ends the daemon.
fn teardown_inventory(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let plan = ctx.g.plan;
    let sb = ctx.g.sb;
    println!("xver (inner): probe teardown-inventory — old-TUI Stop against the branch daemon");
    let rows = inner::daemon_status(
        ctx.g,
        &ctx.cast.client_bin,
        &client_status_label(ctx.cast),
        ctx.ev,
    )?
    .map_err(|e| Abort::Scenario(format!("capture the inventory before Stop: {e}")))?;
    let roles: Vec<&StatusRow> = rows.iter().filter(|r| !r.role.is_empty()).collect();
    ctx.ev.step(format!(
        "before Stop: the branch daemon (pid {}) runs {} agent(s), {} of them in an orchestration \
         role: {}",
        ctx.daemon.pid(),
        rows.len(),
        roles.len(),
        render_rows(&rows)
    ));
    // The destructive frame goes only to a daemon proven, right now, to be the
    // recorded one at the recorded endpoints.
    ctx.g
        .preconnect_logged("old TUI: Stop stimulus", &plan.env, ctx.ev)?;
    ctx.tui.send(b"\x04");
    std::thread::sleep(inner::SETTLE);
    ctx.tui.send(b"\x03");
    if !ctx
        .tui
        .wait_for_grid_string("Quit dot-agent-deck?", UI_TIMEOUT)
    {
        return Err(Abort::Scenario(format!(
            "Ctrl+D then Ctrl+C never opened the quit dialog in the old TUI.\n=== grid ===\n{}",
            ctx.tui.grid()
        )));
    }
    ctx.tui.send(b"\x1b[B"); // Down -> Stop (index 1)
    // Enter takes whichever option is selected, so it goes only once the
    // dialog marks Stop selected (`> Stop`, `render_quit_confirm`) rather than
    // after a fixed pause that assumes the Down has taken effect.
    if !ctx.tui.wait_for_grid_string("> Stop", STEP_TIMEOUT) {
        return Err(Abort::Scenario(format!(
            "Down never moved the old TUI's quit dialog to Stop.\n=== grid ===\n{}",
            ctx.tui.grid()
        )));
    }
    ctx.tui.send(b"\r");
    // Only the unclipped head of the line: v0.41.0 draws this dialog 68 columns
    // wide, which cuts "…and the daemon will shut down" short on screen.
    if !ctx.tui.wait_for_grid(UI_TIMEOUT, |g| {
        g.contains("will be terminated and the daemon") && g.contains("Continue?")
    }) {
        return Err(Abort::Scenario(format!(
            "choosing Stop never opened the confirmation dialog in the old TUI.\n=== grid ===\n{}",
            ctx.tui.grid()
        )));
    }
    ctx.ev
        .excerpt("the old TUI's Stop confirmation", ctx.tui.grid());
    ctx.tui.send(b"y");
    let deadline = Instant::now() + DAEMON_GRACE;
    let mut exited = false;
    while Instant::now() < deadline {
        if ctx.daemon.has_exited() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let tui_exit = ctx.tui.wait_for_exit(STEP_TIMEOUT);
    ctx.ev.step(format!(
        "pressed `y`: the branch daemon (pid {}) {} within {DAEMON_GRACE:?}; the old TUI {}",
        ctx.daemon.pid(),
        if exited { "EXITED" } else { "did NOT exit" },
        match tui_exit {
            Some(ok) => format!("exited (success: {ok})"),
            None => "was still running".to_string(),
        }
    ));

    let log = log_text(sb);
    let records: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("terminating ") && l.contains("managed agent(s)"))
        .collect();
    let frame: Vec<&&str> = records
        .iter()
        .filter(|l| l.contains("shutdown-frame"))
        .collect();
    let signal = records.iter().filter(|l| l.contains("\"signal\"")).count();
    let mut problems = Vec::new();
    if !exited {
        problems.push("the daemon did not exit after Stop".to_string());
    }
    if frame.len() != 1 {
        problems.push(format!(
            "{} `shutdown-frame` inventory record(s), expected exactly 1",
            frame.len()
        ));
    }
    if signal != 0 {
        problems.push(format!(
            "{signal} `signal`-path inventory record(s) — the daemon was torn down by a signal, \
             not by the frame"
        ));
    }
    if let Some(line) = frame.first() {
        let agents = format!("terminating {} managed agent(s)", rows.len());
        let role_regs = format!(
            "destroying {} orchestration role registration(s)",
            roles.len()
        );
        for want in [&agents, &role_regs] {
            if !line.contains(want.as_str()) {
                problems.push(format!("the record does not say `{want}`"));
            }
        }
        for r in &roles {
            let role_name = r.role.trim_end_matches(" (orchestrator)").to_string();
            let entry = format!("{} {role_name}", r.pane_id);
            if !line.contains(&entry) {
                problems.push(format!("the record does not name role `{entry}`"));
            }
            if !r.agent_id.is_empty() && !line.contains(&r.agent_id) {
                problems.push(format!("the record does not name agent `{}`", r.agent_id));
            }
        }
    }
    ctx.ev.tell(
        "probe-teardown-inventory",
        "an old-TUI Stop makes the branch daemon disclose what it destroys, before it exits",
        verdict(problems.is_empty()),
        format!(
            "the old TUI's Stop sent `KIND_SHUTDOWN` to the branch daemon pid {pid}. A teardown \
             record is written by a daemon's own teardown path, and exactly one daemon ran in \
             this namespace (tell 1: one `Attach protocol listening` line in `{log}`); the record \
             below was in that file when pid {pid}'s exit was observed, so it was written before \
             that exit.\n\
             expected: `terminating {n} managed agent(s)`, `destroying {m} orchestration role \
             registration(s)`, and each captured pane/role/agent named.\n\
             `shutdown-frame` records: {f}; `signal` records: {signal}.\n\
             {problems}\n\
             record: {record}",
            pid = ctx.daemon.pid(),
            log = sb.log.display(),
            n = rows.len(),
            m = roles.len(),
            f = frame.len(),
            problems = if problems.is_empty() {
                "every expectation held".to_string()
            } else {
                format!("PROBLEMS: {}", problems.join("; "))
            },
            record = frame
                .first()
                .map(|l| format!("`{}`", l.trim()))
                .unwrap_or_else(|| "<none>".to_string()),
        ),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// #1168 / issue #1031 — a late SessionStart submits a parked pointer
// ---------------------------------------------------------------------------

/// Delegate to `lateboot` (a `clear = true`, Claude-typed stub) with the old
/// CLI. The branch daemon respawns it, waits the fixed 30 s for a
/// `SessionStart` the stub never sends, writes the pointer anyway and submits
/// it with a CR the stub SWALLOWS — issue #1031's parked pointer. After a quiet
/// control, the stub (triggered by a file) runs the OLD hook CLI with a late
/// `SessionStart`, from inside its own process so it carries the respawned
/// agent's identity. The branch daemon's recovery must then submit the pointer
/// exactly once and unmodified.
fn late_session_start(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let plan = ctx.g.plan;
    let sb = ctx.g.sb;
    let pointer = format!("Read .dot-agent-deck/worker-task-{ROLE_LATEBOOT}.md for your task.");
    println!("xver (inner): probe late-session-start — delegate to the lateboot stub (≥30 s)");
    focus_role(ctx.tui, plan, ROLE_ORCHESTRATOR)?;
    ctx.g.preconnect_logged(
        &format!("pane: {} delegate", ctx.cast.pane_cli_label),
        &plan.env,
        ctx.ev,
    )?;
    type_into_pane(
        ctx.tui,
        &format!(
            "{} delegate --to {ROLE_LATEBOOT} --task 'XVER-1031'",
            ctx.cast.pane_cli
        ),
    );
    // Watch the worker pane. Focusing it writes nothing to its PTY — the keys
    // are the deck's own — so the recovery's "operator typed" gate stays shut.
    focus_role(ctx.tui, plan, ROLE_LATEBOOT)?;
    let stub_log = stub::log_path(sb, stub::Mode::LateBoot);
    let swallowed = format!("{}{pointer}", stub::SWALLOWED);
    let submitted_prefix = stub::SUBMITTED;
    let waited = Instant::now();
    let parked = wait_file(&stub_log, Duration::from_secs(90), |s| {
        s.contains(&swallowed)
    });
    ctx.ev.step(format!(
        "after {:?}: the lateboot stub {} the complete pointer with its first CR",
        waited.elapsed(),
        if parked {
            "swallowed"
        } else {
            "NEVER swallowed"
        }
    ));
    if !parked {
        ctx.ev.excerpt("lateboot stub log", read(&stub_log));
        ctx.ev.excerpt("lateboot pane", ctx.tui.grid());
        ctx.ev.tell(
            "probe-late-session-start",
            "a late SessionStart from the old hook CLI submits the parked pointer exactly once",
            Verdict::NotChecked,
            format!(
                "the anti-vacuity precondition did not hold: the stub never recorded \
                 `{swallowed}` within 90 s, so there was no parked pointer to recover"
            ),
        );
        return Ok(());
    }
    // The pid that parked it: the respawned incarnation.
    let incarnation = read(&stub_log)
        .lines()
        .find(|l| l.contains(&swallowed))
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .unwrap_or_default();
    // Quiet control: nothing submits it on its own.
    std::thread::sleep(Duration::from_secs(2));
    let early = count_lines(&read(&stub_log), &incarnation, submitted_prefix);
    if early != 0 {
        ctx.ev.excerpt("lateboot stub log", read(&stub_log));
        ctx.ev.tell(
            "probe-late-session-start",
            "a late SessionStart from the old hook CLI submits the parked pointer exactly once",
            Verdict::NotChecked,
            format!(
                "the quiet control failed: {early} submission(s) BEFORE any late SessionStart, so \
                 the pointer was not parked and the recovery cannot be attributed"
            ),
        );
        return Ok(());
    }
    ctx.ev
        .step("quiet control: 2 s with the pointer parked and no submission".to_string());
    // The stub's own process runs the old hook CLI — a client.
    ctx.g.preconnect_logged(
        &format!("pane: {} hook (lateboot stub)", ctx.cast.pane_cli_label),
        &plan.env,
        ctx.ev,
    )?;
    stub::trigger(sb, stub::Mode::LateBoot).map_err(Abort::Scenario)?;
    let fired = wait_file(&stub_log, Duration::from_secs(20), |s| {
        s.lines()
            .any(|l| l.starts_with(&incarnation) && l.contains(stub::HOOK_RAN))
    });
    let got = wait_file(&stub_log, Duration::from_secs(20), |s| {
        count_lines(s, &incarnation, submitted_prefix) > 0
    });
    // Anything the recovery would do twice would have happened by now; give it
    // a margin past the probe interval anyway.
    std::thread::sleep(Duration::from_secs(3));
    let log = read(&stub_log);
    let exact = format!("{submitted_prefix}{pointer}");
    let submissions: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with(&incarnation) && l.contains(submitted_prefix))
        .collect();
    let ok = fired && got && submissions.len() == 1 && submissions[0].trim_end().ends_with(&exact);
    let recovery_logged = log_text(sb)
        .lines()
        .filter(|l| l.contains("delegate: submitted whatever the worker's input box was holding"))
        .count();
    ctx.ev.excerpt("lateboot stub log", log.clone());
    ctx.ev
        .excerpt("lateboot pane after the late SessionStart", ctx.tui.grid());
    ctx.ev.tell(
        "probe-late-session-start",
        "a late SessionStart from the old hook CLI submits the parked pointer exactly once",
        verdict(ok),
        format!(
            "delegated with `{cli} delegate --to {ROLE_LATEBOOT}`; the branch daemon respawned the \
             stub (pid {incarnation} in the stub log), waited out its fixed 30 s readiness window \
             and wrote the pointer, whose first CR the stub swallowed (`{swallowed}`); 2 s quiet \
             control held.\n\
             then the stub itself ran `{cli} hook --agent claude-code` with a `SessionStart` \
             (`source: startup`): {fired}.\n\
             submissions recorded by that incarnation afterwards: {n} — {subs}\n\
             expected exactly one, ending `{exact}`.\n\
             the branch daemon's own INFO line for the recovery (\"delegate: submitted whatever \
             the worker's input box was holding…\") appears {recovery_logged} time(s) in the log.",
            cli = ctx.cast.pane_cli,
            fired = if fired { "it ran" } else { "it NEVER ran" },
            n = submissions.len(),
            subs = if submissions.is_empty() {
                "<none>".to_string()
            } else {
                submissions
                    .iter()
                    .map(|l| format!("`{}`", l.trim()))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
    );
    Ok(())
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn wait_file(path: &Path, timeout: Duration, pred: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred(&read(path)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Lines of a stub log written by `pid` that contain `needle`.
fn count_lines(log: &str, pid: &str, needle: &str) -> usize {
    log.lines()
        .filter(|l| !pid.is_empty() && l.starts_with(pid) && l.contains(needle))
        .count()
}

// ---------------------------------------------------------------------------
// #1169 / issue #1082 — log-field escaping
// ---------------------------------------------------------------------------

/// From the reviewer pane, the old hook CLI reports a `UserPromptSubmit` whose
/// session id carries a real newline once decoded (`xver-1082\nFORGED-LINE` in
/// the JSON), then an ordinary `Notification`. The branch daemon must write ONE
/// physical `Received event` record with the id escaped, no physical line
/// starting `FORGED-LINE`, and still apply the next event.
fn log_escaping(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let plan = ctx.g.plan;
    let sb = ctx.g.sb;
    println!("xver (inner): probe log-escaping — a newline-bearing session id");
    focus_role(ctx.tui, plan, ROLE_REVIEWER)?;
    type_hook(
        ctx,
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"xver-1082\nFORGED-LINE","prompt":"ordinary"}"#,
    )?;
    let received = wait_for_log(sb, STEP_TIMEOUT, |l| {
        l.lines()
            .any(|x| x.contains("Received event") && x.contains("xver-1082"))
    });
    type_hook(
        ctx,
        r#"{"hook_event_name":"Notification","session_id":"xver-1082-ok","message":"ordinary"}"#,
    )?;
    let rows = inner::wait_for_status(
        ctx.g,
        &ctx.cast.client_bin,
        &client_status_label(ctx.cast),
        DAEMON_TIMEOUT,
        ctx.ev,
        |rows| {
            rows.iter()
                .any(|r| r.role.contains(ROLE_REVIEWER) && r.status == "WaitingForInput")
        },
    )?;
    let log = log_text(sb);
    let escaped = r"session_id=xver-1082\nFORGED-LINE";
    let records: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains("Received event") && l.contains("xver-1082") && !l.contains("xver-1082-ok")
        })
        .collect();
    let escaped_records = records.iter().filter(|l| l.contains(escaped)).count();
    let forged: Vec<&str> = log
        .lines()
        .filter(|l| l.trim_start().starts_with("FORGED-LINE"))
        .collect();
    let alive = !ctx.daemon.has_exited();
    let card = match &rows {
        Ok(rows) => rows
            .iter()
            .find(|r| r.role.contains(ROLE_REVIEWER))
            .map(|r| r.status.clone())
            .unwrap_or_default(),
        Err(e) => format!("<{e}>"),
    };
    let tui_shows = ctx.tui.grid().contains("Needs Input");
    let ok = received
        && records.len() == 1
        && escaped_records == 1
        && forged.is_empty()
        && alive
        && rows.is_ok();
    ctx.ev.tell(
        "probe-log-escaping",
        "a newline-bearing session id is logged as one escaped record, and later events still land",
        verdict(ok),
        format!(
            "from inside the `{ROLE_REVIEWER}` pane: `printf '%s' '<UserPromptSubmit, session_id \
             \"xver-1082\\nFORGED-LINE\">' | {cli} hook --agent claude-code`, then an ordinary \
             `Notification` (session `xver-1082-ok`) the same way.\n\
             `Received event` records for the hostile id: {n} (expected 1), of which {e} carry the \
             escaped `{escaped}`: {records}\n\
             physical log lines starting `FORGED-LINE`: {f}{forged}\n\
             the branch daemon is {alive}; after the Notification `daemon status --json` (asked by \
             the {client} CLI) reports the `{ROLE_REVIEWER}` pane as `{card}`; the {client} TUI's \
             screen {tui}",
            cli = ctx.cast.pane_cli,
            n = records.len(),
            e = escaped_records,
            records = records
                .iter()
                .map(|l| format!("`{}`", l.trim()))
                .collect::<Vec<_>>()
                .join(", "),
            f = forged.len(),
            forged = if forged.is_empty() {
                String::new()
            } else {
                format!(
                    " — {}",
                    forged
                        .iter()
                        .map(|l| format!("`{}`", l.trim()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
            alive = if alive { "alive" } else { "GONE" },
            client = ctx.cast.client_side,
            tui = if tui_shows {
                "shows `Needs Input`"
            } else {
                "does not show `Needs Input` in the focused layout (the daemon's card state above \
                 is the verdict)"
            },
        ),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// #1187 / issue #925 — one session id from two panes
// ---------------------------------------------------------------------------

/// `alpha` and `beta`, two live shells, each run the old hook CLI with the SAME
/// session id: alpha `SessionStart` + `PreToolUse(Bash)`, beta `SessionStart` +
/// `Notification`. The branch daemon must keep two cards — alpha `Working` with
/// its tool, beta `WaitingForInput` — and a later `SessionEnd` from beta must
/// leave alpha alone. Pre-fix, beta's frames take over alpha's keyed session and
/// alpha's live status disappears from `daemon status`.
fn cross_pane_session_key(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let plan = ctx.g.plan;
    println!("xver (inner): probe cross-pane-session-key — one session id from two panes");
    focus_role(ctx.tui, plan, ROLE_ALPHA)?;
    type_hook(
        ctx,
        r#"{"hook_event_name":"SessionStart","session_id":"xver-shared","source":"startup"}"#,
    )?;
    type_hook(
        ctx,
        r#"{"hook_event_name":"PreToolUse","session_id":"xver-shared","tool_name":"Bash","tool_input":{"command":"printf alpha"}}"#,
    )?;
    focus_role(ctx.tui, plan, ROLE_BETA)?;
    type_hook(
        ctx,
        r#"{"hook_event_name":"SessionStart","session_id":"xver-shared","source":"startup"}"#,
    )?;
    type_hook(
        ctx,
        r#"{"hook_event_name":"Notification","session_id":"xver-shared","message":"beta waiting"}"#,
    )?;
    let label = client_status_label(ctx.cast);
    let separated = |rows: &[StatusRow]| {
        let a: Vec<&StatusRow> = rows.iter().filter(|r| r.role == ROLE_ALPHA).collect();
        let b: Vec<&StatusRow> = rows.iter().filter(|r| r.role == ROLE_BETA).collect();
        a.len() == 1
            && b.len() == 1
            && a[0].pane_id != b[0].pane_id
            && a[0].status == "Working"
            && a[0].tool == "Bash"
            && b[0].status == "WaitingForInput"
    };
    let first = inner::wait_for_status(
        ctx.g,
        &ctx.cast.client_bin,
        &label,
        DAEMON_TIMEOUT,
        ctx.ev,
        separated,
    )?;
    let first_rows = match &first {
        Ok(r) => r.clone(),
        Err(_) => {
            inner::daemon_status(ctx.g, &ctx.cast.client_bin, &label, ctx.ev)?.unwrap_or_default()
        }
    };
    ctx.ev.excerpt(
        "the old TUI after both panes reported one session id",
        ctx.tui.grid(),
    );
    // Then beta ends ITS session: alpha must remain.
    type_hook(
        ctx,
        r#"{"hook_event_name":"SessionEnd","session_id":"xver-shared","reason":"other"}"#,
    )?;
    std::thread::sleep(Duration::from_secs(3));
    let after_end =
        inner::daemon_status(ctx.g, &ctx.cast.client_bin, &label, ctx.ev)?.unwrap_or_default();
    let alpha_after: Vec<&StatusRow> = after_end.iter().filter(|r| r.role == ROLE_ALPHA).collect();
    let alpha_kept = alpha_after.len() == 1
        && alpha_after[0].status == "Working"
        && alpha_after[0].tool == "Bash";
    let focus = |rows: &[StatusRow]| {
        rows.iter()
            .filter(|r| r.role == ROLE_ALPHA || r.role == ROLE_BETA)
            .cloned()
            .collect::<Vec<_>>()
    };
    ctx.ev.tell(
        "probe-cross-pane-session-key",
        "one session id from two panes leaves two cards, each with its own status",
        verdict(first.is_ok() && alpha_kept),
        format!(
            "from inside `{ROLE_ALPHA}`: `SessionStart` then `PreToolUse(Bash, \"printf alpha\")`; \
             from inside `{ROLE_BETA}`: `SessionStart` then `Notification` — all four with \
             session id `xver-shared`, each through `{cli} hook --agent claude-code`, so each \
             carries its own pane's identity.\n\
             `daemon status --json` (asked by the {client} CLI): {first}\n\
             expected exactly one `{ROLE_ALPHA}` (`Working`, tool `Bash`) and one `{ROLE_BETA}` \
             (`WaitingForInput`) on distinct panes: {sep}\n\
             after `{ROLE_BETA}` sent `SessionEnd` for the same id: {after} — `{ROLE_ALPHA}` {kept}",
            cli = ctx.cast.pane_cli,
            client = ctx.cast.client_side,
            first = render_rows(&focus(&first_rows)),
            sep = if first.is_ok() { "held" } else { "did NOT hold" },
            after = render_rows(&focus(&after_end)),
            kept = if alpha_kept {
                "kept its status and tool"
            } else {
                "did NOT keep its status and tool"
            },
        ),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// #1188 / issue #1129 — SignalAck against fire-and-forget clients
// ---------------------------------------------------------------------------

/// The generic tells already had the old CLI send `work-done` (fire and
/// forget) to the branch daemon, which now writes an acknowledgement nobody
/// reads. This adds the other acknowledged verb, `dispatch`, from the
/// orchestrator — its unit must come up in a real worktree holding the task —
/// and then one more old-CLI status event, which must still land: an
/// acknowledgement written to a peer that already closed must not wedge the
/// hook listener.
fn signal_ack(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let plan = ctx.g.plan;
    let sb = ctx.g.sb;
    println!("xver (inner): probe signal-ack — old-CLI dispatch, then a status event");
    let sentinel = "XVER-1129-REV-DISPATCH";
    let (worktree, unit) = dispatch_unit(ctx, "xver-1129-rev", &format!("'{sentinel}'"))?;
    let listed = worktree_list(sb, &sb.project);
    let in_list = listed.contains(&format!("worktree {}", worktree.display()));
    let record = probe::dispatched_record(sb);
    let handed = wait_file(&record, STEP_TIMEOUT, |s| s.contains(sentinel));
    ctx.ev
        .excerpt("the orchestrator pane after the dispatch", ctx.tui.grid());

    focus_role(ctx.tui, plan, ROLE_REVIEWER)?;
    ctx.g.preconnect_logged(
        &format!("pane: {} agent-event", ctx.cast.pane_cli_label),
        &plan.env,
        ctx.ev,
    )?;
    type_into_pane(
        ctx.tui,
        &format!("{} agent-event --type waiting", ctx.cast.pane_cli),
    );
    let after = inner::wait_for_status(
        ctx.g,
        &ctx.cast.client_bin,
        &client_status_label(ctx.cast),
        DAEMON_TIMEOUT,
        ctx.ev,
        |rows| {
            rows.iter()
                .any(|r| r.role.contains(ROLE_REVIEWER) && r.status == "WaitingForInput")
        },
    )?;
    let ok = unit.is_ok() && in_list && handed && after.is_ok();
    ctx.ev.tell(
        "probe-signal-ack",
        "old fire-and-forget work-done and dispatch take effect on the acknowledging branch daemon",
        verdict(ok),
        format!(
            "work-done: tell 4 above — the {client} CLI's fire-and-forget `work-done` reached the \
             orchestrator through the branch daemon, which writes a `signal_ack` it never reads.\n\
             dispatch: `{cli} dispatch xver-1129-rev --single --task '{sentinel}'` from the \
             orchestrator pane. The unit {unit}; `git worktree list` of the project {listed}; the \
             unit {handed} (recorded in `{record}`).\n\
             listener: afterwards `{cli} agent-event --type waiting` from the `{ROLE_REVIEWER}` \
             pane {after}.",
            client = ctx.cast.client_side,
            cli = ctx.cast.pane_cli,
            unit = match &unit {
                Ok(r) => format!("came up: {}", render_rows(std::slice::from_ref(r))),
                Err(e) => format!("NEVER came up at `{}`: {e}", worktree.display()),
            },
            listed = if in_list {
                format!("lists `{}`", worktree.display())
            } else {
                format!("does NOT list `{}`:\n{listed}", worktree.display())
            },
            handed = if handed {
                format!("was handed a task carrying `{sentinel}`")
            } else {
                format!("was NOT handed `{sentinel}` within {STEP_TIMEOUT:?}")
            },
            record = record.display(),
            after = match &after {
                Ok(_) => "landed — the reviewer's card reads `WaitingForInput`".to_string(),
                Err(e) => format!("did NOT land: {e}"),
            },
        ),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// #1190 / issue #1181 — spawned git children under hostile location variables
// ---------------------------------------------------------------------------

/// Every process of the run carries #1181's eight location variables pointed
/// at a decoy repository. An old-CLI dispatch from the orchestrator pane must
/// create its worktree from the INTENDED repository — the project — and leave
/// the decoy byte-for-byte as it was.
///
/// Two anti-vacuity controls: raw `git` under the run's own environment must
/// resolve the DECOY (the hostile variables are live), and the decoy must be
/// unchanged by everything BEFORE the dispatch too — a change there is reported
/// as an observation about another call site, not attributed to dispatch.
fn git_env(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let plan = ctx.g.plan;
    let sb = ctx.g.sb;
    println!("xver (inner): probe git-env — dispatch under hostile git location variables");
    let decoy = probe::decoy_repo(sb);
    let mut raw = std::process::Command::new("/usr/bin/git");
    raw.args(["rev-parse", "--absolute-git-dir"])
        .current_dir(&sb.project)
        .env_clear();
    for (k, v) in &plan.env {
        raw.env(k, v);
    }
    let resolved = raw
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let hostile_live = Path::new(&resolved) == decoy.join(".git");
    if !hostile_live {
        ctx.ev.tell(
            "probe-git-env",
            "a dispatch under hostile git location variables changes only the intended repository",
            Verdict::NotChecked,
            format!(
                "anti-vacuity control failed: raw `git rev-parse --absolute-git-dir` in `{}` under \
                 the run's environment printed {resolved:?}, not the decoy's `{}` — the hostile \
                 variables are not live, so a clean decoy would prove nothing",
                sb.project.display(),
                decoy.join(".git").display()
            ),
        );
        return Ok(());
    }
    let decoy_before = snapshot(sb, &decoy);
    let before_daemon: Option<BTreeMap<String, String>> =
        std::fs::read_to_string(decoy_baseline(sb))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
    let earlier = match &before_daemon {
        Some(b) => {
            let d = diff(b, &decoy_before);
            if d.is_empty() {
                "byte-for-byte unchanged".to_string()
            } else {
                format!(
                    "CHANGED before any dispatch — another call site, not attributed to the \
                     dispatch: {}",
                    d.join("; ")
                )
            }
        }
        None => "NOT MEASURED — the pre-daemon snapshot is missing".to_string(),
    };
    let intended_before = snapshot(sb, &sb.project);
    let (worktree, unit) = dispatch_unit(ctx, "xver-1181-rev", "'XVER-1181'")?;
    // Let any trailing git child finish before the after-snapshot.
    std::thread::sleep(Duration::from_secs(2));
    let decoy_after = snapshot(sb, &decoy);
    let intended_after = snapshot(sb, &sb.project);
    let decoy_diff = diff(&decoy_before, &decoy_after);
    let listed = worktree_list(sb, &sb.project);
    let in_list = listed.contains(&format!("worktree {}", worktree.display()));
    let common = sandbox::sandbox_git(sb, &worktree)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let rooted = Path::new(&common) == sb.project.join(".git");
    let intended_diff = diff(&intended_before, &intended_after);
    let ok = unit.is_ok() && in_list && rooted && decoy_diff.is_empty();
    ctx.ev.tell(
        "probe-git-env",
        "a dispatch under hostile git location variables changes only the intended repository",
        verdict(ok),
        format!(
            "every process of the run carries GIT_DIR, GIT_WORK_TREE, GIT_COMMON_DIR, \
             GIT_INDEX_FILE, GIT_OBJECT_DIRECTORY and GIT_ALTERNATE_OBJECT_DIRECTORIES pointed at \
             the decoy `{decoy}`, GIT_NAMESPACE=xver-1181 and GIT_DISCOVERY_ACROSS_FILESYSTEM=1; \
             control: raw `git rev-parse --absolute-git-dir` in the project under that \
             environment printed `{resolved}` — the variables are live.\n\
             `{cli} dispatch xver-1181-rev --single --task 'XVER-1181'` from the orchestrator \
             pane: the unit {unit}.\n\
             the project's `git worktree list` {listed}; the worktree's common dir is `{common}` \
             ({rooted}).\n\
             decoy (HEAD, refs, worktree list, status, index, objects, sentinel, config, \
             info/exclude), before vs after the dispatch: {decoy_diff}\n\
             decoy, before the daemon started vs just before the dispatch (an observation, not \
             part of the verdict): {earlier}\n\
             intended repository, before vs after: {intended_diff}",
            decoy = decoy.display(),
            cli = ctx.cast.pane_cli,
            unit = match &unit {
                Ok(r) => format!("came up: {}", render_rows(std::slice::from_ref(r))),
                Err(e) => format!("NEVER came up at `{}`: {e}", worktree.display()),
            },
            listed = if in_list {
                format!("lists `{}`", worktree.display())
            } else {
                format!("does NOT list `{}`", worktree.display())
            },
            rooted = if rooted {
                "the intended repository"
            } else {
                "NOT the intended repository"
            },
            decoy_diff = if decoy_diff.is_empty() {
                "byte-for-byte unchanged".to_string()
            } else {
                format!("CHANGED — {}", decoy_diff.join("; "))
            },
            intended_diff = if intended_diff.is_empty() {
                "unchanged".to_string()
            } else {
                intended_diff.join("; ")
            },
        ),
    );
    Ok(())
}

/// Everything about a repository a misdirected git child could have changed,
/// read with a clean environment.
fn snapshot(sb: &Sandbox, repo: &Path) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let git = |args: &[&str]| {
        sandbox::sandbox_git(sb, repo)
            .args(args)
            .output()
            .map(|o| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            })
            .unwrap_or_else(|e| format!("<{e}>"))
    };
    m.insert("HEAD".into(), git(&["rev-parse", "HEAD"]));
    m.insert(
        "refs".into(),
        git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
    );
    m.insert(
        "worktrees".into(),
        git(&["worktree", "list", "--porcelain"]),
    );
    m.insert(
        "status".into(),
        git(&[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored",
        ]),
    );
    let dotgit = repo.join(".git");
    for (key, rel) in [
        ("index", "index"),
        ("config", "config"),
        ("info/exclude", "info/exclude"),
        ("HEAD file", "HEAD"),
    ] {
        m.insert(key.into(), digest(&dotgit.join(rel)));
    }
    m.insert("objects".into(), tree_listing(&dotgit.join("objects")));
    m.insert("refs dir".into(), tree_listing(&dotgit.join("refs")));
    m.insert(
        "worktrees dir".into(),
        tree_listing(&dotgit.join("worktrees")),
    );
    for f in ["DECOY-SENTINEL", ".dot-agent-deck.toml"] {
        let p = repo.join(f);
        if p.exists() {
            m.insert(format!("file {f}"), digest(&p));
        }
    }
    m
}

/// A file's length and 64-bit FNV-1a hash — enough to say "changed", which is
/// all a comparison needs.
fn digest(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(b) => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in &b {
                h ^= u64::from(*byte);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            format!("{} bytes, fnv1a {h:016x}", b.len())
        }
        Err(e) => format!("<{e}>"),
    }
}

/// Every file under `dir`, with its size, sorted.
fn tree_listing(dir: &Path) -> String {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(_) => {
                    let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                    out.push(format!(
                        "{} {len}",
                        p.strip_prefix(dir).unwrap_or(&p).display()
                    ));
                }
                Err(_) => {}
            }
        }
    }
    out.sort();
    out.join("\n")
}

fn diff(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .map(|k| {
            format!(
                "{k}: {:?} → {:?}",
                before.get(k).map(|s| s.trim()).unwrap_or("<absent>"),
                after.get(k).map(|s| s.trim()).unwrap_or("<absent>")
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// #1183 / issue #1182 — a delivery confirmed through Claude's paste envelope
// ---------------------------------------------------------------------------

/// An old-CLI `dispatch --single` from the orchestrator pane makes the branch
/// daemon deliver a multi-line prompt (the task plus the dispatch instructions)
/// as a bracketed paste to a Claude-typed unit — the stub. The stub captures the
/// exact pasted payload and, on the submit CR, reports it through the OLD hook
/// CLI wrapped in Claude's `<pasted_content id="57b9">` envelope, which v0.41.0
/// leaves intact (and truncates, as it truncates every prompt). The branch
/// daemon must confirm the delivery with `confirmation="paste-envelope"`, and
/// never retry, probe or abandon it.
fn paste_envelope(ctx: &mut Ctx<'_, '_>) -> Result<(), Abort> {
    let sb = ctx.g.sb;
    println!("xver (inner): probe paste-envelope — a dispatch the stub confirms in an envelope");
    let (worktree, unit) = dispatch_unit(
        ctx,
        "xver-1182-rev",
        "\"$(printf 'XVER-1182-LINE-ONE\\nXVER-1182-LINE-TWO')\"",
    )?;
    let confirmed = wait_for_log(sb, Duration::from_secs(90), |l| {
        l.lines().any(|x| x.contains("paste-envelope"))
    });
    // Through the rest of the confirmation window: a retry or an abandonment
    // would be logged by now if the confirmation had not stopped the chain.
    std::thread::sleep(Duration::from_secs(20));
    let log = log_text(sb);
    let confirmations: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains("confirmation=\"paste-envelope\"")
                || l.contains("confirmation=paste-envelope")
        })
        .collect();
    let bad: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains("prompt delivery unconfirmed")
                || l.contains("probing submit")
                || l.contains("abandoning")
                || l.contains("delivery cannot be confirmed by this agent")
        })
        .collect();
    let stub_log = read(&stub::log_path(sb, stub::Mode::PasteEnvelope));
    let pastes = stub_log
        .lines()
        .filter(|l| l.contains(stub::PASTE_CAPTURED))
        .count();
    let reports = stub_log
        .lines()
        .filter(|l| l.contains(stub::HOOK_RAN))
        .count();
    let ok = unit.is_ok()
        && confirmed
        && confirmations.len() == 1
        && bad.is_empty()
        && pastes == 1
        && reports == 2; // SessionStart at boot, then the one UserPromptSubmit
    ctx.ev.excerpt("paste-envelope stub log", stub_log.clone());
    ctx.ev.tell(
        "probe-paste-envelope",
        "the branch daemon confirms a delivery the old hook CLI reports inside the paste envelope",
        verdict(ok),
        format!(
            "`{cli} dispatch xver-1182-rev --single --task <two lines>` from the orchestrator \
             pane: the unit {unit}. The daemon delivered a multi-line prompt to the Claude-typed \
             stub, which captured {pastes} bracketed paste(s) and ran `{cli} hook --agent \
             claude-code` {reports} time(s) (a `SessionStart` at boot, then the \
             `UserPromptSubmit` whose prompt is `\\n\\n<pasted_content id=\"57b9\">\\n<the exact \
             pasted payload>\\n</pasted_content id=\"57b9\">`).\n\
             `confirmation=\"paste-envelope\"` records: {n} (expected 1) — {conf}\n\
             retry / probe / abandon / unconfirmable records for the spawn path, through 20 s past \
             the confirmation: {b}{bad}",
            cli = ctx.cast.pane_cli,
            unit = match &unit {
                Ok(r) => format!("came up: {}", render_rows(std::slice::from_ref(r))),
                Err(e) => format!("NEVER came up at `{}`: {e}", worktree.display()),
            },
            n = confirmations.len(),
            conf = confirmations
                .iter()
                .map(|l| format!("`{}`", l.trim()))
                .collect::<Vec<_>>()
                .join(", "),
            b = bad.len(),
            bad = if bad.is_empty() {
                String::new()
            } else {
                format!(
                    " — {}",
                    bad.iter()
                        .map(|l| format!("`{}`", l.trim()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_diff_names_exactly_the_keys_that_changed() {
        let a: BTreeMap<String, String> = [
            ("HEAD".to_string(), "abc".to_string()),
            ("refs".to_string(), "r".to_string()),
        ]
        .into();
        let mut b = a.clone();
        assert!(diff(&a, &b).is_empty());
        b.insert(
            "refs".into(),
            "r\nrefs/namespaces/xver-1181/refs/heads/x".into(),
        );
        b.insert("worktrees dir".into(), "x".into());
        let d = diff(&a, &b);
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d.iter().any(|l| l.starts_with("refs:")));
        assert!(d.iter().any(|l| l.contains("<absent>")));
    }

    #[test]
    fn a_digest_changes_with_the_content_and_names_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("xver-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let f = dir.join("f");
        std::fs::write(&f, b"one").expect("write");
        let one = digest(&f);
        std::fs::write(&f, b"two").expect("write");
        assert_ne!(one, digest(&f));
        assert!(digest(&dir.join("absent")).starts_with('<'));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stub_log_lines_are_counted_per_incarnation() {
        let log = "10 SWALLOW-STUB-SWALLOWED:x\n11 SWALLOW-STUB-SUBMITTED:x\n10 SWALLOW-STUB-SUBMITTED:x\n";
        assert_eq!(count_lines(log, "10", stub::SUBMITTED), 1);
        assert_eq!(count_lines(log, "11", stub::SUBMITTED), 1);
        assert_eq!(
            count_lines(log, "", stub::SUBMITTED),
            0,
            "an unknown incarnation matches nothing"
        );
    }

    #[test]
    fn the_discovery_probe_needs_a_per_uid_branch_daemon() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let mut plan: Plan = serde_json::from_value(serde_json::json!({
            "root": "/srv/runs/r1", "uid": 1000, "user": "op",
            "mode": "Resolved", "keep_xdg_runtime_dir": false, "experimental": false,
            "max_lifetime_secs": 1, "previous": "v0.41.0", "direction": "Reverse",
            "probe": "DiscoveryFallback", "fixture": "", "extra_env": [], "env": [],
            "matrices": [], "masks": [], "masked_home": null, "outer_mnt_ns": "mnt:[1]"
        }))
        .expect("plan");
        let [per_uid, flat] = <[EndpointMatrix; 2]>::try_from(EndpointMatrix::candidates(
            &sb,
            crate::sandbox::EndpointMode::Resolved,
            false,
            1000,
            crate::sandbox::Direction::Reverse,
        ))
        .expect("two candidates");
        assert!(daemon_layout_precondition(&plan, &per_uid).is_none());
        assert!(
            daemon_layout_precondition(&plan, &flat)
                .is_some_and(|m| m.contains("does not carry #1121's layout"))
        );
        plan.probe = Probe::Generic;
        assert!(daemon_layout_precondition(&plan, &flat).is_none());
    }
}
