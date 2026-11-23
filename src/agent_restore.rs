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
//! 6. the old process is gone, on evidence: the kernel says no process holds
//!    the record's pid, or the one holding it started at another time
//!    ([`crate::launch_records::process_gone`]); and no process is left in
//!    the old agent's session ([`old_session_left`], Linux and macOS). A
//!    record without the pid fields (written by a build before stage 3), a
//!    process the OS would not let the deck look at, or a session it could
//!    not list, refuses a restore (`old-process-unknown`) but not a start
//!    over;
//! 7. no other agent the deck runs is in the same conversation, nor being
//!    restored into it (`session-in-use`, [`Sessions::in_use`],
//!    [`InFlight`]);
//! 8. for a start over of a dispatched unit, the saved task is read through
//!    the held folder without following a symlink, a regular owner-only file
//!    of bounded size ([`crate::dispatch_task::read_saved_in`]);
//! 9. the request came from the person: one carrying a pane claim
//!    ([`CallerClaim`]) is refused, as reclaim's is (the same cooperative
//!    guard, not an authentication boundary).
//!
//! Restores and start-overs of one attempt, and restores into one
//! conversation, are serialized ([`InFlight`]): a second request while one
//! runs is refused, so two clients cannot start two writers into one
//! conversation. At most [`MAX_PENDING_RESTORES`] run at once.
//!
//! A dry run also reads the item's Git facts with the list's bounded probes
//! ([`crate::interrupted_work_daemon::item_git_facts`]), so a client can name
//! an open pull request on the item's branch in its question. It refuses
//! nothing: a resume continues the work that pull request is about.
//!
//! **Restore.** The agent is started as a dashboard pane of its own (a fresh
//! pane work, `launch: pane`), with the recorded command, `SHELL` override,
//! agent type and name, a one-shot resume request in its environment
//! ([`crate::telemetry::shim::RESUME_ENV_KEYS`]: a fresh token bound to the
//! confirmation's lifetime, [`crate::telemetry::shim::mint_token`]) and the
//! installed shim directory first on that one spawn's `PATH`
//! ([`restore_path`]). It is confirmed only when its first attested
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
//! between, as for reclaim. The session scan sees processes still in the old
//! agent's session; one that left it (`setsid`) is not found. A launch the
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

/// How long past the confirmation's bound a restore's resume token stays
/// usable: room for the agent's launch script to reach the shim.
const TOKEN_SLACK_SECS: u64 = 60;

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

/// The most restores and start-overs running at once (audit S3).
pub const MAX_PENDING_RESTORES: usize = 16;

/// What the running restores and start-overs hold: each one's attempt, and
/// for a restore the conversation it resumes into.
#[derive(Default)]
struct Held {
    attempts: HashSet<AttemptId>,
    sessions: HashSet<String>,
}

static IN_FLIGHT: LazyLock<Mutex<Held>> = LazyLock::new(|| Mutex::new(Held::default()));

/// One attempt, and the conversation a restore of it resumes into, held
/// against a second restore or start-over until dropped: of the same attempt
/// (`already-restoring`), or of any attempt into the same conversation
/// (`session-in-use`, audit "later slices" 3); and at most
/// [`MAX_PENDING_RESTORES`] at once (`too-many-pending`).
struct InFlight {
    attempt: AttemptId,
    session: Option<String>,
}

impl InFlight {
    fn take(attempt: &AttemptId, session: Option<&str>) -> Result<Self, RestoreRefusal> {
        let mut held = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if held.attempts.contains(attempt) {
            return Err(RestoreRefusal::AlreadyRestoring);
        }
        if session.is_some_and(|session| held.sessions.contains(session)) {
            return Err(RestoreRefusal::SessionInUse);
        }
        if held.attempts.len() >= MAX_PENDING_RESTORES {
            return Err(RestoreRefusal::TooManyPending);
        }
        held.attempts.insert(attempt.clone());
        if let Some(session) = session {
            held.sessions.insert(session.to_string());
        }
        Ok(InFlight {
            attempt: attempt.clone(),
            session: session.map(str::to_string),
        })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut held = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        held.attempts.remove(&self.attempt);
        if let Some(session) = &self.session {
            held.sessions.remove(session);
        }
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
    /// A dispatched unit's start over: its saved task is read from the held
    /// folder and posted first ([`start`]).
    wants_task: bool,
}

/// What the journal checks found for a request.
struct Prepared {
    journal: WorkJournal,
    /// The item, as this request's own read found it.
    item: InterruptedItem,
    /// The item's works, root first, which a success closes.
    works: Vec<WorkId>,
    targets: Vec<Result<Target, RestoreRefused>>,
}

/// What a request's read says of every attempt's conversation: each
/// attempt's latest session, and which attempts ended.
struct Sessions<'a> {
    latest: std::collections::HashMap<&'a AttemptId, crate::work_journal::SessionReport<'a>>,
    ended: HashSet<&'a AttemptId>,
}

impl<'a> Sessions<'a> {
    fn of(read: &'a crate::work_journal::JournalRead) -> Self {
        let ended = read
            .lines
            .iter()
            .filter_map(|line| match &line.event {
                Event::AttemptEnded { attempt, .. } => Some(attempt),
                _ => None,
            })
            .collect();
        Sessions {
            latest: crate::work_journal::latest_sessions(&read.lines),
            ended,
        }
    }

    /// Whether an attempt other than `attempt`, not ended and of a run that
    /// is live, is in `session` now: an agent the deck runs (a copy restored
    /// before, say) that a resume would write beside.
    fn in_use(&self, attempt: &AttemptId, session: &str, live: &dyn Fn(&str) -> bool) -> bool {
        self.latest.iter().any(|(other, report)| {
            *other != attempt
                && report.session == session
                && !self.ended.contains(other)
                && live(other.run())
        })
    }
}

/// The journal checks of a request: 1–4, 6 and 7 of the module docs (8 and 5 in [`start`]).
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
    let sessions = Sessions::of(&snap.read);
    let live = |run: &str| journal.run_is_live(run);
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
            check_agent(&sessions, &live, &launches, &works, &item, agent, want)
                .map_err(|reason| RestoreRefused::new(Some(agent.attempt.clone()), reason))
        })
        .collect();
    drop(sessions);
    Ok(Prepared {
        journal,
        item,
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
    sessions: &Sessions<'_>,
    live: &dyn Fn(&str) -> bool,
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
            let Some(report) = sessions.latest.get(&agent.attempt) else {
                return Err(RestoreRefusal::NoSessionId);
            };
            let session = report.session;
            let agent_type = record
                .agent_type
                .clone()
                .unwrap_or_else(|| report.agent.clone());
            let Some(resume) = crate::agent_registry::spec(&agent_type).resume else {
                return Err(RestoreRefusal::NotRestorable);
            };
            if !resume.valid_session_id(session) {
                return Err(RestoreRefusal::SessionIdInvalid);
            }
            if sessions.in_use(&agent.attempt, session, live) {
                return Err(RestoreRefusal::SessionInUse);
            }
            (Some(session.to_string()), Some(resume.program))
        }
    };
    let gone = crate::launch_records::process_gone(&record).and_then(|()| match record.pid {
        Some(pid) => old_session_left(pid),
        None => Err(ProcessNotGone::OldProcessUnknown),
    });
    match gone {
        Ok(()) => {}
        Err(ProcessNotGone::StillRunning) => return Err(RestoreRefusal::StillRunning),
        // A start over writes to no old conversation; a resume would.
        Err(ProcessNotGone::OldProcessUnknown) if want == Want::Resume => {
            return Err(RestoreRefusal::OldProcessUnknown);
        }
        Err(ProcessNotGone::OldProcessUnknown) => {}
    }
    Ok(Target {
        attempt: agent.attempt.clone(),
        record,
        session,
        program,
        wants_task: want == Want::StartOver
            && matches!(item.kind, ItemKind::Unit | ItemKind::IssueDispatch),
    })
}

/// Whether the session the old agent `pid` led is empty: a child it left
/// behind there could still write to its conversation or worktree (audit
/// "later slices" 2). Asked once [`crate::launch_records::process_gone`]
/// found `pid` itself gone. `Ok` when no process is in that session;
/// `still-running` when one is; `old-process-unknown` when the OS would not
/// say ([`crate::platform::proc::session_has_members`]: Linux and macOS
/// list their processes; elsewhere nothing can be said).
///
/// A `pid` now held by another process (it started at another time): on
/// Linux the kernel hands out no pid that is still a session's id, so the
/// old session emptied before; elsewhere the scan still runs, and a new
/// process that leads a session of that id refuses (the safe side).
fn old_session_left(pid: u32) -> Result<(), ProcessNotGone> {
    use crate::platform::proc::{ProcessProbe, probe_process, session_has_members};
    match probe_process(pid) {
        ProcessProbe::Started(_) if cfg!(target_os = "linux") => return Ok(()),
        ProcessProbe::Started(_) | ProcessProbe::Absent => {}
        ProcessProbe::Unknown => return Err(ProcessNotGone::OldProcessUnknown),
    }
    match session_has_members(pid) {
        Some(false) => Ok(()),
        Some(true) => Err(ProcessNotGone::StillRunning),
        None => Err(ProcessNotGone::OldProcessUnknown),
    }
}

/// The `PATH` a restore spawn gets: the shim directory `shim` first, then
/// `current` (the daemon's own) without any entry that is `shim` already.
/// An absent or empty `current` gives `shim` alone, never `<shim>:`, whose
/// trailing empty entry would make the current directory searched (audit
/// "later slices" 5).
fn restore_path(shim: &str, current: Option<&std::ffi::OsStr>) -> Option<String> {
    let current = current.unwrap_or_default();
    if current.is_empty() {
        return Some(shim.to_string());
    }
    let current = current.to_str()?;
    let rest: Vec<&str> = current.split(':').filter(|entry| *entry != shim).collect();
    if rest.is_empty() {
        return Some(shim.to_string());
    }
    Some(format!("{shim}:{}", rest.join(":")))
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
    // Any claim, well formed or not: see the module docs' item 9.
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
        item,
        works,
        targets,
    } = prepared;
    // §4 item 10: a dry run says what Git and GitHub say of the item, an
    // open pull request above all, so the question can show it. It refuses
    // nothing: a resume continues the work that pull request is about.
    if dry_run {
        outcome.git = Some(crate::interrupted_work_daemon::item_git_facts(&item).await);
    }
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
        item,
        works,
        mut targets,
    } = match prepared(daemon.registry, id, vec![attempt], Want::StartOver).await {
        Ok(prepared) => prepared,
        Err(refusal) => return refused(refusal),
    };
    let git = if dry_run {
        Some(crate::interrupted_work_daemon::item_git_facts(&item).await)
    } else {
        None
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
            git,
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
/// folder is checked here, last before the spawn, and a dispatched unit's
/// saved task is read through the held folder.
#[cfg(not(unix))]
async fn start(
    _daemon: &Daemon<'_>,
    _journal: &WorkJournal,
    _works: &[WorkId],
    target: Target,
    _mode: Option<RestoreMode>,
    _dry_run: bool,
) -> Started {
    Started::Refused(RestoreRefused::new(
        Some(target.attempt),
        RestoreRefusal::Unsupported,
    ))
}

/// Start `target`: resumed when `mode` is set, started over otherwise. The
/// folder is checked here, last before the spawn, and a dispatched unit's
/// saved task is read through the held folder.
#[cfg(unix)]
async fn start(
    daemon: &Daemon<'_>,
    journal: &WorkJournal,
    works: &[WorkId],
    target: Target,
    mode: Option<RestoreMode>,
    dry_run: bool,
) -> Started {
    use std::os::fd::AsFd as _;
    let attempt = target.attempt.clone();
    let refuse = |reason| Started::Refused(RestoreRefused::new(Some(attempt.clone()), reason));
    let _held = match InFlight::take(&target.attempt, target.session.as_deref()) {
        Ok(held) => held,
        Err(reason) => return refuse(reason),
    };
    let cwd = target.record.cwd.clone().unwrap_or_default();
    let work = target.record.work.clone();
    let wants_task = target.wants_task;
    let folder = tokio::task::spawn_blocking(move || {
        let dir = verified_folder(&cwd)?;
        let task = if wants_task {
            match crate::dispatch_task::read_saved_in(dir.as_fd(), &work) {
                Ok(task) => Some(task),
                Err(crate::dispatch_task::SavedTaskError::NotSaved) => {
                    return Err(RestoreRefusal::NoSavedTask);
                }
                Err(crate::dispatch_task::SavedTaskError::Unsafe) => {
                    return Err(RestoreRefusal::TaskUnsafe);
                }
            }
        } else {
            None
        };
        Ok((dir, task))
    })
    .await;
    let (dir, task) = match folder {
        Ok(Ok(found)) => found,
        Ok(Err(reason)) => return refuse(reason),
        Err(_) => return refuse(RestoreRefusal::FolderUnsafe),
    };
    if let Some(task) = task.as_deref()
        && crate::spawn::admit_seed(daemon.registry, SAVED_TASK_REASON, task).is_err()
    {
        return refuse(RestoreRefusal::TaskUnsafe);
    }
    // The shim this restore resumes through, as this daemon installed it,
    // and the directory it acknowledges in, held.
    let shim = match target.program {
        Some(program) => {
            let Some(shim_dir) = crate::telemetry::shim::installed_dir() else {
                return refuse(RestoreRefusal::Unsupported);
            };
            if !shim_dir.join(program).is_file() || shim_dir.to_str().is_none() {
                return refuse(RestoreRefusal::Unsupported);
            }
            let Ok(acks) = crate::telemetry::shim::open_acks(&shim_dir, true) else {
                return refuse(RestoreRefusal::Unsupported);
            };
            // Audit S3: retire what expired, then refuse rather than add to
            // a directory at its bound.
            let left = crate::telemetry::shim::retire_expired(&acks, now_secs());
            if left >= crate::telemetry::shim::MAX_ACK_ENTRIES {
                return refuse(RestoreRefusal::TooManyPending);
            }
            Some((shim_dir, acks))
        }
        None => None,
    };
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
    if let (Some(session), Some(program), Some((shim_dir, _))) =
        (&target.session, target.program, &shim)
    {
        // Bound to this attempt's confirmation: a shim reached after it ends
        // claims nothing (audit "later slices" 6, S3).
        let lifetime = confirm_timeout().as_secs().saturating_add(TOKEN_SLACK_SECS);
        let Some(minted) = crate::telemetry::shim::mint_token(now_secs(), lifetime) else {
            return refuse(RestoreRefusal::SpawnFailed);
        };
        let Some(path) = shim_dir
            .to_str()
            .and_then(|shim| restore_path(shim, std::env::var_os("PATH").as_deref()))
        else {
            return refuse(RestoreRefusal::Unsupported);
        };
        env.push(("PATH".to_string(), path));
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
    let spawned = daemon.registry.spawn_agent_in(opts, &dir);
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
    // Every attached client is told of the new dashboard pane, titled with
    // its name, as an attach start announces its pane (PRD #1223): a TUI that
    // asked the restore question otherwise drew the agent only from its own
    // hooks, under its agent type rather than its name. A start that is then
    // not confirmed is closed through `close_started_agents`, which announces
    // the removal.
    if let Some(started_record) = daemon.registry.agent_record_any(&agent_id) {
        crate::spawn::surface_attach_started_agent(
            daemon.event_tx,
            &started_record,
            record.command.as_deref(),
            Some(daemon.registry.inbox()),
        );
    }

    let (how, close, first) = match (&target.session, mode) {
        (Some(session), Some(mode)) => {
            let confirmed = confirm(
                daemon.registry,
                &pane_id,
                &agent_id,
                session,
                shim.as_ref().map(|(_, acks)| acks).zip(token.as_deref()),
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
            task.map(|task| (SAVED_TASK_REASON, task)),
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
/// early on a refused acknowledgement from the shim (`ack`: the held
/// directory and the request's token, read with
/// [`crate::telemetry::shim::ack_state`]) and on the agent exiting. An
/// acknowledgement only says the shim gave the arguments; the session the
/// agent attests is what confirms.
#[cfg(unix)]
async fn confirm(
    registry: &AgentPtyRegistry,
    pane: &str,
    agent_id: &str,
    session: &str,
    ack: Option<(&crate::platform::trusted_dir::TrustedDir, &str)>,
    bound: Duration,
) -> Result<(), RestoreFailure> {
    use crate::telemetry::shim::{AckState, ack_state};
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
        if let Some((acks, token)) = ack
            && ack_state(acks, token) == AckState::Refused
        {
            return Err(RestoreFailure::Refused);
        }
        if registry.agent_record_any(agent_id).is_none() || registry.pane_occupant_has_exited(pane)
        {
            return Err(RestoreFailure::Exited);
        }
        if Instant::now() >= deadline {
            let applied =
                ack.is_some_and(|(acks, token)| ack_state(acks, token) == AckState::Resumed);
            return Err(if applied {
                RestoreFailure::TimedOut
            } else {
                RestoreFailure::ShimNotReached
            });
        }
        tokio::time::sleep(CONFIRM_POLL).await;
    }
}

/// Seconds since the Unix epoch, now.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The agent type the shim resumes as, for a test.
#[cfg(test)]
fn program_of(agent: &crate::event::AgentType) -> Option<&'static str> {
    crate::agent_registry::spec(agent).resume.map(|r| r.program)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRD #1790 stage 3 (audit "later slices" 5): the shim leads the
    /// restore's `PATH` once, and an absent or empty `PATH` never becomes
    /// `<shim>:`, which would search the current directory.
    #[test]
    fn the_restore_path_leads_with_the_shim_once_and_adds_no_empty_entry() {
        use std::ffi::OsStr;
        let shim = "/state/telemetry/bin";
        assert_eq!(restore_path(shim, None).as_deref(), Some(shim));
        assert_eq!(
            restore_path(shim, Some(OsStr::new(""))).as_deref(),
            Some(shim)
        );
        assert_eq!(
            restore_path(shim, Some(OsStr::new("/usr/bin:/bin"))).as_deref(),
            Some("/state/telemetry/bin:/usr/bin:/bin")
        );
        assert_eq!(
            restore_path(shim, Some(OsStr::new("/state/telemetry/bin:/usr/bin"))).as_deref(),
            Some("/state/telemetry/bin:/usr/bin"),
            "deduplicated"
        );
        assert_eq!(
            restore_path(shim, Some(OsStr::new("/state/telemetry/bin"))).as_deref(),
            Some(shim)
        );
        assert_eq!(
            restore_path(shim, Some(OsStr::new("/usr/bin::/bin"))).as_deref(),
            Some("/state/telemetry/bin:/usr/bin::/bin"),
            "an empty entry the daemon's own PATH had is kept as it was"
        );
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
        let first = InFlight::take(&attempt, None).expect("free");
        assert_eq!(
            InFlight::take(&attempt, None).err(),
            Some(RestoreRefusal::AlreadyRestoring)
        );
        drop(first);
        assert!(InFlight::take(&attempt, None).is_ok());
    }

    /// PRD #1790 stage 3 (audit "later slices" 3): two different attempts
    /// resuming into one conversation are not restored at once.
    #[test]
    fn two_attempts_into_one_conversation_are_not_restored_at_once() {
        const SESSION: &str = "fedcba98-7654-4321-8fab-0123456789ab";
        let one = AttemptId::parse("r-session-test/1").expect("an attempt id");
        let two = AttemptId::parse("r-session-test/2").expect("an attempt id");
        let first = InFlight::take(&one, Some(SESSION)).expect("free");
        assert_eq!(
            InFlight::take(&two, Some(SESSION)).err(),
            Some(RestoreRefusal::SessionInUse)
        );
        drop(first);
        assert!(InFlight::take(&two, Some(SESSION)).is_ok());
    }

    /// PRD #1790 stage 3 (audit "later slices" 3): an attempt the deck runs
    /// now in the same conversation makes it in use; one that ended, or whose
    /// run is gone, does not.
    #[test]
    fn a_live_attempt_in_the_same_conversation_makes_it_in_use() {
        use crate::work_journal::{EndHow, JournalRead, Line};
        const SESSION: &str = "fedcba98-7654-4321-8fab-0123456789ab";
        let work = WorkId::parse("w-abcdefgh-0000000000000001").expect("a work id");
        let target = AttemptId::parse("r-old/1").expect("an attempt id");
        let live_copy = AttemptId::parse("r-live/3").expect("an attempt id");
        let ended = AttemptId::parse("r-live/4").expect("an attempt id");
        let line = |run: &str, seq: u64, event: Event| Line {
            v: 1,
            ts_ms: seq,
            run: run.to_string(),
            seq,
            event,
        };
        let session = |attempt: &AttemptId| Event::AttemptSession {
            work: work.clone(),
            attempt: attempt.clone(),
            session: SESSION.to_string(),
            agent: crate::event::AgentType::ClaudeCode,
        };
        let mut read = JournalRead {
            lines: vec![
                line("r-old", 1, session(&target)),
                line("r-live", 2, session(&ended)),
                line(
                    "r-live",
                    3,
                    Event::AttemptEnded {
                        work: work.clone(),
                        attempt: ended.clone(),
                        how: EndHow::Exited,
                        code: None,
                        cause: None,
                    },
                ),
            ],
            ..JournalRead::default()
        };
        let live = |run: &str| run == "r-live";
        assert!(!Sessions::of(&read).in_use(&target, SESSION, &live));
        read.lines.push(line("r-live", 4, session(&live_copy)));
        assert!(Sessions::of(&read).in_use(&target, SESSION, &live));
        assert!(!Sessions::of(&read).in_use(&target, SESSION, &|_: &str| false));
    }

    /// PRD #1790 stage 3 (audit "later slices" 2): a pid no process holds and
    /// no session is named after leaves nothing behind; this process's own
    /// session, led by nobody gone, is not asked about.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_empty_session_has_nothing_left() {
        assert_eq!(old_session_left(i32::MAX as u32), Ok(()));
    }
}
