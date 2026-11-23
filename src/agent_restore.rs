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
//!    ([`crate::launch_records::read_all_with_stats`]: nothing truncated,
//!    nothing skipped);
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
//!    record without the pid fields (written by a build before stage 3) is
//!    listed as Start over (`old-process-unknown`, review R#2); it, a
//!    process the OS would not let the deck look at, or a session it could
//!    not list, refuses a restore but not a start over. The same holds,
//!    strictly, for every agent an earlier failed restore of the attempt
//!    started (`restore_failed`'s `to`, audit F4): a restore or start over
//!    is refused while any process of it may be left;
//! 7. no other agent the deck runs is in the same conversation, nor being
//!    restored into it (`session-in-use`, [`Sessions::in_use`],
//!    [`InFlight`], [`claim`]), and no newer live work runs in the agent's
//!    folder (`directory-in-use`, review R#3);
//! 8. for a start over of a dispatched unit, the saved task is read through
//!    the held folder without following a symlink, a regular owner-only file
//!    of bounded size ([`crate::dispatch_task::read_saved_in`]);
//! 9. the request came from the person: one carrying a pane claim
//!    ([`CallerClaim`]) is refused, as reclaim's is (the same cooperative
//!    guard, not an authentication boundary);
//! 10. Git and GitHub, with the list's bounded probes
//!     ([`crate::interrupted_work_daemon::item_git_facts`], review R#3): a
//!     worktree that is gone refuses either action (`folder-missing`); an
//!     open pull request on the item's branch refuses a start over
//!     (`pr-open`), which would repeat the task beside it, and only adds a
//!     note to a restore's question, since the resumed agent may be the one
//!     maintaining it;
//! 11. for a start over, the recorded command does not reopen an earlier
//!     conversation ([`crate::agent_registry::ResumeSpec::command_reopens`],
//!     `command-reopens`, audit F3).
//!
//! **The request is bounded before anything is read** (audit F5): more
//! agents than an item lists ([`MAX_ITEM_AGENTS`]), an agent named twice, or
//! an id longer than [`MAX_SELECTOR_BYTES`] / [`MAX_ITEM_ID_BYTES`] refuses
//! the whole request (`bad-selection`), and at most [`MAX_PENDING_RESTORES`]
//! requests are prepared or running at once (`too-many-pending`).
//!
//! **One writer per conversation** (audit F1, F2). Before an agent is
//! started, its attempt and the conversation a restore resumes into are
//! claimed, in this daemon ([`InFlight`]) and across every daemon sharing the
//! state directory ([`claim`]: a `flock`ed file per attempt and per
//! conversation in an owner-only directory of it). With the claims held,
//! every check above runs again from a fresh read ([`revalidate`]), and the
//! claims are kept until the agent is confirmed and its successor recorded
//! in the journal, or it is stopped. A second request meanwhile is refused
//! (`already-restoring`, `session-in-use`); one after it finds the item
//! closed, or the new agent live in the conversation.
//!
//! **Restore.** The restart note is admitted to the inbox first
//! (`note-refused`, review R#4). The agent is started as a dashboard pane of
//! its own (a fresh pane work, `launch: pane`), with the recorded command,
//! `SHELL` override, agent type and name, a one-shot resume request in its
//! environment ([`crate::telemetry::shim::RESUME_ENV_KEYS`]: a fresh token
//! bound to the confirmation's lifetime,
//! [`crate::telemetry::shim::mint_token`]) and the installed shim directory
//! first on that one spawn's `PATH` ([`restore_path`]). It is confirmed only
//! when its first attested hook session
//! ([`AgentPtyRegistry::watch_restore_session`]) is the requested one, within
//! [`CONFIRM_TIMEOUT`]. A different session, a refused acknowledgement from
//! the shim, the agent exiting, or no confirmation in time stops the new
//! agent before anything is posted to it ([`stop_proven`]), writes
//! `restore_failed` for the attempt (so the list offers Start over from then
//! on) and answers `not-resumed`, or `not-stopped` when the stop could not be
//! confirmed. A confirmed restore posts the restart note as the agent's first
//! inbox message (reason `restart note`), then writes `closed how=restored`
//! on the item's works and `attempt_restored{how: resumed}`. A note that
//! cannot be posted after all stops the agent again and leaves the item
//! listed as it was (`note-not-delivered`).
//!
//! **Start over.** The recorded command, in the verified folder, with no
//! resume request. For an agent the deck can resume, whose hooks report its
//! conversation before its prompt (Claude Code), the first conversation it
//! reports must not be the old one, or it is stopped (`not-fresh`, audit F3);
//! an agent with no such report (the documented gaps) is started without
//! that check. A dispatched unit's saved task is posted as its first inbox
//! message (reason `saved task`), and no restart note: the agent is new and
//! has no conversation to be told about. Then `closed how=started_over` and
//! `attempt_restored{how: started_over}`.
//!
//! **Stopping an agent it started** ([`stop_proven`], audit F4) closes it as
//! `StopAgent` would, then kills every process left in its POSIX session (a
//! child that moved to another process group, or ignores `SIGTERM`) and
//! answers stopped only once the session is empty.
//!
//! **What this does not guard against.** Another process can change the
//! folder between the last check and the spawn, as for reclaim. The session
//! scans see processes still in a session; one that left it (`setsid`) is
//! not found. A launch the shim cannot reach (the agent called by an
//! absolute path, `PATH` replaced, a wrapper that drops arguments) is never
//! resumed: it is caught by the confirmation and answered `not-resumed`. A
//! start over of an agent that reports no conversation before its prompt is
//! not checked for freshness beyond its recorded command.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use crate::daemon_protocol::CallerClaim;
use crate::event::{AgentType, BroadcastMsg};
use crate::interrupted_work::{
    CloseSets, GitFacts, InterruptedItem, ItemKind, MAX_ITEM_AGENTS, PrFact, Resolved, RestoreMode,
    RestoreOutcome, RestoreReason, RestoreRefusal, RestoreRefused, RestoreVerdict, RestoredAgent,
    StartOverOutcome, resolve, still_interrupted,
};
use crate::launch_records::{LaunchRecord, ProcessNotGone};
use crate::work_journal::{
    AttemptId, CloseHow, Event, RestoreFailure, RestoredHow, ShimOutcome, WorkId, WorkJournal,
};

/// How long a restore waits for the restored agent to confirm its
/// conversation.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the confirmation looks.
const CONFIRM_POLL: Duration = Duration::from_millis(50);

/// How long [`stop_proven`] keeps killing what is left in a stopped agent's
/// session before it answers that it could not confirm the stop.
const STOP_PROOF_BOUND: Duration = Duration::from_secs(5);

/// How long past the confirmation's bound a restore's resume token stays
/// usable: room for the agent's launch script to reach the shim.
const TOKEN_SLACK_SECS: u64 = 60;

/// The inbox reason the restart note is posted under.
pub const RESTART_NOTE_REASON: &str = "restart note";

/// The inbox reason a start-over's saved task is posted under.
pub const SAVED_TASK_REASON: &str = "saved task";

/// Audit F5: the longest attempt id a request may name. The deck's own are
/// a run id, `/` and an agent number, far shorter.
pub const MAX_SELECTOR_BYTES: usize = 128;

/// Audit F5: the longest item id or name a request may give, as a
/// dismissal's selector is bounded.
pub const MAX_ITEM_ID_BYTES: usize = 512;

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

/// Whether the restart note's post fails on purpose: in an e2e build only,
/// when `DOT_AGENT_DECK_TEST_RESTORE_REFUSE_NOTE` is `1`, so lane 1 can reach
/// the path a full inbox takes after the agent confirmed (review R#4).
fn note_post_refused_for_test() -> bool {
    #[cfg(feature = "e2e")]
    if std::env::var("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_NOTE").as_deref() == Ok("1") {
        return true;
    }
    false
}

/// In an e2e build only: a start over waits here, after its request was
/// read and checked and before anything of [`start`], while the file
/// `DOT_AGENT_DECK_TEST_RESTORE_BARRIER` names exists (at most two minutes),
/// having first created that path with the extension `reached`. Lane 1's
/// `journal/restore/020` restores the agent meanwhile, to show a request
/// prepared before another acted reads everything again (audit F1).
#[cfg(feature = "e2e")]
async fn test_barrier(want: Want) {
    if want != Want::StartOver {
        return;
    }
    let Some(path) = std::env::var_os("DOT_AGENT_DECK_TEST_RESTORE_BARRIER") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let _ = std::fs::write(path.with_extension("reached"), b"");
    let deadline = Instant::now() + Duration::from_secs(120);
    while path.exists() && Instant::now() < deadline {
        tokio::time::sleep(CONFIRM_POLL).await;
    }
}

/// The most restores and start-overs running at once (audit S3), and the
/// most requests prepared at once (audit F5).
pub const MAX_PENDING_RESTORES: usize = 16;

/// Audit F5: the requests being prepared or run. A request takes a permit
/// before its first read and holds it to its answer; with none left it is
/// refused `too-many-pending` before anything is read.
static REQUESTS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(MAX_PENDING_RESTORES));

/// What the running restores and start-overs hold: each one's attempt, and
/// for a restore the conversation it resumes into.
#[derive(Default)]
struct Held {
    attempts: HashSet<AttemptId>,
    sessions: HashSet<String>,
}

static IN_FLIGHT: LazyLock<Mutex<Held>> = LazyLock::new(|| Mutex::new(Held::default()));

/// One attempt, and the conversation a restore of it resumes into, held
/// against a second restore or start-over in this daemon until dropped: of
/// the same attempt (`already-restoring`), or of any attempt into the same
/// conversation (`session-in-use`, audit "later slices" 3); and at most
/// [`MAX_PENDING_RESTORES`] at once (`too-many-pending`). [`claim`] holds the
/// same against another daemon on the state directory.
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

/// Audit F2: the claims of one restore or start over across every daemon
/// that shares the state directory. An owner-only directory in it,
/// [`CLAIMS_DIR`], holds one file per claimed attempt and one per claimed
/// conversation; a claim is an exclusive `flock` on that file, taken without
/// waiting, so a second daemon's claim of either is refused. The lock goes
/// with the descriptor, so a daemon that dies releases what it held.
#[cfg(unix)]
pub(crate) mod claim {
    use std::path::Path;

    use crate::interrupted_work::RestoreRefusal;
    use crate::platform::trusted_dir::TrustedDir;
    use crate::work_journal::AttemptId;

    /// The claims directory, under the state directory.
    pub(crate) const CLAIMS_DIR: &str = "restore-claims";

    /// One claimed name. Dropping it removes the name while the lock is
    /// still held, then closes the file, which releases the lock: a claimer
    /// that opened the old file before the removal finds, once it holds the
    /// lock, that the name no longer names it, and is refused.
    struct One {
        dir: TrustedDir,
        name: String,
        _file: std::fs::File,
    }

    impl Drop for One {
        fn drop(&mut self) {
            let _ = self.dir.remove_file(&self.name);
        }
    }

    /// An attempt's claim, and its conversation's for a restore, held until
    /// dropped.
    pub(crate) struct Claims {
        _held: Vec<One>,
    }

    fn hex(text: &str) -> String {
        text.bytes().map(|b| format!("{b:02x}")).collect()
    }

    /// The file an attempt's claim locks.
    pub(crate) fn attempt_name(attempt: &AttemptId) -> String {
        format!("attempt-{}.lock", hex(attempt.as_str()))
    }

    /// The file a conversation's claim locks.
    pub(crate) fn session_name(session: &str) -> String {
        format!("session-{}.lock", hex(session))
    }

    /// Lock `name` in `dir` without waiting: `Ok(None)` when another
    /// descriptor holds it, or the name was removed or replaced after it was
    /// opened (a claim being released then).
    fn lock(dir: &TrustedDir, name: &str) -> std::io::Result<Option<One>> {
        use std::os::unix::fs::MetadataExt as _;
        let file = match dir.create_new_owner_only(name) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match dir.open_regular(name) {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        if !crate::platform::lock::try_lock_file(&file)? {
            return Ok(None);
        }
        let meta = file.metadata()?;
        match dir.identity(name) {
            Ok(named) if named == (meta.dev(), meta.ino()) => {}
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
        Ok(Some(One {
            dir: dir.try_clone()?,
            name: name.to_string(),
            _file: file,
        }))
    }

    /// Claim `attempt`, then `session` when given, in the state directory
    /// `state_dir`. Refused `already-restoring` or `session-in-use` when
    /// another daemon holds either; `spawn-failed` when the directory cannot
    /// be opened or a lock cannot be taken for another reason.
    pub(crate) fn take(
        state_dir: &Path,
        attempt: &AttemptId,
        session: Option<&str>,
    ) -> Result<Claims, RestoreRefusal> {
        let failed = |error: std::io::Error| {
            tracing::warn!(%error, "restore: could not take a claim in the state directory");
            RestoreRefusal::SpawnFailed
        };
        let dir = TrustedDir::anchor(state_dir)
            .and_then(|anchor| anchor.child(CLAIMS_DIR, true))
            .map_err(failed)?;
        let mut held = Vec::new();
        match lock(&dir, &attempt_name(attempt)).map_err(failed)? {
            Some(one) => held.push(one),
            None => return Err(RestoreRefusal::AlreadyRestoring),
        }
        if let Some(session) = session {
            match lock(&dir, &session_name(session)).map_err(failed)? {
                Some(one) => held.push(one),
                None => return Err(RestoreRefusal::SessionInUse),
            }
        }
        Ok(Claims { _held: held })
    }
}

/// What was asked of an agent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want {
    Resume,
    StartOver,
}

/// One agent the checks passed, ready to start.
struct Target {
    attempt: AttemptId,
    record: LaunchRecord,
    /// The agent the record runs, as the checks read it.
    agent_type: Option<AgentType>,
    /// The conversation to resume into (a restore).
    session: Option<String>,
    /// The conversation the old agent was last in, for a start over to be
    /// told apart from (audit F3); `None` when the deck holds none.
    old_session: Option<String>,
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
    latest: HashMap<&'a AttemptId, crate::work_journal::SessionReport<'a>>,
    ended: HashSet<&'a AttemptId>,
    /// Audit F4: for each attempt, the agents its failed restores started.
    successors: HashMap<&'a AttemptId, Vec<&'a AttemptId>>,
}

impl<'a> Sessions<'a> {
    fn of(read: &'a crate::work_journal::JournalRead) -> Self {
        let mut ended = HashSet::new();
        let mut successors: HashMap<&AttemptId, Vec<&AttemptId>> = HashMap::new();
        for line in &read.lines {
            match &line.event {
                Event::AttemptEnded { attempt, .. } => {
                    ended.insert(attempt);
                }
                Event::RestoreFailed {
                    attempt,
                    to: Some(to),
                    ..
                } => successors.entry(attempt).or_default().push(to),
                _ => {}
            }
        }
        Sessions {
            latest: crate::work_journal::latest_sessions(&read.lines),
            ended,
            successors,
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

    /// Review R#3: whether a launch other than `attempt`'s, not ended and of
    /// a live run, started in `cwd`: newer work in the agent's folder that a
    /// restore or start over would run beside.
    fn folder_in_use(
        &self,
        attempt: &AttemptId,
        cwd: &str,
        launches: &[LaunchRecord],
        live: &dyn Fn(&str) -> bool,
    ) -> bool {
        launches.iter().any(|other| {
            other.attempt != *attempt
                && other.cwd.as_deref() == Some(cwd)
                && !self.ended.contains(&other.attempt)
                && live(other.attempt.run())
        })
    }
}

/// Audit F5: the request's selectors, bounded before anything is read.
fn check_selection(id: &str, attempts: &[String]) -> Result<(), RestoreRefusal> {
    if id.len() > MAX_ITEM_ID_BYTES
        || attempts.len() > MAX_ITEM_AGENTS
        || attempts.iter().any(|a| a.len() > MAX_SELECTOR_BYTES)
    {
        return Err(RestoreRefusal::BadSelection);
    }
    let mut seen = HashSet::new();
    if !attempts.iter().all(|a| seen.insert(a.as_str())) {
        return Err(RestoreRefusal::BadSelection);
    }
    Ok(())
}

/// The journal checks of a request: 1–4, 6, 7 and 11 of the module docs (8
/// and 5 in [`start`], 10 in [`restore`] and [`start_over`]). Blocking.
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
    // Audit suggestion: this read is the authoritative one, so a record it
    // skipped refuses, as the snapshot's does.
    let launches = match crate::launch_records::read_all_with_stats(state_dir) {
        Ok(read) if !read.truncated && read.skipped == 0 => read.records,
        _ => return Err(refuse(RestoreRefusal::IncompleteRead)),
    };
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

/// Whether the process a launch `record` names, and every process left in
/// its session, is gone ([`crate::launch_records::process_gone`],
/// [`old_session_left`]).
fn launch_gone(record: &LaunchRecord) -> Result<(), ProcessNotGone> {
    crate::launch_records::process_gone(record).and_then(|()| match record.pid {
        Some(pid) => old_session_left(pid),
        None => Err(ProcessNotGone::OldProcessUnknown),
    })
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
    // Review R#2: a record without the old process's identity is listed as
    // Start over; a restore of it is refused for that reason.
    if want == Want::Resume
        && agent.restore
            == (RestoreVerdict::StartOver {
                reason: RestoreReason::OldProcessUnknown,
            })
    {
        return Err(RestoreRefusal::OldProcessUnknown);
    }
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
    let cwd = match (&record.cwd, &record.cwd_moved_from) {
        (Some(cwd), _) => cwd.clone(),
        (None, Some(_)) => return Err(RestoreRefusal::FolderMoved),
        (None, None) => return Err(RestoreRefusal::FolderUnknown),
    };
    let report = sessions.latest.get(&agent.attempt);
    let agent_type = record
        .agent_type
        .clone()
        .or_else(|| report.map(|report| report.agent.clone()))
        .or_else(|| AgentType::from_command(record.command.as_deref()));
    let (session, program) = match want {
        Want::StartOver => {
            // Audit F3: a command that reopens a conversation is no fresh
            // start.
            let reopens = agent_type
                .as_ref()
                .and_then(|agent| crate::agent_registry::spec(agent).session_args)
                .zip(record.command.as_deref())
                .is_some_and(|(args, command)| args.command_reopens(command));
            if reopens {
                return Err(RestoreRefusal::CommandReopens);
            }
            (None, None)
        }
        Want::Resume => {
            let Some(report) = report else {
                return Err(RestoreRefusal::NoSessionId);
            };
            let session = report.session;
            let Some(resume) = agent_type
                .as_ref()
                .and_then(|agent| crate::agent_registry::spec(agent).resume)
            else {
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
    if sessions.folder_in_use(&agent.attempt, &cwd, launches, live) {
        return Err(RestoreRefusal::DirectoryInUse);
    }
    match launch_gone(&record) {
        Ok(()) => {}
        Err(ProcessNotGone::StillRunning) => return Err(RestoreRefusal::StillRunning),
        // A start over writes to no old conversation; a resume would.
        Err(ProcessNotGone::OldProcessUnknown) if want == Want::Resume => {
            return Err(RestoreRefusal::OldProcessUnknown);
        }
        Err(ProcessNotGone::OldProcessUnknown) => {}
    }
    // Audit F4: an agent an earlier failed restore started must be gone too,
    // whatever is asked: it was started in this folder, and for a restore
    // into this conversation.
    for successor in sessions
        .successors
        .get(&agent.attempt)
        .into_iter()
        .flatten()
    {
        let mut records = launches.iter().filter(|r| r.attempt == **successor);
        let Some(started) = records.next() else {
            return Err(RestoreRefusal::OldProcessUnknown);
        };
        match launch_gone(started) {
            Ok(()) => {}
            Err(ProcessNotGone::StillRunning) => return Err(RestoreRefusal::StillRunning),
            Err(ProcessNotGone::OldProcessUnknown) => {
                return Err(RestoreRefusal::OldProcessUnknown);
            }
        }
    }
    Ok(Target {
        attempt: agent.attempt.clone(),
        record,
        agent_type,
        session,
        old_session: report.map(|report| report.session.to_string()),
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

/// Audit F1: every check of [`prepare`] again for one agent, from a fresh
/// read, with its claims held: the answer a start acts on. A conversation
/// that is no longer the one claimed refuses (`session-in-use`).
async fn revalidate(
    registry: &AgentPtyRegistry,
    item: &WorkId,
    held: &Target,
    want: Want,
) -> Result<(Target, Vec<WorkId>), RestoreRefusal> {
    let refused = |refused: RestoreRefused| refused.reason;
    let Prepared {
        works, mut targets, ..
    } = prepared(
        registry,
        item.to_string(),
        vec![held.attempt.to_string()],
        want,
    )
    .await
    .map_err(refused)?;
    let fresh = match targets.pop() {
        Some(Ok(fresh)) => fresh,
        Some(Err(refusal)) => return Err(refusal.reason),
        None => return Err(RestoreRefusal::NotListed),
    };
    if fresh.session != held.session {
        return Err(RestoreRefusal::SessionInUse);
    }
    Ok((fresh, works))
}

/// Act-time Git facts (review R#3): a refusal of `want` when the worktree is
/// gone, or, for a start over, when the branch has an open pull request.
fn git_refusal(git: &GitFacts, want: Want) -> Option<RestoreRefusal> {
    if git.worktree_exists == Some(false) {
        return Some(RestoreRefusal::FolderMissing);
    }
    (want == Want::StartOver && git.pr == PrFact::Open).then_some(RestoreRefusal::PrOpen)
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
    let whole = |mut outcome: RestoreOutcome, reason| {
        outcome.refused.push(RestoreRefused::new(None, reason));
        outcome
    };
    // Any claim, well formed or not: see the module docs' item 9.
    if caller.is_some() {
        return whole(outcome, RestoreRefusal::AgentCaller);
    }
    if !cfg!(unix) {
        return whole(outcome, RestoreRefusal::Unsupported);
    }
    if let Err(reason) = check_selection(&id, &attempts) {
        return whole(outcome, reason);
    }
    let Ok(_permit) = REQUESTS.try_acquire() else {
        return whole(outcome, RestoreRefusal::TooManyPending);
    };
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
        works: _,
        targets,
    } = prepared;
    outcome.item = Some(item.id.to_string());
    // §4 item 10 and review R#3: what Git and GitHub say of the item. A dry
    // run carries it, an open pull request above all, so the question can
    // show it; it refuses a restore only when the worktree is gone, since a
    // resume continues the work that pull request is about.
    let git = crate::interrupted_work_daemon::item_git_facts(&item).await;
    let git_refused = git_refusal(&git, Want::Resume);
    if dry_run {
        outcome.git = Some(git);
    }
    for target in targets {
        let target = match target {
            Ok(target) => target,
            Err(refused) => {
                outcome.refused.push(refused);
                continue;
            }
        };
        if let Some(reason) = git_refused {
            outcome
                .refused
                .push(RestoreRefused::new(Some(target.attempt), reason));
            continue;
        }
        match start(&daemon, &journal, &item.id, target, Some(mode), dry_run).await {
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
    let refused = |refused: RestoreRefused, item: Option<String>| StartOverOutcome {
        dry_run,
        refused: Some(refused),
        item,
        ..StartOverOutcome::default()
    };
    let whole = |reason| refused(RestoreRefused::new(None, reason), None);
    if caller.is_some() {
        return whole(RestoreRefusal::AgentCaller);
    }
    if !cfg!(unix) {
        return whole(RestoreRefusal::Unsupported);
    }
    if attempt.is_empty() {
        return whole(RestoreRefusal::BadRequest);
    }
    let attempts = vec![attempt];
    if let Err(reason) = check_selection(&id, &attempts) {
        return whole(reason);
    }
    let Ok(_permit) = REQUESTS.try_acquire() else {
        return whole(RestoreRefusal::TooManyPending);
    };
    let Prepared {
        journal,
        item,
        works: _,
        mut targets,
    } = match prepared(daemon.registry, id, attempts, Want::StartOver).await {
        Ok(prepared) => prepared,
        Err(refusal) => return refused(refusal, None),
    };
    let item_id = Some(item.id.to_string());
    let git = crate::interrupted_work_daemon::item_git_facts(&item).await;
    let target = match targets.pop() {
        Some(Ok(target)) => target,
        Some(Err(refusal)) => return refused(refusal, item_id),
        None => {
            return refused(
                RestoreRefused::new(None, RestoreRefusal::BadRequest),
                item_id,
            );
        }
    };
    if let Some(reason) = git_refusal(&git, Want::StartOver) {
        return refused(RestoreRefused::new(Some(target.attempt), reason), item_id);
    }
    match start(&daemon, &journal, &item.id, target, None, dry_run).await {
        Started::Would(_) => StartOverOutcome {
            dry_run,
            would_start: true,
            git: Some(git),
            item: item_id,
            ..StartOverOutcome::default()
        },
        Started::Done(agent) => StartOverOutcome {
            dry_run,
            started: Some(agent),
            item: item_id,
            ..StartOverOutcome::default()
        },
        Started::Refused(refusal) => refused(refusal, item_id),
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
    _item: &WorkId,
    target: Target,
    _mode: Option<RestoreMode>,
    _dry_run: bool,
) -> Started {
    Started::Refused(RestoreRefused::new(
        Some(target.attempt),
        RestoreRefusal::Unsupported,
    ))
}

/// Start `target`, an agent of the item `item`: resumed when `mode` is set,
/// started over otherwise. Its claims are taken first and every check runs
/// again with them held ([`revalidate`]); the folder is checked here, last
/// before the spawn, and a dispatched unit's saved task is read through the
/// held folder.
#[cfg(unix)]
async fn start(
    daemon: &Daemon<'_>,
    journal: &WorkJournal,
    item: &WorkId,
    target: Target,
    mode: Option<RestoreMode>,
    dry_run: bool,
) -> Started {
    use std::os::fd::AsFd as _;
    let want = if mode.is_some() {
        Want::Resume
    } else {
        Want::StartOver
    };
    #[cfg(feature = "e2e")]
    test_barrier(want).await;
    let attempt = target.attempt.clone();
    let refuse = |reason| Started::Refused(RestoreRefused::new(Some(attempt.clone()), reason));
    let _held = match InFlight::take(&target.attempt, target.session.as_deref()) {
        Ok(held) => held,
        Err(reason) => return refuse(reason),
    };
    let Some(state_dir) = journal.state_dir() else {
        return refuse(RestoreRefusal::IncompleteRead);
    };
    let _claims = match claim::take(state_dir, &target.attempt, target.session.as_deref()) {
        Ok(claims) => claims,
        Err(reason) => return refuse(reason),
    };
    let (target, works) = match revalidate(daemon.registry, item, &target, want).await {
        Ok(fresh) => fresh,
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
    // Review R#4: the restart note is admitted before anything is started,
    // so an inbox that cannot take it costs no agent.
    if let Some(mode) = mode
        && crate::spawn::admit_seed(daemon.registry, RESTART_NOTE_REASON, mode.note()).is_err()
    {
        return refuse(RestoreRefusal::NoteRefused);
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
    }
    // Audit F3: a start over of an agent that reports its conversation
    // before its prompt is watched, so a launch that reopens the old one is
    // caught before the saved task goes to it.
    let fresh_check = match (&target.session, &target.old_session) {
        (None, Some(old)) => target
            .agent_type
            .as_ref()
            .map(crate::agent_registry::spec)
            .filter(|spec| {
                spec.resume.is_some()
                    && spec.first_hook == crate::agent_registry::FirstHook::BeforePrompt
            })
            .map(|_| old.clone()),
        _ => None,
    };
    if target.session.is_some() || fresh_check.is_some() {
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
    // Audit F4: the new agent's session, read while it runs, so a stop can
    // be proven against everything it started.
    let sid = daemon
        .registry
        .child_pid(&agent_id)
        .and_then(crate::platform::proc::session_of);
    let new_attempt = agent_id
        .parse::<u64>()
        .ok()
        .and_then(|agent| journal.attempt_id(agent));
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
    // A failed start: stop the agent, proven; record the failure with the
    // agent it started when `failure` is given, or when the stop could not
    // be proven (so a later request checks that agent is gone); refuse.
    let fail = |failure: Option<(RestoreFailure, Option<ShimOutcome>)>, reason| {
        let started = &started;
        let attempt = &target.attempt;
        let work = &record.work;
        let new_attempt = new_attempt.clone();
        async move {
            let stopped = stop_proven(daemon, started, sid).await;
            let failure = match (failure, stopped) {
                (Some(failure), _) => Some(failure),
                (None, false) => Some((RestoreFailure::Unknown, None)),
                (None, true) => None,
            };
            if let Some((failure, shim)) = failure {
                record_blocking(
                    journal,
                    vec![Event::RestoreFailed {
                        work: work.clone(),
                        attempt: attempt.clone(),
                        failure,
                        shim,
                        to: new_attempt,
                    }],
                )
                .await;
            }
            Started::Refused(RestoreRefused::new(
                Some(attempt.clone()),
                if stopped {
                    reason
                } else {
                    RestoreRefusal::NotStopped
                },
            ))
        }
    };

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
            if let Err(Unconfirmed {
                failure,
                shim: shim_outcome,
                seen,
            }) = confirmed
            {
                tracing::info!(
                    attempt = %target.attempt,
                    ?failure,
                    shim = ?shim_outcome,
                    requested = %session,
                    seen = ?seen,
                    "restore: the agent did not come back into its conversation; stopping it"
                );
                return fail(Some((failure, shim_outcome)), RestoreRefusal::NotResumed).await;
            }
            (
                RestoredHow::Resumed,
                CloseHow::Restored,
                Some((RESTART_NOTE_REASON, mode.note().to_string())),
            )
        }
        _ => {
            if let Some(old) = &fresh_check {
                let seen =
                    first_session(daemon.registry, &pane_id, &agent_id, confirm_timeout()).await;
                daemon.registry.unwatch_restore_session(&pane_id);
                let failure = match seen {
                    Ok(seen) if seen != *old => None,
                    Ok(_) => Some(RestoreFailure::SessionMismatch),
                    Err(failure) => Some(failure),
                };
                if let Some(failure) = failure {
                    tracing::info!(
                        attempt = %target.attempt,
                        ?failure,
                        "restore: the started-over agent did not start a new conversation; stopping it"
                    );
                    // Only an unproven stop is recorded: the item stays as
                    // it was, its verdict unchanged.
                    return fail(None, RestoreRefusal::NotFresh).await;
                }
            }
            (
                RestoredHow::StartedOver,
                CloseHow::StartedOver,
                task.map(|task| (SAVED_TASK_REASON, task)),
            )
        }
    };
    let first_message_posted = match &first {
        Some((reason, body)) => {
            !(*reason == RESTART_NOTE_REASON && note_post_refused_for_test())
                && crate::spawn::post_seed(daemon.registry, &pane_id, &agent_id, reason, body)
                    .is_ok()
        }
        None => true,
    };
    // Review R#4: a restored agent that did not get its restart note does not
    // know the deck restarted. It is stopped again, and the item stays listed
    // as it was, for the person to try again.
    if how == RestoredHow::Resumed && !first_message_posted {
        tracing::warn!(
            attempt = %target.attempt,
            "restore: the restart note could not be posted; stopping the restored agent"
        );
        return fail(None, RestoreRefusal::NoteNotDelivered).await;
    }
    let mut events: Vec<Event> = works
        .iter()
        .map(|work| Event::Closed {
            work: work.clone(),
            how: close,
        })
        .collect();
    if let Some(to) = new_attempt {
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

/// Audit F4: stop an agent a restore or start over just started, as
/// `StopAgent` would ([`crate::spawn::close_started_agents`]), then kill every
/// process left in its session `sid` ([`sweep_session`]). `true` only when
/// that session is then empty; `false` when it could not be read, or a
/// process stayed in it past [`STOP_PROOF_BOUND`].
#[cfg(unix)]
async fn stop_proven(
    daemon: &Daemon<'_>,
    started: &crate::spawn::SpawnedAgent,
    sid: Option<u32>,
) -> bool {
    crate::spawn::close_started_agents(
        daemon.registry,
        Some(daemon.state),
        Some(daemon.event_tx),
        std::slice::from_ref(started),
    )
    .await;
    let Some(sid) = sid else {
        return false;
    };
    tokio::task::spawn_blocking(move || sweep_session(sid, STOP_PROOF_BOUND))
        .await
        .unwrap_or(false)
}

/// Kill every process in the session `sid` with `SIGKILL`, until it is empty
/// or `bound` passes; `true` when it emptied. A process is signalled only
/// while it is still in that session; the daemon's own session is never
/// swept.
#[cfg(unix)]
fn sweep_session(sid: u32, bound: Duration) -> bool {
    use crate::platform::proc::{session_members, session_of};
    if session_of(std::process::id()) == Some(sid) {
        return false;
    }
    let deadline = Instant::now() + bound;
    loop {
        match session_members(sid) {
            Some(members) if members.is_empty() => return true,
            Some(members) => {
                for pid in members {
                    let Ok(target) = libc::pid_t::try_from(pid) else {
                        continue;
                    };
                    if target > 0 && session_of(pid) == Some(sid) {
                        // SAFETY: a positive pid, read in the stopped agent's
                        // session just now; SIGKILL touches nothing else.
                        unsafe {
                            libc::kill(target, libc::SIGKILL);
                        }
                    }
                }
            }
            None => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CONFIRM_POLL);
    }
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

/// Why [`confirm`] gave up: the failure the journal records, what the shim
/// had done with the request then ([`crate::telemetry::shim::ack_outcome`],
/// `None` without a request), and for a different conversation the one the
/// agent reported.
#[cfg(unix)]
#[derive(Debug)]
struct Unconfirmed {
    failure: RestoreFailure,
    shim: Option<ShimOutcome>,
    seen: Option<String>,
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
) -> Result<(), Unconfirmed> {
    use crate::telemetry::shim::{AckState, ack_outcome, ack_state};
    let deadline = Instant::now() + bound;
    let give_up = |failure, seen| Unconfirmed {
        failure,
        shim: ack.map(|(acks, token)| ack_outcome(acks, token)),
        seen,
    };
    loop {
        if let Some((generation, seen)) = registry.restore_session_seen(pane)
            && generation == agent_id
        {
            return if seen == session {
                Ok(())
            } else {
                Err(give_up(RestoreFailure::SessionMismatch, Some(seen)))
            };
        }
        if let Some((acks, token)) = ack
            && ack_state(acks, token) == AckState::Refused
        {
            return Err(give_up(RestoreFailure::Refused, None));
        }
        if registry.agent_record_any(agent_id).is_none() || registry.pane_occupant_has_exited(pane)
        {
            return Err(give_up(RestoreFailure::Exited, None));
        }
        if Instant::now() >= deadline {
            let applied =
                ack.is_some_and(|(acks, token)| ack_state(acks, token) == AckState::Resumed);
            return Err(give_up(
                if applied {
                    RestoreFailure::TimedOut
                } else {
                    RestoreFailure::ShimNotReached
                },
                None,
            ));
        }
        tokio::time::sleep(CONFIRM_POLL).await;
    }
}

/// Audit F3: wait at most `bound` for the agent `agent_id` in `pane` to
/// report its first attested session, and answer it; `exited` when the
/// agent exits first, `timed-out` when nothing came in time.
#[cfg(unix)]
async fn first_session(
    registry: &AgentPtyRegistry,
    pane: &str,
    agent_id: &str,
    bound: Duration,
) -> Result<String, RestoreFailure> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some((generation, seen)) = registry.restore_session_seen(pane)
            && generation == agent_id
        {
            return Ok(seen);
        }
        if registry.agent_record_any(agent_id).is_none() || registry.pane_occupant_has_exited(pane)
        {
            return Err(RestoreFailure::Exited);
        }
        if Instant::now() >= deadline {
            return Err(RestoreFailure::TimedOut);
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

    /// PRD #1790 stage 3 (audit F5): a selection is bounded before anything
    /// is read: an agent named twice, more agents than an item lists, or an
    /// id longer than the deck's own refuses the whole request.
    #[test]
    fn a_repeated_or_oversized_selection_is_refused_before_anything_is_read() {
        let ok = |n: usize| (0..n).map(|n| format!("r-select/{n}")).collect::<Vec<_>>();
        assert_eq!(check_selection("w-item", &[]), Ok(()));
        assert_eq!(check_selection("w-item", &ok(MAX_ITEM_AGENTS)), Ok(()));
        for (id, attempts) in [
            ("w-item", ok(MAX_ITEM_AGENTS + 1)),
            (
                "w-item",
                vec!["r-a/1".into(), "r-b/1".into(), "r-a/1".into()],
            ),
            ("w-item", vec!["x".repeat(MAX_SELECTOR_BYTES + 1)]),
            (&*"w".repeat(MAX_ITEM_ID_BYTES + 1), ok(1)),
        ] {
            assert_eq!(
                check_selection(id, &attempts),
                Err(RestoreRefusal::BadSelection),
                "{} agents",
                attempts.len()
            );
        }
    }

    /// PRD #1790 stage 3 (review R#3): at act time a gone worktree refuses
    /// either action, and an open pull request refuses a start over only.
    #[test]
    fn act_time_git_facts_refuse_a_gone_worktree_and_an_open_pr_start_over() {
        let facts = |worktree_exists, pr| GitFacts {
            worktree_exists,
            pr,
            ..GitFacts::default()
        };
        for want in [Want::Resume, Want::StartOver] {
            assert_eq!(
                git_refusal(&facts(Some(false), PrFact::None), want),
                Some(RestoreRefusal::FolderMissing)
            );
            assert_eq!(git_refusal(&facts(Some(true), PrFact::None), want), None);
            assert_eq!(git_refusal(&facts(None, PrFact::Unknown), want), None);
        }
        assert_eq!(
            git_refusal(&facts(Some(true), PrFact::Open), Want::StartOver),
            Some(RestoreRefusal::PrOpen)
        );
        assert_eq!(
            git_refusal(&facts(Some(true), PrFact::Open), Want::Resume),
            None,
            "a restore only notes the pull request"
        );
    }

    /// PRD #1790 stage 3 (audit F2): an attempt, and a conversation, are
    /// claimed once across every claimer of one state directory, and a
    /// released claim leaves no file behind and can be taken again.
    #[cfg(unix)]
    #[test]
    fn a_claim_holds_its_attempt_and_conversation_against_a_second_claimer() {
        const SESSION: &str = "fedcba98-7654-4321-8fab-0123456789ab";
        let state = crate::test_temp::tempdir().unwrap();
        let one = AttemptId::parse("r-claim/1").unwrap();
        let two = AttemptId::parse("r-claim/2").unwrap();
        let first = claim::take(state.path(), &one, Some(SESSION)).expect("free");
        assert_eq!(
            claim::take(state.path(), &one, None).err(),
            Some(RestoreRefusal::AlreadyRestoring)
        );
        assert_eq!(
            claim::take(state.path(), &two, Some(SESSION)).err(),
            Some(RestoreRefusal::SessionInUse)
        );
        // The refused claim of `two` released `two`'s attempt again.
        drop(claim::take(state.path(), &two, None).expect("two's attempt is free"));
        drop(first);
        let dir = state.path().join(claim::CLAIMS_DIR);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "a released claim leaves nothing behind"
        );
        assert!(claim::take(state.path(), &two, Some(SESSION)).is_ok());
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// PRD #1790 stage 3 (audit F2): a claim file opened before its holder
    /// released it, and locked after, is not the claim any more: the name
    /// was removed, so locking the old file claims nothing.
    #[cfg(unix)]
    #[test]
    fn a_claim_file_released_after_it_was_opened_claims_nothing() {
        let state = crate::test_temp::tempdir().unwrap();
        let attempt = AttemptId::parse("r-claim/3").unwrap();
        let held = claim::take(state.path(), &attempt, None).expect("free");
        let path = state
            .path()
            .join(claim::CLAIMS_DIR)
            .join(claim::attempt_name(&attempt));
        let stale = std::fs::File::open(&path).unwrap();
        drop(held);
        assert!(crate::platform::lock::try_lock_file(&stale).unwrap());
        assert!(!path.exists(), "the released name is gone");
        // A new claimer creates the name afresh, and is not refused by the
        // stale descriptor's lock on the removed file.
        assert!(claim::take(state.path(), &attempt, None).is_ok());
    }

    /// PRD #1790 stage 3 (audit F4): the sweep never signals the daemon's
    /// own session, and answers an empty session as stopped.
    #[cfg(unix)]
    #[test]
    fn the_sweep_spares_the_daemons_own_session_and_answers_an_empty_one() {
        let own = crate::platform::proc::session_of(std::process::id()).expect("a session");
        assert!(!sweep_session(own, Duration::from_millis(10)));
        #[cfg(target_os = "linux")]
        assert!(sweep_session(i32::MAX as u32, Duration::from_millis(10)));
    }

    /// PRD #1790 stage 3 (review R#3): a launch of a live run, not ended, in
    /// the same folder is newer work there; one that ended, of a run that is
    /// gone, or in another folder is not.
    #[test]
    fn newer_live_work_in_the_same_folder_makes_it_in_use() {
        use crate::work_journal::{EndHow, JournalRead, Line};
        let work = WorkId::parse("w-abcdefgh-0000000000000001").expect("a work id");
        let target = AttemptId::parse("r-old/1").expect("an attempt id");
        let newer = AttemptId::parse("r-live/2").expect("an attempt id");
        let ended = AttemptId::parse("r-live/3").expect("an attempt id");
        let record = |attempt: &AttemptId, cwd: &str| LaunchRecord {
            v: crate::launch_records::SCHEMA_VERSION,
            work: work.clone(),
            attempt: attempt.clone(),
            command: None,
            shell: None,
            agent_type: None,
            cwd: Some(cwd.into()),
            cwd_moved_from: None,
            name: None,
            role: None,
            orchestration: None,
            orchestration_id: None,
            ts_ms: 1,
            pid: None,
            pid_start: None,
        };
        let read = JournalRead {
            lines: vec![Line {
                v: 1,
                ts_ms: 1,
                run: "r-live".into(),
                seq: 1,
                event: Event::AttemptEnded {
                    work: work.clone(),
                    attempt: ended.clone(),
                    how: EndHow::Exited,
                    code: None,
                    cause: None,
                },
            }],
            ..JournalRead::default()
        };
        let sessions = Sessions::of(&read);
        let live = |run: &str| run == "r-live";
        let mut launches = vec![record(&target, "/w"), record(&ended, "/w")];
        assert!(!sessions.folder_in_use(&target, "/w", &launches, &live));
        launches.push(record(&newer, "/other"));
        assert!(!sessions.folder_in_use(&target, "/w", &launches, &live));
        launches.push(record(&newer, "/w"));
        assert!(sessions.folder_in_use(&target, "/w", &launches, &live));
        assert!(!sessions.folder_in_use(&target, "/w", &launches, &|_: &str| false));
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
