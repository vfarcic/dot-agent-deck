//! PRD #1790 stage 3: the daemon side of bringing an interrupted agent back —
//! [`crate::daemon_protocol::AttachRequest::RestoreInterrupted`] (with its
//! conversation) and `StartOverInterrupted` (without it, in the folder it ran
//! in).
//!
//! **Nothing the list said is trusted.** Each request reads the journal, the
//! launch records and the kept reports afresh
//! ([`crate::interrupted_work_daemon::snapshot`]), resolves the item, and
//! checks again, at act time and in a dry run alike, what
//! `docs/develop/work-journal.md` ("For the PRDs that build on it") asks of a
//! consumer:
//!
//! 1. the journal read and the launch-record read are complete
//!    ([`crate::launch_records::read_all`], never the partial set);
//! 2. the attempt has exactly one launch record, whose work is one of the
//!    item's works;
//! 3. the item is still interrupted (nothing of it is live, it was not
//!    dismissed, reclaimed, restored or started over) and the agent's verdict
//!    offers what was asked;
//! 4. for a restore, the conversation id the journal holds
//!    ([`crate::work_journal::latest_session_of`]) is of the agent's own
//!    syntax ([`crate::agent_registry::ResumeSpec::valid_session_id`]); it
//!    reaches the agent only as its own argument, through the launch shim
//!    ([`crate::telemetry::shim`]), never in the command string, which runs
//!    exactly as it was recorded;
//! 5. the folder is the recorded one, absolute, on disk, opened without
//!    following a final symlink, and, by owner and mode bits, one only this
//!    account can change, as is every directory on its path
//!    ([`crate::orchestrator_context::resolve_trusted_dir_path`],
//!    [`crate::orchestrator_context::project_root_refusal`]); the agent starts
//!    through that held descriptor ([`AgentPtyRegistry::spawn_agent_in`]);
//! 6. the old process is gone: no process with the record's pid and start
//!    time ([`crate::launch_records::process_gone`]), and on Linux no process
//!    left in the old agent's session ([`old_session_has_members`]). A record
//!    without the pid fields (written by a build before stage 3) refuses a
//!    restore (`old-process-unknown`) but not a start over;
//! 7. for a start over of a dispatched unit, the saved task is read without
//!    following a symlink, a regular owner-only file of bounded size
//!    ([`crate::dispatch_task::read_saved`]);
//! 8. the request came from the person: one carrying a pane claim
//!    ([`CallerClaim`]) is refused, as reclaim's is (the same cooperative
//!    guard, not an authentication boundary).
//!
//! Restores and start-overs of one attempt are serialized ([`InFlight`]): a
//! second request while one runs is refused, so two clients cannot start two
//! writers into one conversation.
//!
//! **Restore.** The agent is started as a dashboard pane of its own (a fresh
//! pane work, `launch: pane`), with the recorded command, `SHELL` override,
//! agent type and name, a one-shot resume request in its environment
//! ([`crate::telemetry::shim::RESUME_ENV_KEYS`]) and the shim directory first
//! on that one spawn's `PATH`. It is confirmed only when its first attested
//! hook session ([`AgentPtyRegistry::watch_restore_session`]) is the requested
//! one, within [`CONFIRM_TIMEOUT`]. A different session, a refused
//! acknowledgement from the shim, the agent exiting, or no confirmation in
//! time stops the new agent (its card and its process group) before anything
//! is posted to it, writes `restore_failed` for the attempt (so the list
//! offers Start over from then on) and answers `not-resumed`. A confirmed
//! restore posts the restart note as the agent's first inbox message (reason
//! `restart note`), then writes `closed how=restored` on the item's works and
//! `attempt_restored{how: resumed}`.
//!
//! **Start over.** The recorded command, in the verified folder, with no
//! resume request; a dispatched unit's saved task is posted as its first
//! inbox message (reason `saved task`), and no restart note: the agent is new
//! and has no conversation to be told about. Then `closed how=started_over`
//! and `attempt_restored{how: started_over}`.
//!
//! **What this does not guard against.** The checks run once each, before the
//! spawn; another process can change the folder or start an agent there in
//! between, as for reclaim. The descendant scan is Linux-only. A launch the
//! shim cannot reach (the agent called by an absolute path, `PATH`
//! replaced, a wrapper that drops arguments) is never resumed: it is caught
//! by the confirmation and answered `not-resumed`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use crate::daemon_protocol::CallerClaim;
use crate::event::BroadcastMsg;
use crate::interrupted_work::{
    CloseSets, InterruptedItem, ItemKind, Resolved, RestoreMode, RestoreOutcome, RestoreRefusal,
    RestoreRefused, RestoreVerdict, RestoredAgent, StartOverOutcome, resolve, still_interrupted,
};
use crate::launch_records::{LaunchRecord, ProcessNotGone};
use crate::work_journal::{
    AttemptId, CloseHow, Event, RestoreFailure, RestoredHow, WorkId, WorkJournal,
};

/// How long a restore waits for the restored agent to confirm its
/// conversation.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the confirmation looks.
const CONFIRM_POLL: Duration = Duration::from_millis(50);

/// The inbox reason the restart note is posted under.
pub const RESTART_NOTE_REASON: &str = "restart note";

/// The inbox reason a start-over's saved task is posted under.
pub const SAVED_TASK_REASON: &str = "saved task";

/// The bound on a restore's confirmation: [`CONFIRM_TIMEOUT`], or in an e2e
/// build the seconds `DOT_AGENT_DECK_TEST_RESTORE_CONFIRM_SECS` names.
fn confirm_timeout() -> Duration {
    #[cfg(feature = "e2e")]
    if let Some(secs) = std::env::var("DOT_AGENT_DECK_TEST_RESTORE_CONFIRM_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_secs(secs);
    }
    CONFIRM_TIMEOUT
}

/// The attempts a restore or start-over is running for.
static IN_FLIGHT: LazyLock<Mutex<HashSet<AttemptId>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// One attempt held against a second restore or start-over until dropped.
struct InFlight(AttemptId);

impl InFlight {
    fn take(attempt: &AttemptId) -> Option<Self> {
        let mut held = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        held.insert(attempt.clone())
            .then(|| InFlight(attempt.clone()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// What was asked of an agent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Resume,
    StartOver,
}

/// One agent the checks passed, ready to start.
struct Target {
    attempt: AttemptId,
    record: LaunchRecord,
    /// The conversation to resume into (a restore).
    session: Option<String>,
    /// The program the shim resumes (a restore).
    program: Option<&'static str>,
    /// The saved task to post first (a dispatched unit's start over).
    task: Option<String>,
}

/// What the journal checks found for a request.
struct Prepared {
    journal: WorkJournal,
    /// The item's works, root first, which a success closes.
    works: Vec<WorkId>,
    targets: Vec<Result<Target, RestoreRefused>>,
}

/// The journal checks of a request: 1–4, 6 and 7 of the module docs.
/// Blocking.
fn prepare(
    journal: WorkJournal,
    retained: Option<&crate::retained_store::RetainedStore>,
    id: &str,
    attempts: &[String],
    want: Want,
) -> Result<Prepared, RestoreRefused> {
    let refuse = |reason| RestoreRefused::new(None, reason);
    let snap = crate::interrupted_work_daemon::snapshot(&journal, retained)
        .map_err(|_| refuse(RestoreRefusal::Unreadable))?;
    if snap.read.stats.truncated || snap.list.incomplete {
        return Err(refuse(RestoreRefusal::IncompleteRead));
    }
    let item: InterruptedItem = match resolve(&snap.list, id) {
        Resolved::One(item) => item.clone(),
        Resolved::NotListed => return Err(refuse(RestoreRefusal::NotListed)),
        Resolved::Ambiguous => return Err(refuse(RestoreRefusal::Ambiguous)),
    };
    if !still_interrupted(&snap.read, &item.id, |run| journal.run_is_live(run)) {
        return Err(refuse(RestoreRefusal::NotListed));
    }
    let Some(state_dir) = journal.state_dir() else {
        return Err(refuse(RestoreRefusal::IncompleteRead));
    };
    let launches = crate::launch_records::read_all(state_dir)
        .map_err(|_| refuse(RestoreRefusal::IncompleteRead))?;
    let works = CloseSets::build(&snap.read).works_under(&item.id);
    let chosen: Vec<Result<&crate::interrupted_work::ItemAgent, RestoreRefused>> = if attempts
        .is_empty()
    {
        let offered: Vec<_> = item
            .agents
            .iter()
            .filter(|agent| offers(agent.restore, want))
            .map(Ok)
            .collect();
        if offered.is_empty() {
            return Err(refuse(RestoreRefusal::NotRestorable));
        }
        offered
    } else {
        attempts
            .iter()
            .map(|wanted| {
                item.agents
                    .iter()
                    .find(|agent| agent.attempt.as_str() == wanted)
                    .ok_or_else(|| {
                        RestoreRefused::new(AttemptId::parse(wanted), RestoreRefusal::NotAnAgent)
                    })
            })
            .collect()
    };
    let targets = chosen
        .into_iter()
        .map(|agent| {
            let agent = agent?;
            check_agent(&snap.read, &launches, &works, &item, agent, want)
                .map_err(|reason| RestoreRefused::new(Some(agent.attempt.clone()), reason))
        })
        .collect();
    Ok(Prepared {
        journal,
        works,
        targets,
    })
}

/// Whether a verdict offers what was asked: a restore needs
/// [`RestoreVerdict::Resume`]; a start over is offered to an agent that can
/// be resumed or started over.
fn offers(verdict: RestoreVerdict, want: Want) -> bool {
    match want {
        Want::Resume => verdict == RestoreVerdict::Resume,
        Want::StartOver => matches!(
            verdict,
            RestoreVerdict::Resume | RestoreVerdict::StartOver { .. }
        ),
    }
}

/// The checks of one agent of the item.
fn check_agent(
    read: &crate::work_journal::JournalRead,
    launches: &[LaunchRecord],
    works: &[WorkId],
    item: &InterruptedItem,
    agent: &crate::interrupted_work::ItemAgent,
    want: Want,
) -> Result<Target, RestoreRefusal> {
    if !offers(agent.restore, want) {
        return Err(RestoreRefusal::NotRestorable);
    }
    let mut records = launches.iter().filter(|r| r.attempt == agent.attempt);
    let record = match (records.next(), records.next()) {
        (None, _) => return Err(RestoreRefusal::NoLaunchRecord),
        (Some(_), Some(_)) => return Err(RestoreRefusal::AmbiguousRecord),
        (Some(record), None) => record.clone(),
    };
    if !works.contains(&record.work) {
        return Err(RestoreRefusal::AmbiguousRecord);
    }
    match (&record.cwd, &record.cwd_moved_from) {
        (Some(_), _) => {}
        (None, Some(_)) => return Err(RestoreRefusal::FolderMoved),
        (None, None) => return Err(RestoreRefusal::FolderUnknown),
    }
    let (session, program) = match want {
        Want::StartOver => (None, None),
        Want::Resume => {
            let Some((session, reported)) =
                crate::work_journal::latest_session_of(&read.lines, &agent.attempt)
            else {
                return Err(RestoreRefusal::NoSessionId);
            };
            let agent_type = record
                .agent_type
                .clone()
                .unwrap_or_else(|| reported.clone());
            let Some(resume) = crate::agent_registry::spec(&agent_type).resume else {
                return Err(RestoreRefusal::NotRestorable);
            };
            if !resume.valid_session_id(session) {
                return Err(RestoreRefusal::SessionIdInvalid);
            }
            (Some(session.to_string()), Some(resume.program))
        }
    };
    match crate::launch_records::process_gone(&record) {
        Ok(()) => {}
        Err(ProcessNotGone::StillRunning) => return Err(RestoreRefusal::StillRunning),
        // A start over writes to no old conversation; a resume would.
        Err(ProcessNotGone::OldProcessUnknown) if want == Want::Resume => {
            return Err(RestoreRefusal::OldProcessUnknown);
        }
        Err(ProcessNotGone::OldProcessUnknown) => {}
    }
    if let Some(pid) = record.pid
        && old_session_has_members(pid)
    {
        return Err(RestoreRefusal::StillRunning);
    }
    let task = match want {
        Want::StartOver if matches!(item.kind, ItemKind::Unit | ItemKind::IssueDispatch) => {
            let folder = Path::new(record.cwd.as_deref().unwrap_or_default());
            match crate::dispatch_task::read_saved(folder, &record.work) {
                Ok(task) => Some(task),
                Err(crate::dispatch_task::SavedTaskError::NotSaved) => {
                    return Err(RestoreRefusal::NoSavedTask);
                }
                Err(crate::dispatch_task::SavedTaskError::Unsafe) => {
                    return Err(RestoreRefusal::TaskUnsafe);
                }
            }
        }
        _ => None,
    };
    Ok(Target {
        attempt: agent.attempt.clone(),
        record,
        session,
        program,
        task,
    })
}

/// Whether a process other than the old agent itself is still in the session
/// the old agent `pid` led (Linux): a child it left behind that could still
/// write to its conversation or worktree. Asked only while `pid` names no
/// process: Linux does not hand out a pid that is still some process's
/// session id, so a live `pid` (another process, its start time differs,
/// [`crate::launch_records::process_gone`]) means that session emptied
/// before. A `/proc` that cannot be listed answers `true`, the safe side.
/// Elsewhere `false`: not looked for.
#[cfg(target_os = "linux")]
fn old_session_has_members(pid: u32) -> bool {
    if crate::platform::proc::process_start_of(pid).is_some() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|stat| session_of_stat(&stat))
            == Some(pid)
    })
}

#[cfg(not(target_os = "linux"))]
fn old_session_has_members(_pid: u32) -> bool {
    false
}

/// The session id in a `/proc/<pid>/stat` line: the fourth field after the
/// command name's closing parenthesis (state, ppid, pgrp, session).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn session_of_stat(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(3)?.parse().ok()
}

/// The folder check of the module docs' item 5, answering the held directory
/// the agent is started through. Blocking.
#[cfg(unix)]
fn verified_folder(
    cwd: &str,
) -> Result<crate::project_resolve::VerifiedProjectDir, RestoreRefusal> {
    use std::os::unix::fs::MetadataExt as _;
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err(RestoreRefusal::FolderUnknown);
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(RestoreRefusal::FolderMissing);
        }
        Err(_) => return Err(RestoreRefusal::FolderUnsafe),
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let resolved = crate::orchestrator_context::resolve_trusted_dir_path(
        path,
        euid,
        &crate::platform::paths::effective_private_group,
    )
    .map_err(|_| RestoreRefusal::FolderUnsafe)?;
    let dir = crate::project_resolve::VerifiedProjectDir::open(&resolved).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RestoreRefusal::FolderMissing
        } else {
            RestoreRefusal::FolderUnsafe
        }
    })?;
    let meta = std::fs::symlink_metadata(&resolved).map_err(|_| RestoreRefusal::FolderUnsafe)?;
    if crate::prep_token::inode_identity(&meta) != Some(dir.identity()) {
        return Err(RestoreRefusal::FolderUnsafe);
    }
    let private = if meta.mode() & 0o020 != 0 {
        crate::platform::paths::effective_private_group()
    } else {
        None
    };
    if crate::orchestrator_context::project_root_refusal(
        meta.uid(),
        meta.gid(),
        meta.mode() & 0o7777,
        euid,
        private,
    )
    .is_some()
    {
        return Err(RestoreRefusal::FolderUnsafe);
    }
    Ok(dir)
}

/// The pane id a restored or started-over agent runs in: `restored-`, its
/// name and six random hex characters, through
/// [`crate::spawn::next_pane_id`]. The daemon's pane counter starts again in
/// each daemon, so without the random part a restore would reuse the pane id
/// the agent it brings back had under the daemon before.
fn restored_pane_id(name: Option<&str>) -> String {
    let mut bytes = [0u8; 3];
    let _ = getrandom::fill(&mut bytes);
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    crate::spawn::next_pane_id(
        &format!("restored-{}-{suffix}", name.unwrap_or("agent")),
        None,
    )
}

/// A fresh one-shot resume token: 32 lowercase hex characters from OS
/// randomness ([`crate::telemetry::shim::is_resume_token`] accepts it).
fn resume_token() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The handles a restore needs beyond the registry, to stop an agent the way
/// `StopAgent` would.
pub struct Daemon<'a> {
    pub registry: &'a Arc<AgentPtyRegistry>,
    pub state: &'a crate::state::SharedState,
    pub event_tx: &'a broadcast::Sender<BroadcastMsg>,
}

/// Run the journal checks on the blocking pool.
async fn prepared(
    registry: &AgentPtyRegistry,
    id: String,
    attempts: Vec<String>,
    want: Want,
) -> Result<Prepared, RestoreRefused> {
    let (journal, retained) = crate::interrupted_work_daemon::sources(registry)
        .map_err(|_| RestoreRefused::new(None, RestoreRefusal::Unreadable))?;
    tokio::task::spawn_blocking(move || prepare(journal, retained.as_deref(), &id, &attempts, want))
        .await
        .unwrap_or_else(|_| Err(RestoreRefused::new(None, RestoreRefusal::Unreadable)))
}

/// Restore the item `id`'s agents `attempts` (empty: every agent of it a
/// restore is offered for) with their conversations, each posted `mode`'s
/// restart note once confirmed. With `dry_run`, run every check and start
/// nothing. Answers once each agent confirmed or was stopped.
pub async fn restore(
    daemon: Daemon<'_>,
    id: String,
    attempts: Vec<String>,
    mode: RestoreMode,
    dry_run: bool,
    caller: Option<&CallerClaim>,
) -> RestoreOutcome {
    let mut outcome = RestoreOutcome {
        dry_run,
        ..RestoreOutcome::default()
    };
    // Any claim, well formed or not: see the module docs' item 8.
    if caller.is_some() {
        outcome
            .refused
            .push(RestoreRefused::new(None, RestoreRefusal::AgentCaller));
        return outcome;
    }
    if !cfg!(unix) {
        outcome
            .refused
            .push(RestoreRefused::new(None, RestoreRefusal::Unsupported));
        return outcome;
    }
    let prepared = match prepared(daemon.registry, id, attempts, Want::Resume).await {
        Ok(prepared) => prepared,
        Err(refused) => {
            outcome.refused.push(refused);
            return outcome;
        }
    };
    let Prepared {
        journal,
        works,
        targets,
    } = prepared;
    for target in targets {
        let target = match target {
            Ok(target) => target,
            Err(refused) => {
                outcome.refused.push(refused);
                continue;
            }
        };
        match start(&daemon, &journal, &works, target, Some(mode), dry_run).await {
            Started::Would(attempt) => outcome.would_restore.push(attempt),
            Started::Done(agent) => outcome.restored.push(agent),
            Started::Refused(refused) => outcome.refused.push(refused),
        }
    }
    outcome
}

/// Start the item `id`'s agent `attempt` over, without its conversation, in
/// the folder it ran in, posting a dispatched unit's saved task first. With
/// `dry_run`, run every check and start nothing.
pub async fn start_over(
    daemon: Daemon<'_>,
    id: String,
    attempt: String,
    dry_run: bool,
    caller: Option<&CallerClaim>,
) -> StartOverOutcome {
    let refused = |refused: RestoreRefused| StartOverOutcome {
        dry_run,
        refused: Some(refused),
        ..StartOverOutcome::default()
    };
    if caller.is_some() {
        return refused(RestoreRefused::new(None, RestoreRefusal::AgentCaller));
    }
    if !cfg!(unix) {
        return refused(RestoreRefused::new(None, RestoreRefusal::Unsupported));
    }
    if attempt.is_empty() {
        return refused(RestoreRefused::new(None, RestoreRefusal::BadRequest));
    }
    let Prepared {
        journal,
        works,
        mut targets,
    } = match prepared(daemon.registry, id, vec![attempt], Want::StartOver).await {
        Ok(prepared) => prepared,
        Err(refusal) => return refused(refusal),
    };
    let target = match targets.pop() {
        Some(Ok(target)) => target,
        Some(Err(refusal)) => return refused(refusal),
        None => return refused(RestoreRefused::new(None, RestoreRefusal::BadRequest)),
    };
    match start(&daemon, &journal, &works, target, None, dry_run).await {
        Started::Would(_) => StartOverOutcome {
            dry_run,
            would_start: true,
            ..StartOverOutcome::default()
        },
        Started::Done(agent) => StartOverOutcome {
            dry_run,
            started: Some(agent),
            ..StartOverOutcome::default()
        },
        Started::Refused(refusal) => refused(refusal),
    }
}

/// What starting one agent came to.
enum Started {
    Would(AttemptId),
    Done(RestoredAgent),
    Refused(RestoreRefused),
}

/// Start `target`: resumed when `mode` is set, started over otherwise. The
/// folder is checked here, last before the spawn.
async fn start(
    daemon: &Daemon<'_>,
    journal: &WorkJournal,
    works: &[WorkId],
    target: Target,
    mode: Option<RestoreMode>,
    dry_run: bool,
) -> Started {
    let attempt = target.attempt.clone();
    let refuse = |reason| Started::Refused(RestoreRefused::new(Some(attempt.clone()), reason));
    let Some(_held) = InFlight::take(&target.attempt) else {
        return refuse(RestoreRefusal::AlreadyRestoring);
    };
    #[cfg(unix)]
    let dir = {
        let cwd = target.record.cwd.clone().unwrap_or_default();
        match tokio::task::spawn_blocking(move || verified_folder(&cwd)).await {
            Ok(Ok(dir)) => dir,
            Ok(Err(reason)) => return refuse(reason),
            Err(_) => return refuse(RestoreRefusal::FolderUnsafe),
        }
    };
    if let Some(task) = target.task.as_deref()
        && crate::spawn::admit_seed(daemon.registry, SAVED_TASK_REASON, task).is_err()
    {
        return refuse(RestoreRefusal::TaskUnsafe);
    }
    // The shim this restore resumes through, and where it acknowledges.
    let shim_dir =
        crate::telemetry::exporter::default_config_dir().join(crate::telemetry::shim::DIR_NAME);
    let acks = crate::telemetry::shim::resume_acks_dir(&shim_dir);
    if let Some(program) = target.program
        && (!shim_dir.join(program).is_file() || acks.is_none())
    {
        return refuse(RestoreRefusal::Unsupported);
    }
    if dry_run {
        return Started::Would(target.attempt);
    }
    if daemon.registry.is_shutting_down() || daemon.registry.is_admission_frozen() {
        return refuse(RestoreRefusal::ShuttingDown);
    }

    let record = &target.record;
    let pane_id = restored_pane_id(record.name.as_deref());
    let mut env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.clone())];
    if let Some(shell) = &record.shell {
        env.push(("SHELL".to_string(), shell.clone()));
    }
    let mut token = None;
    if let (Some(session), Some(program)) = (&target.session, target.program) {
        let Some(minted) = resume_token() else {
            return refuse(RestoreRefusal::SpawnFailed);
        };
        let Some(shim) = shim_dir.to_str() else {
            return refuse(RestoreRefusal::Unsupported);
        };
        let path = std::env::var("PATH").unwrap_or_default();
        env.push(("PATH".to_string(), format!("{shim}:{path}")));
        env.push((
            crate::telemetry::shim::RESUME_AGENT_ENV.to_string(),
            program.to_string(),
        ));
        env.push((
            crate::telemetry::shim::RESUME_SESSION_ENV.to_string(),
            session.clone(),
        ));
        env.push((
            crate::telemetry::shim::RESUME_TOKEN_ENV.to_string(),
            minted.clone(),
        ));
        token = Some(minted);
        daemon.registry.watch_restore_session(&pane_id);
    }
    let opts = SpawnOptions {
        command: record.command.as_deref(),
        cwd: record.cwd.as_deref(),
        display_name: record.name.as_deref(),
        rows: 24,
        cols: 80,
        env,
        tab_membership: None,
        agent_type: record.agent_type.clone(),
        origin: None,
        // A fresh pane work: the agent comes back as a dashboard pane of its
        // own, which every client shows through the ordinary events.
        work_id: None,
    };
    #[cfg(unix)]
    let spawned = daemon.registry.spawn_agent_in(opts, &dir);
    #[cfg(not(unix))]
    let spawned = daemon.registry.spawn_agent(opts);
    #[cfg(unix)]
    drop(dir);
    let agent_id = match spawned {
        Ok(id) => id,
        Err(error) => {
            daemon.registry.unwatch_restore_session(&pane_id);
            tracing::warn!(%error, attempt = %target.attempt, "restore: the agent could not be started");
            return refuse(RestoreRefusal::SpawnFailed);
        }
    };
    let started = crate::spawn::SpawnedAgent {
        id: agent_id.clone(),
        pane_id: pane_id.clone(),
        role_name: None,
    };

    let (how, close, first) = match (&target.session, mode) {
        (Some(session), Some(mode)) => {
            let confirmed = confirm(
                daemon.registry,
                &pane_id,
                &agent_id,
                session,
                acks.as_deref().zip(token.as_deref()),
                confirm_timeout(),
            )
            .await;
            daemon.registry.unwatch_restore_session(&pane_id);
            if let Err(failure) = confirmed {
                tracing::info!(
                    attempt = %target.attempt,
                    ?failure,
                    "restore: the agent did not come back into its conversation; stopping it"
                );
                crate::spawn::close_started_agents(
                    daemon.registry,
                    Some(daemon.state),
                    Some(daemon.event_tx),
                    std::slice::from_ref(&started),
                )
                .await;
                record_blocking(
                    journal,
                    vec![Event::RestoreFailed {
                        work: record.work.clone(),
                        attempt: target.attempt.clone(),
                        failure,
                    }],
                )
                .await;
                return refuse(RestoreRefusal::NotResumed);
            }
            (
                RestoredHow::Resumed,
                CloseHow::Restored,
                Some((RESTART_NOTE_REASON, mode.note().to_string())),
            )
        }
        _ => (
            RestoredHow::StartedOver,
            CloseHow::StartedOver,
            target.task.clone().map(|task| (SAVED_TASK_REASON, task)),
        ),
    };
    let first_message_posted = match &first {
        Some((reason, body)) => {
            crate::spawn::post_seed(daemon.registry, &pane_id, &agent_id, reason, body).is_ok()
        }
        None => true,
    };
    let to = agent_id
        .parse::<u64>()
        .ok()
        .and_then(|agent| journal.attempt_id(agent));
    let mut events: Vec<Event> = works
        .iter()
        .map(|work| Event::Closed {
            work: work.clone(),
            how: close,
        })
        .collect();
    if let Some(to) = to {
        events.push(Event::AttemptRestored {
            work: record.work.clone(),
            from: target.attempt.clone(),
            to,
            how,
        });
    }
    let recorded = record_blocking(journal, events).await;
    Started::Done(RestoredAgent {
        attempt: target.attempt,
        agent_id,
        pane_id,
        name: record.name.clone(),
        first_message_posted,
        recorded,
    })
}

/// Write `events`, waiting for each, on the blocking pool; `true` when all
/// were written.
async fn record_blocking(journal: &WorkJournal, events: Vec<Event>) -> bool {
    let journal = journal.clone();
    tokio::task::spawn_blocking(move || {
        let mut all = true;
        for event in events {
            if let Err(error) = journal.try_record(event) {
                tracing::warn!(?error, "restore: could not record");
                all = false;
            }
        }
        all
    })
    .await
    .unwrap_or(false)
}

/// Wait at most `bound` for the agent `agent_id` in `pane` to report its
/// first attested session, and answer whether it is `session`. Gives up
/// early on a refused acknowledgement from the shim (`ack`: its directory
/// and the request's token) and on the agent exiting.
async fn confirm(
    registry: &AgentPtyRegistry,
    pane: &str,
    agent_id: &str,
    session: &str,
    ack: Option<(&Path, &str)>,
    bound: Duration,
) -> Result<(), RestoreFailure> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some((generation, seen)) = registry.restore_session_seen(pane)
            && generation == agent_id
        {
            return if seen == session {
                Ok(())
            } else {
                Err(RestoreFailure::SessionMismatch)
            };
        }
        if let Some((dir, token)) = ack
            && dir
                .join(format!("{token}{}", crate::telemetry::shim::REFUSED_SUFFIX))
                .exists()
        {
            return Err(RestoreFailure::Refused);
        }
        if registry.agent_record_any(agent_id).is_none() || registry.pane_occupant_has_exited(pane)
        {
            return Err(RestoreFailure::Exited);
        }
        if Instant::now() >= deadline {
            let applied = ack.is_some_and(|(dir, token)| dir.join(token).exists());
            return Err(if applied {
                RestoreFailure::TimedOut
            } else {
                RestoreFailure::ShimNotReached
            });
        }
        tokio::time::sleep(CONFIRM_POLL).await;
    }
}

/// The agent type the shim resumes as, for a test.
#[cfg(test)]
fn program_of(agent: &crate::event::AgentType) -> Option<&'static str> {
    crate::agent_registry::spec(agent).resume.map(|r| r.program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_is_read_after_the_command_names_closing_parenthesis() {
        assert_eq!(
            session_of_stat("4242 (a (weird) name) S 1 4242 4100 0 -1"),
            Some(4100)
        );
        assert_eq!(session_of_stat("4242 (claude) S 1 4242"), None);
        assert_eq!(session_of_stat("no parenthesis"), None);
    }

    #[test]
    fn a_restored_pane_id_is_valid_fresh_and_not_a_role_pane() {
        let first = restored_pane_id(Some("unit 014"));
        let second = restored_pane_id(Some("unit 014"));
        assert_ne!(first, second);
        for id in [&first, &second] {
            assert!(crate::agent_pty::is_valid_pane_id_env(id), "{id}");
            assert!(id.contains("restored-unit-014-"), "{id}");
            assert!(!crate::spawn::is_orchestration_role_pane_id(id), "{id}");
        }
    }

    #[test]
    fn a_resume_token_is_one_the_shim_accepts() {
        let token = resume_token().expect("OS randomness");
        assert!(crate::telemetry::shim::is_resume_token(&token), "{token}");
        assert_ne!(resume_token(), Some(token));
    }

    #[test]
    fn restore_needs_a_resume_verdict_and_start_over_takes_either() {
        let start_over = RestoreVerdict::StartOver {
            reason: crate::interrupted_work::RestoreReason::NotResumed,
        };
        let none = RestoreVerdict::NotRestorable {
            reason: crate::interrupted_work::RestoreReason::OrchestrationLater,
        };
        assert!(offers(RestoreVerdict::Resume, Want::Resume));
        assert!(!offers(start_over, Want::Resume));
        assert!(offers(start_over, Want::StartOver));
        assert!(offers(RestoreVerdict::Resume, Want::StartOver));
        assert!(!offers(none, Want::StartOver));
        assert_eq!(
            program_of(&crate::event::AgentType::ClaudeCode),
            Some("claude")
        );
    }

    #[test]
    fn one_attempt_is_held_against_a_second_restore_until_released() {
        let attempt = AttemptId::parse("r-held-test/7").expect("an attempt id");
        let first = InFlight::take(&attempt).expect("free");
        assert!(InFlight::take(&attempt).is_none());
        drop(first);
        assert!(InFlight::take(&attempt).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_pid_or_an_empty_session_has_no_members_left() {
        // This process is alive, so its pid is not scanned.
        assert!(!old_session_has_members(std::process::id()));
        // A pid no process holds and no session is named after.
        assert!(!old_session_has_members(i32::MAX as u32));
    }
}
