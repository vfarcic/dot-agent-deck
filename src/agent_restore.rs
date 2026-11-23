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
//!    not list, refuses a restore but not a start over. A process that has
//!    exited and only waits to be reaped counts as gone, here and in every
//!    later check alike (audit F4b). The same holds, strictly, for every
//!    agent an earlier failed restore of the attempt started
//!    (`restore_failed`'s `to`, audit F4): a restore or start over is
//!    refused while any process of it may be left;
//! 7. no other agent the deck runs is in the same conversation, nor being
//!    restored into it (`session-in-use`, [`Sessions::in_use`],
//!    [`InFlight`], [`claim`]): an agent a restore confirmed counts as in
//!    its conversation from the durable successor link alone (audit B1), and
//!    an agent a failed restore could not be proven to stop fences the
//!    conversations it may be writing to, from every item, until it is
//!    proven gone ([`Sessions::fenced`], audit B2); and no newer live work
//!    runs in the agent's folder, by whatever path it was started there
//!    (`directory-in-use`, review R#3, audit S1);
//! 8. for a start over of a dispatched unit, the saved task is read through
//!    the held folder without following a symlink, a regular owner-only file
//!    of bounded size ([`crate::dispatch_task::read_saved_in`]);
//! 9. the request came from the person: one carrying a pane claim
//!    ([`CallerClaim`]) is refused, as reclaim's is (the same cooperative
//!    guard, not an authentication boundary);
//! 10. Git and GitHub, with the list's bounded probes
//!     ([`crate::interrupted_work_daemon::item_git_facts`], review R#3),
//!     read for a start that acts in [`start`], with its claims held, for
//!     the item as the fresh read found it (audit S5): a worktree that is
//!     gone refuses either action (`folder-missing`); an open pull request on
//!     the item's branch refuses a start over (`pr-open`), which would repeat
//!     the task beside it, and only adds a note to a restore's question,
//!     since the resumed agent may be the one maintaining it;
//! 11. for a start over, a recorded command the deck can read does not
//!     reopen an earlier conversation
//!     ([`crate::agent_registry::FreshSpec::command_reopens`],
//!     `command-reopens`, audit F3): an early refusal only; the launch
//!     shim's check at the real arguments is the guarantee (below).
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
//! every check above runs again from a fresh read ([`revalidate`]). A
//! started agent's claims are released once its successor link,
//! `attempt_restored` with the conversation it was confirmed in, has been
//! written and waited for (audit B1), or once it was stopped after a
//! failure. A link that cannot be written stops the agent (`not-recorded`);
//! a failure whose stop can be neither proven nor recorded keeps the claims
//! for this daemon's life ([`keep_fenced`]). A second request meanwhile is
//! refused (`already-restoring`, `session-in-use`); one after it finds the
//! item closed, or the new agent in the conversation.
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
//! **Start over.** The recorded command, in the verified folder. For an
//! agent with a [`crate::agent_registry::FreshSpec`] (Claude Code, Codex,
//! OpenCode, Pi, Devin), the spawn carries a one-shot fresh-start request
//! ([`crate::telemetry::shim::FRESH_ENV_KEYS`]) and the shim directory first
//! on its `PATH`: the shim checks the arguments the program it stands in
//! front of is started with (the first program of the agent's name on
//! `PATH` after the shim, which may be a script of the user's) and refuses
//! ones that select an existing conversation, and starts Claude Code under a
//! new conversation id the deck chose. A refusal answers
//! `command-reopens`; a shim not reached, or a Claude Code whose first
//! attested conversation is not the new one, answers `not-fresh`; either
//! stops the agent before anything is posted to it ([`confirm_fresh`],
//! audit B3). Antigravity and a plain shell carry no request and are not
//! checked. A dispatched unit's saved task is posted as its first inbox
//! message (reason `saved task`), and no restart note: the agent is new and
//! has no conversation to be told about. Then
//! `attempt_restored{how: started_over}` and `closed how=started_over`.
//!
//! **Stopping an agent it started** ([`stop_proven`], audit F4, F4a,
//! round-4 F5) kills every process in the POSIX session its leader leads (a
//! child that moved to another process group, or ignores `SIGTERM`), the
//! leader stopped first and killed last, while that leader, named by the
//! pid and start time read as it was spawned, shows the session is still
//! the launch's ([`sweep_session`]: identity-checked, through a pidfd on
//! Linux); then closes it as `StopAgent` would. It answers stopped only once
//! no process in that session can run.
//!
//! **What this does not guard against.** Another process can change the
//! folder between the last check and the spawn, as for reclaim, and GitHub
//! can change after its last read. The session scans see processes still in
//! a session; one that left it (`setsid`) is not found. Outside Linux's
//! pidfds a pid reissued between the sweep's identity check and its signal
//! is not ruled out ([`crate::platform::proc::ProcessHandle`]). A launch in
//! which no program of the agent's name is started through `PATH` (the
//! agent called by an absolute path, `PATH` replaced) gets no
//! acknowledgement, so it is not confirmed (`not-resumed`, `not-fresh`). A
//! Claude Code start over is confirmed by the new conversation the agent
//! reports, however it was started; for Codex, OpenCode, Pi and Devin the
//! check is of the arguments the shim sees, so a script of the user's named
//! after the agent that selects a conversation itself, or a program a launch
//! starts after its fresh-start token was spent, is not checked (round-4
//! F3/B1).

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
/// for a restore the conversation it resumes into; and what agents this
/// daemon could neither prove stopped nor record fence for the rest of its
/// life ([`keep_fenced`], round-4 B2): conversations, and agent kinds whose
/// survivor's conversation is not known (`None`: of a kind not known).
#[derive(Default)]
struct Held {
    attempts: HashSet<AttemptId>,
    sessions: HashSet<String>,
    fenced_sessions: HashSet<String>,
    fenced_kinds: HashSet<Option<AgentType>>,
}

static IN_FLIGHT: LazyLock<Mutex<Held>> = LazyLock::new(|| Mutex::new(Held::default()));

/// One attempt, and the conversation a restore of it resumes into, held
/// against a second restore or start-over in this daemon until dropped: of
/// the same attempt (`already-restoring`), or of any attempt into the same
/// conversation (`session-in-use`, audit "later slices" 3); and at most
/// [`MAX_PENDING_RESTORES`] at once (`too-many-pending`). A restore of an
/// agent of `kind` is refused `session-in-use` too while this daemon fences
/// its conversation or its kind ([`keep_fenced`]). [`claim`] holds the same
/// against another daemon on the state directory.
struct InFlight {
    attempt: AttemptId,
    session: Option<String>,
}

impl InFlight {
    fn take(
        attempt: &AttemptId,
        session: Option<&str>,
        kind: Option<&AgentType>,
    ) -> Result<Self, RestoreRefusal> {
        let mut held = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if held.attempts.contains(attempt) {
            return Err(RestoreRefusal::AlreadyRestoring);
        }
        if let Some(session) = session
            && (held.sessions.contains(session)
                || held.fenced_sessions.contains(session)
                || held.fenced_kinds.contains(&None)
                || held.fenced_kinds.contains(&kind.cloned()))
        {
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

    /// Round-4 B2: the file a fence on every conversation of agents of
    /// `kind` locks (`None`: of every kind), held by a daemon that could
    /// neither prove an agent of that kind stopped nor record it
    /// ([`super::keep_fenced`]).
    pub(crate) fn kind_name(kind: Option<&crate::event::AgentType>) -> String {
        let key = kind
            .and_then(|kind| serde_json::to_value(kind).ok())
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "any".to_string());
        format!("kind-{}.lock", hex(&key))
    }

    /// Whether another descriptor holds `name`'s lock in `dir` now, looked
    /// at without taking the claim: the name is left as it was.
    fn held_elsewhere(dir: &TrustedDir, name: &str) -> std::io::Result<bool> {
        let file = match dir.open_regular(name) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        // The lock, if taken, goes with `file`; the name stays.
        Ok(!crate::platform::lock::try_lock_file(&file)?)
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
    /// another daemon holds either, and `session-in-use` for a `session`
    /// while a daemon fences the conversations of agents of `kind`, or of
    /// every kind ([`kind_name`]); `spawn-failed` when the directory cannot
    /// be opened or a lock cannot be taken or looked at for another reason.
    pub(crate) fn take(
        state_dir: &Path,
        attempt: &AttemptId,
        session: Option<&str>,
        kind: Option<&crate::event::AgentType>,
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
            for fence in [kind_name(None), kind_name(kind)] {
                if held_elsewhere(&dir, &fence).map_err(failed)? {
                    return Err(RestoreRefusal::SessionInUse);
                }
            }
        }
        Ok(Claims { _held: held })
    }

    /// Round-4 B2: hold, for as long as the answer is kept, a claim on each
    /// of `sessions` and, when `kind` is given, on the fence of every
    /// conversation of agents of that kind (`Some(None)`: of every kind), in
    /// `state_dir`. What could not be held is left out; `held_all` says
    /// whether anything was.
    pub(crate) fn fence(
        state_dir: &Path,
        sessions: &[String],
        kind: Option<Option<&crate::event::AgentType>>,
    ) -> Fence {
        let mut out = Fence {
            _held: Vec::new(),
            held_all: true,
        };
        let Ok(dir) =
            TrustedDir::anchor(state_dir).and_then(|anchor| anchor.child(CLAIMS_DIR, true))
        else {
            out.held_all = false;
            return out;
        };
        let names = sessions
            .iter()
            .map(|session| session_name(session))
            .chain(kind.map(kind_name));
        for name in names {
            // A daemon only looking at a kind's fence holds its lock for a
            // moment: try again a few times.
            let mut taken = None;
            for _ in 0..5 {
                if let Ok(Some(one)) = lock(&dir, &name) {
                    taken = Some(one);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            match taken {
                Some(one) => out._held.push(one),
                None => out.held_all = false,
            }
        }
        out
    }

    /// What [`fence`] holds.
    pub(crate) struct Fence {
        _held: Vec<One>,
        pub(crate) held_all: bool,
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
/// attempt's latest session, which attempts ended, and what restores before
/// it left behind.
struct Sessions<'a> {
    latest: HashMap<&'a AttemptId, crate::work_journal::SessionReport<'a>>,
    ended: HashSet<&'a AttemptId>,
    /// Audit F4: for each attempt, the agents its failed restores started.
    successors: HashMap<&'a AttemptId, Vec<&'a AttemptId>>,
    /// Audit B1: each restore's successor and the conversation it was
    /// confirmed in (`attempt_restored`'s `session`), the successor's
    /// ownership of that conversation that does not wait for its own
    /// `attempt_session`.
    edges: Vec<(&'a AttemptId, &'a str)>,
    /// Audit B2: each failed restore's successor whose stop could not be
    /// proven, the conversations it may be writing to (`restore_failed`'s
    /// `sessions`), and whether the one it is in is not known
    /// (`unknown_session`, round-4 B2).
    fences: Vec<(&'a AttemptId, &'a [String], bool)>,
}

impl<'a> Sessions<'a> {
    fn of(read: &'a crate::work_journal::JournalRead) -> Self {
        let mut ended = HashSet::new();
        let mut successors: HashMap<&AttemptId, Vec<&AttemptId>> = HashMap::new();
        let mut edges = Vec::new();
        let mut fences = Vec::new();
        for line in &read.lines {
            match &line.event {
                Event::AttemptEnded { attempt, .. } => {
                    ended.insert(attempt);
                }
                Event::RestoreFailed {
                    attempt,
                    to: Some(to),
                    sessions,
                    unknown_session,
                    ..
                } => {
                    successors.entry(attempt).or_default().push(to);
                    if !sessions.is_empty() || *unknown_session {
                        fences.push((to, sessions.as_slice(), *unknown_session));
                    }
                }
                Event::AttemptRestored {
                    to,
                    session: Some(session),
                    ..
                } => edges.push((to, session.as_str())),
                _ => {}
            }
        }
        Sessions {
            latest: crate::work_journal::latest_sessions(&read.lines),
            ended,
            successors,
            edges,
            fences,
        }
    }

    /// Whether an attempt other than `attempt`, not ended and of a run that
    /// is live, is in `session` now: an agent the deck runs (a copy restored
    /// before, say) that a resume would write beside. A restored agent is in
    /// the conversation its restore confirmed (audit B1) until it reports
    /// another one itself.
    fn in_use(&self, attempt: &AttemptId, session: &str, live: &dyn Fn(&str) -> bool) -> bool {
        let running = |other: &AttemptId| {
            other != attempt && !self.ended.contains(other) && live(other.run())
        };
        self.latest
            .iter()
            .any(|(other, report)| report.session == session && running(other))
            || self.edges.iter().any(|(to, confirmed)| {
                *confirmed == session
                    && running(to)
                    && self
                        .latest
                        .get(to)
                        .is_none_or(|report| report.session == session)
            })
    }

    /// Audit B2: whether an agent a failed restore started, and could not be
    /// proven to have stopped, may still be writing to `session`, a
    /// conversation of an agent of `kind`: its launch record names a
    /// process, or a session member, that is not proven gone
    /// ([`launch_gone`]), or there is no single record to check. It may be
    /// when its failure named `session`, or named no conversation it is
    /// known to be in (round-4 B2) and it is of `kind` (or either kind is not
    /// known). Its attempt having ended in the journal does not count: what
    /// it left running is what matters.
    fn fenced(&self, session: &str, kind: Option<&AgentType>, launches: &[LaunchRecord]) -> bool {
        self.fences.iter().any(|(to, sessions, unknown)| {
            let mut records = launches.iter().filter(|r| r.attempt == **to);
            let record = match (records.next(), records.next()) {
                (Some(record), None) => Some(record),
                _ => None,
            };
            let same_kind = match (record.and_then(|r| r.agent_type.as_ref()), kind) {
                (Some(theirs), Some(ours)) => theirs == ours,
                _ => true,
            };
            (sessions.iter().any(|fenced| fenced == session) || (*unknown && same_kind))
                && record.is_none_or(|record| launch_gone(record).is_err())
        })
    }

    /// Review R#3, audit S1/F7: whether a launch other than `attempt`'s,
    /// not ended and of a live run, started in the directory `cwd` names:
    /// newer work in the agent's folder that a restore or start over would
    /// run beside. Directories are compared by what they are
    /// ([`DirKey`]: the canonical path, or the device and inode), so a
    /// symlink to the folder or a spelling with `..` in it is the same
    /// folder; a live launch whose folder cannot be read, or that has none
    /// recorded (its prepared folder moved, say), counts as in it (round-4
    /// S1). Blocking.
    fn folder_in_use(
        &self,
        attempt: &AttemptId,
        cwd: &str,
        launches: &[LaunchRecord],
        live: &dyn Fn(&str) -> bool,
    ) -> bool {
        let target = DirKey::of(Path::new(cwd));
        launches.iter().any(|other| {
            if other.attempt == *attempt
                || self.ended.contains(&other.attempt)
                || !live(other.attempt.run())
            {
                return false;
            }
            // Round-4 S1: nothing says it is somewhere else.
            let Some(other_cwd) = other.cwd.as_deref() else {
                return true;
            };
            if other_cwd == cwd {
                return true;
            }
            match (&target, DirKey::of(Path::new(other_cwd))) {
                (Some(target), Some(other)) => target.same(&other),
                // The agent's own folder that cannot be read is refused
                // later (`folder-missing`, `folder-unsafe`); another's is
                // taken to be this one.
                (None, _) => false,
                (Some(_), None) => true,
            }
        })
    }
}

/// Audit S1/F7: what a directory is, for telling whether two spellings name
/// one: its canonical path and, on Unix, its device and inode.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DirKey {
    canonical: std::path::PathBuf,
    inode: Option<(u64, u64)>,
}

impl DirKey {
    /// `path`'s key, or `None` when it cannot be read or is not a
    /// directory (round-4 S1).
    fn of(path: &Path) -> Option<Self> {
        let canonical = std::fs::canonicalize(path).ok()?;
        let meta = std::fs::metadata(&canonical).ok()?;
        if !meta.is_dir() {
            return None;
        }
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt as _;
            Some((meta.dev(), meta.ino()))
        };
        #[cfg(not(unix))]
        let inode = {
            let _ = meta;
            None
        };
        Some(DirKey { canonical, inode })
    }

    fn same(&self, other: &DirKey) -> bool {
        self.canonical == other.canonical || (self.inode.is_some() && self.inode == other.inode)
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

/// PRD #1790 round-4 F1: whether the agent each attempt names, a successor
/// a restore's safety evidence names, is proven gone ([`launch_gone`]), for
/// journal compaction to let that evidence go
/// ([`crate::work_journal::WorkJournal::compact`]). The launch records in
/// `state_dir` are read once; a read that fails, stops at its budget or
/// skips a record, an attempt without exactly one record, or a process that
/// cannot be checked answers no. Blocking.
pub(crate) fn successor_gone_check(state_dir: &Path) -> impl Fn(&AttemptId) -> bool + use<> {
    let records = match crate::launch_records::read_all_with_stats(state_dir) {
        Ok(read) if !read.truncated && read.skipped == 0 => read.records,
        _ => Vec::new(),
    };
    move |attempt: &AttemptId| {
        let mut found = records.iter().filter(|r| r.attempt == *attempt);
        match (found.next(), found.next()) {
            (Some(record), None) => launch_gone(record).is_ok(),
            _ => false,
        }
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
            // An early refusal only: the shim's check at the real arguments
            // is what holds a start over to a fresh start (audit B3).
            let reopens = record.command.as_deref().is_some_and(|command| {
                crate::agent_registry::ALL
                    .iter()
                    .filter_map(|spec| spec.fresh_start)
                    .any(|fresh| fresh.command_reopens(command))
            });
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
            if sessions.in_use(&agent.attempt, session, live)
                || sessions.fenced(session, agent_type.as_ref(), launches)
            {
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
        // A zombie leader may still lead members: they are what is scanned.
        ProcessProbe::Started(_) | ProcessProbe::Absent | ProcessProbe::Exited => {}
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
) -> Result<(Target, Vec<WorkId>, InterruptedItem), RestoreRefusal> {
    let refused = |refused: RestoreRefused| refused.reason;
    let Prepared {
        works,
        mut targets,
        item: fresh_item,
        ..
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
    Ok((fresh, works, fresh_item))
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
    // resume continues the work that pull request is about. A restore that
    // acts reads it again in `start`, with its claims held (audit S5), and
    // does not carry it.
    let git_refused = if dry_run {
        let git = crate::interrupted_work_daemon::item_git_facts(&item).await;
        let refused = git_refusal(&git, Want::Resume);
        outcome.git = Some(git);
        refused
    } else {
        None
    };
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
    // A dry run's Git facts, for the question; a start over that acts reads
    // them again in `start`, with its claims held (audit S5).
    let git = if dry_run {
        Some(crate::interrupted_work_daemon::item_git_facts(&item).await)
    } else {
        None
    };
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
    if let Some(reason) = git
        .as_ref()
        .and_then(|git| git_refusal(git, Want::StartOver))
    {
        return refused(RestoreRefused::new(Some(target.attempt), reason), item_id);
    }
    match start(&daemon, &journal, &item.id, target, None, dry_run).await {
        Started::Would(_) => StartOverOutcome {
            dry_run,
            would_start: true,
            git,
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
/// again with them held ([`revalidate`]), Git and GitHub among them for a
/// start that acts (audit S5); the folder is checked here, last before the
/// spawn, and a dispatched unit's saved task is read through the held
/// folder.
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
    let held = match InFlight::take(
        &target.attempt,
        target.session.as_deref(),
        target.agent_type.as_ref(),
    ) {
        Ok(held) => held,
        Err(reason) => return refuse(reason),
    };
    let Some(state_dir) = journal.state_dir() else {
        return refuse(RestoreRefusal::IncompleteRead);
    };
    let claims = match claim::take(
        state_dir,
        &target.attempt,
        target.session.as_deref(),
        target.agent_type.as_ref(),
    ) {
        Ok(claims) => claims,
        Err(reason) => return refuse(reason),
    };
    let (target, works, fresh_item) = match revalidate(daemon.registry, item, &target, want).await {
        Ok(fresh) => fresh,
        Err(reason) => return refuse(reason),
    };
    // Audit S5: Git and GitHub read for the item as this fresh read found
    // it, with the claims held, close to the spawn. A dry run read them
    // before, for the question.
    if !dry_run {
        let git = crate::interrupted_work_daemon::item_git_facts(&fresh_item).await;
        if let Some(reason) = git_refusal(&git, want) {
            return refuse(reason);
        }
    }
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
    // Audit B3: the program a start over's fresh-start request is for, by
    // the agent's type. An agent the shim does not stand in front of (the
    // default shell, Antigravity) has none, and is started unchecked.
    let fresh_spec = match want {
        Want::StartOver => target
            .agent_type
            .as_ref()
            .and_then(|agent| crate::agent_registry::spec(agent).fresh_start),
        Want::Resume => None,
    };
    // The shim this restore or start over goes through, as this daemon
    // installed it, and the directory it acknowledges in, held.
    let shim_program = target.program.or(fresh_spec.map(|fresh| fresh.program));
    let shim = match shim_program {
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
    // Audit B3: the new conversation id a start over of Claude Code is
    // started under, which its first report must name.
    let mut new_session = None;
    if let (Some(program), Some((shim_dir, _))) = (shim_program, &shim) {
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
        let [agent_key, session_key, token_key] = match want {
            Want::Resume => crate::telemetry::shim::RESUME_ENV_KEYS,
            Want::StartOver => crate::telemetry::shim::FRESH_ENV_KEYS,
        };
        env.push((agent_key.to_string(), program.to_string()));
        let session = match (&target.session, fresh_spec) {
            (Some(session), _) => Some(session.clone()),
            (None, Some(fresh)) if fresh.new_id_option.is_some() => {
                let Some(id) = new_conversation_id() else {
                    return refuse(RestoreRefusal::SpawnFailed);
                };
                new_session = Some(id.clone());
                Some(id)
            }
            (None, _) => None,
        };
        if let Some(session) = session {
            env.push((session_key.to_string(), session));
        }
        env.push((token_key.to_string(), minted.clone()));
        token = Some(minted);
    }
    // Every start through the shim watches for the conversation the agent
    // reports first: a restore and a Claude Code start over are confirmed by
    // it, and any failure keeps it as evidence of where the agent is
    // (round-4 B2).
    if target.session.is_some() || token.is_some() {
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
    // Audit F4, F4a, round-4 F5: the new agent's session leader, named by
    // the pid and start time the registry read as it spawned it, before
    // anything could reap it, so a stop is proven against that process's
    // session only ([`sweep_session`]). No start time read then: no leader,
    // and no stop can be proven.
    let leader = daemon
        .registry
        .child_identity(&agent_id)
        .and_then(|(pid, start)| Some(crate::platform::proc::SessionMember { pid, start: start? }));
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
    // A failed start: stop the agent, proven. Record the failure with the
    // agent it started when `failure` is given, or when the stop could not
    // be proven, then with the conversations `fence` says it may be writing
    // to, which no restore enters until that agent is proven gone (audit
    // B2); when the deck does not know which conversation it is in, no
    // restore of an agent of its kind starts until then (round-4 B2). A
    // record that cannot be written leaves the claims held for this daemon's
    // life instead, with a claim on each of those conversations, or on the
    // kind, beside them: the fence it could not write ([`keep_fenced`]).
    let held = Mutex::new(Some((held, claims)));
    let kind = target.agent_type.clone();
    let fail = |failure: Option<(RestoreFailure, Option<ShimOutcome>)>, fence: Fence, reason| {
        let started = &started;
        let attempt = &target.attempt;
        let work = &record.work;
        let new_attempt = new_attempt.clone();
        let held = &held;
        let kind = kind.clone();
        async move {
            let stopped = stop_proven(daemon, started, leader).await;
            let fence = fence.of(kind.as_ref());
            let failure = match (failure, stopped) {
                (Some(failure), _) => Some(failure),
                (None, false) => Some((RestoreFailure::Unknown, None)),
                (None, true) => None,
            };
            let recorded = match failure {
                Some((failure, shim)) => {
                    !failure_refused_for_test()
                        && record_blocking(
                            journal,
                            vec![Event::RestoreFailed {
                                work: work.clone(),
                                attempt: attempt.clone(),
                                failure,
                                shim,
                                to: new_attempt.clone(),
                                sessions: if stopped {
                                    Vec::new()
                                } else {
                                    fence.sessions.clone()
                                },
                                unknown_session: !stopped && fence.unknown,
                            }],
                        )
                        .await
                }
                None => true,
            };
            if !stopped && (!recorded || new_attempt.is_none()) {
                keep_fenced(
                    held.lock().unwrap_or_else(|e| e.into_inner()).take(),
                    journal.state_dir(),
                    &fence,
                    kind.as_ref(),
                );
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

    // The conversation the new agent is confirmed in, for the durable edge.
    let (how, close, first, confirmed_session) = match (&target.session, mode) {
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
                // Round-4 B2: where it went when it reported, else unknown.
                let fence = Fence::new(std::iter::once(session.clone()).chain(seen.clone()))
                    .unknown_unless(seen.is_some());
                return fail(
                    Some((failure, shim_outcome)),
                    fence,
                    RestoreRefusal::NotResumed,
                )
                .await;
            }
            (
                RestoredHow::Resumed,
                CloseHow::Restored,
                Some((RESTART_NOTE_REASON, mode.note().to_string())),
                Some(session.clone()),
            )
        }
        _ => {
            let mut confirmed_session = None;
            if let (Some(fresh), Some((_, acks)), Some(token)) =
                (fresh_spec, shim.as_ref(), token.as_deref())
            {
                let checked = confirm_fresh(
                    daemon.registry,
                    &pane_id,
                    &agent_id,
                    (acks, token),
                    new_session.as_deref(),
                    confirm_timeout(),
                )
                .await;
                daemon.registry.unwatch_restore_session(&pane_id);
                match checked {
                    Ok(seen) => confirmed_session = seen,
                    Err(Unfresh { reason, seen }) => {
                        tracing::info!(
                            attempt = %target.attempt,
                            program = fresh.program,
                            ?reason,
                            seen = ?seen,
                            "restore: the started-over agent was not started fresh; stopping it"
                        );
                        // Only an unproven stop is recorded: the item stays
                        // as it was, its verdict unchanged.
                        let fence =
                            Fence::new(target.old_session.iter().cloned().chain(seen.clone()))
                                .unknown_unless(seen.is_some());
                        return fail(None, fence, reason).await;
                    }
                }
            }
            (
                RestoredHow::StartedOver,
                CloseHow::StartedOver,
                task.map(|task| (SAVED_TASK_REASON, task)),
                confirmed_session,
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
        let known = confirmed_session.is_some();
        let fence = Fence::new(confirmed_session).unknown_unless(known);
        return fail(None, fence, RestoreRefusal::NoteNotDelivered).await;
    }
    // Audit B1: the successor link, with the conversation it was confirmed
    // in, is written and waited for before the closing events and before
    // the claims are released: every later check, in any daemon, finds the
    // new agent in that conversation from it. One that cannot be written is
    // a safety failure, not a handoff: the agent is stopped again.
    let edge_written = match &new_attempt {
        Some(to) if !edge_refused_for_test() => {
            record_blocking(
                journal,
                vec![Event::AttemptRestored {
                    work: record.work.clone(),
                    from: target.attempt.clone(),
                    to: to.clone(),
                    how,
                    session: confirmed_session.clone(),
                }],
            )
            .await
        }
        _ => false,
    };
    if !edge_written {
        tracing::warn!(
            attempt = %target.attempt,
            "restore: the restored agent's successor link could not be recorded; stopping it"
        );
        let known = confirmed_session.is_some();
        let fence = Fence::new(confirmed_session).unknown_unless(known);
        return fail(None, fence, RestoreRefusal::NotRecorded).await;
    }
    let events: Vec<Event> = works
        .iter()
        .map(|work| Event::Closed {
            work: work.clone(),
            how: close,
        })
        .collect();
    let recorded = record_blocking(journal, events).await;
    drop(held);
    Started::Done(RestoredAgent {
        attempt: target.attempt,
        agent_id,
        pane_id,
        name: record.name.clone(),
        first_message_posted,
        recorded,
    })
}

/// Round-4 B2: the conversations an agent a failed start could not be
/// proven to have stopped may be writing to: the one asked for, and the one
/// it reported or was confirmed in, when it did; and whether the one it is
/// in is not known.
#[derive(Debug, Clone, Default)]
struct Fence {
    sessions: Vec<String>,
    unknown: bool,
}

impl Fence {
    fn new(sessions: impl IntoIterator<Item = String>) -> Self {
        let mut out: Vec<String> = Vec::new();
        for session in sessions {
            if !out.contains(&session) {
                out.push(session);
            }
        }
        Fence {
            sessions: out,
            unknown: false,
        }
    }

    /// Unknown, unless the agent reported or was confirmed in a
    /// conversation.
    fn unknown_unless(mut self, known: bool) -> Self {
        self.unknown = !known;
        self
    }

    /// The fence an agent of `kind` needs: one whose conversation is not
    /// known fences every conversation of its kind only when a restore of
    /// that kind exists (no restore of another kind can enter one of its
    /// conversations); of a kind not known, it does.
    fn of(mut self, kind: Option<&AgentType>) -> Self {
        let restorable = kind.is_none_or(|kind| crate::agent_registry::spec(kind).resume.is_some());
        self.unknown &= restorable;
        self
    }
}

/// Audit B1/B2, round-4 B2: what an agent's start held, kept for the rest of
/// this daemon's life when the deck could neither prove that agent stopped
/// nor write that down, and beside it a claim on each conversation `fence`
/// names and, when the one it is in is not known, on every conversation of
/// its `kind` (in this daemon, and as a claim file in the state directory
/// `state_dir`): no other restore in this daemon enters its attempt or one
/// of those conversations meanwhile, nor one in another daemon on the state
/// directory, for each claim this one could take. A claim another daemon
/// held at that moment could not be taken, and is logged.
#[cfg(unix)]
fn keep_fenced(
    held: Option<(InFlight, claim::Claims)>,
    state_dir: Option<&Path>,
    fence: &Fence,
    kind: Option<&AgentType>,
) {
    /// One fence kept: the start's own claims, and the ones taken for it.
    type Kept = (Option<(InFlight, claim::Claims)>, Option<claim::Fence>);
    static FENCED: Mutex<Vec<Kept>> = Mutex::new(Vec::new());
    {
        let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        in_flight
            .fenced_sessions
            .extend(fence.sessions.iter().cloned());
        if fence.unknown {
            in_flight.fenced_kinds.insert(kind.cloned());
        }
    }
    let files = state_dir
        .map(|state_dir| claim::fence(state_dir, &fence.sessions, fence.unknown.then_some(kind)));
    if files.as_ref().is_none_or(|files| !files.held_all) {
        tracing::warn!(
            sessions = ?fence.sessions,
            unknown = fence.unknown,
            "restore: an agent that could not be proven stopped is fenced in this daemon only; \
             another daemon on the state directory may not see every fence"
        );
    }
    FENCED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((held, files));
}

/// Whether the failure record's write fails on purpose: in an e2e build
/// only, when `DOT_AGENT_DECK_TEST_RESTORE_REFUSE_FAILURE` is `1`, so lane 1
/// can reach the path an unproven stop whose `restore_failed` cannot be
/// written takes (round-4 B2).
fn failure_refused_for_test() -> bool {
    #[cfg(feature = "e2e")]
    if std::env::var("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_FAILURE").as_deref() == Ok("1") {
        return true;
    }
    false
}

/// Whether the successor link's write fails on purpose: in an e2e build
/// only, when `DOT_AGENT_DECK_TEST_RESTORE_REFUSE_EDGE` is `1`, so lane 1
/// can reach the path a journal that cannot take it takes (audit B1).
fn edge_refused_for_test() -> bool {
    #[cfg(feature = "e2e")]
    if std::env::var("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_EDGE").as_deref() == Ok("1") {
        return true;
    }
    false
}

/// A new conversation id for a start over that names one (audit B3): a
/// random version-4 UUID in canonical lowercase form, `None` without OS
/// randomness.
fn new_conversation_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// Audit F4, round-4 F5: stop an agent a restore or start over just
/// started. The processes in the session its `leader` leads are killed
/// first, while that leader is there to show the session is still the
/// launch's ([`sweep_session`]), then the agent is closed as `StopAgent`
/// would close it ([`crate::spawn::close_started_agents`]). `true` only when that session
/// was seen with no process left that can run; `false` when the leader was
/// not known, the session could not be read, a process in it could not be
/// signalled or attributed to the launch, or one stayed in it past
/// [`STOP_PROOF_BOUND`].
#[cfg(unix)]
async fn stop_proven(
    daemon: &Daemon<'_>,
    started: &crate::spawn::SpawnedAgent,
    leader: Option<crate::platform::proc::SessionMember>,
) -> bool {
    let close = || {
        crate::spawn::close_started_agents(
            daemon.registry,
            Some(daemon.state),
            Some(daemon.event_tx),
            std::slice::from_ref(started),
        )
    };
    let Some(leader) = leader else {
        close().await;
        return false;
    };
    // In an e2e build only: `DOT_AGENT_DECK_TEST_RESTORE_SKIP_SWEEP=1` leaves
    // what the close did not stop running and answers unproven, so lane 1
    // can reach an unproven stop deterministically (audit B2).
    #[cfg(feature = "e2e")]
    if std::env::var("DOT_AGENT_DECK_TEST_RESTORE_SKIP_SWEEP").as_deref() == Ok("1") {
        close().await;
        return false;
    }
    let swept = tokio::task::spawn_blocking(move || {
        sweep_session(&OsView, leader, STOP_PROOF_BOUND, CONFIRM_POLL)
    })
    .await
    .unwrap_or(false);
    close().await;
    swept
}

/// Audit F4a: what [`sweep_session`] reads of processes and how it signals
/// one, so a test can stand in for the OS.
#[cfg(unix)]
trait ProcessView {
    type Handle;
    /// The session this daemon is in.
    fn own_session(&self) -> Option<u32>;
    /// What holds `pid` now, named by its start time whether it runs, is
    /// stopped, or exited and waits to be reaped (it holds the pid until
    /// then).
    fn probe(&self, pid: u32) -> crate::platform::proc::ProcessProbe;
    /// Whether a pid that is a session's id can be handed to another process
    /// only once that session is empty (Linux).
    fn reissue_proves_empty(&self) -> bool;
    /// The session's members that can still run, each named by pid and
    /// start time.
    fn members(&self, sid: u32) -> Option<Vec<crate::platform::proc::SessionMember>>;
    /// A handle on whichever process holds `pid` now
    /// ([`crate::platform::proc::Opened`]).
    fn open(&self, pid: u32) -> crate::platform::proc::Opened<Self::Handle>;
    /// The start time and session of the process `handle` names, read after
    /// it was opened.
    fn identity(&self, handle: &Self::Handle) -> Option<(u64, u32)>;
    /// `SIGSTOP` through `handle`.
    fn stop(&self, handle: &Self::Handle);
    /// `SIGKILL` through `handle`.
    fn kill(&self, handle: &Self::Handle);
}

/// The OS, as [`sweep_session`] sees it.
#[cfg(unix)]
struct OsView;

#[cfg(unix)]
impl ProcessView for OsView {
    type Handle = crate::platform::proc::ProcessHandle;

    fn own_session(&self) -> Option<u32> {
        crate::platform::proc::session_of(std::process::id())
    }

    fn probe(&self, pid: u32) -> crate::platform::proc::ProcessProbe {
        crate::platform::proc::probe_process_held(pid)
    }

    fn reissue_proves_empty(&self) -> bool {
        cfg!(target_os = "linux")
    }

    fn members(&self, sid: u32) -> Option<Vec<crate::platform::proc::SessionMember>> {
        crate::platform::proc::session_members(sid)
    }

    fn open(&self, pid: u32) -> crate::platform::proc::Opened<Self::Handle> {
        crate::platform::proc::ProcessHandle::open(pid)
    }

    fn identity(&self, handle: &Self::Handle) -> Option<(u64, u32)> {
        let pid = handle.pid();
        Some((
            crate::platform::proc::process_start_of(pid)?,
            crate::platform::proc::session_of(pid)?,
        ))
    }

    fn stop(&self, handle: &Self::Handle) {
        handle.stop();
    }

    fn kill(&self, handle: &Self::Handle) {
        handle.kill();
    }
}

/// Audit F4, F4a, round-4 F5: kill every process in the session `leader`
/// leads, `leader` last, until no process in it can run or `bound` passes;
/// `true` when it was seen so.
///
/// **The launch's own generation of that session is what is signalled**,
/// outside the bare-pid window below. A
/// listing of the session's members counts only when the leader, by pid and
/// start time, was there both before and after it (running, stopped, or
/// exited and not yet reaped): a session id is not handed out again while
/// the process it names holds it, so every member listed then is the
/// launch's. The leader is stopped (`SIGSTOP`) first, so it neither starts
/// more processes nor exits and is reaped while the others are killed, and
/// it is killed once it is the only member left. A member is signalled only
/// through a handle opened on it whose process still has the start time it
/// was listed with and is still in the session; on Linux that handle is a
/// pidfd, and a member no pidfd could be opened on (other than a kernel
/// without the call) is not signalled at all, so the session is not seen
/// empty. With no pidfd (macOS, a kernel before 5.3) the signal goes to the
/// pid, and a pid reissued in the short window after that check is the
/// residual `docs/develop/agent-restore.md` names.
///
/// **Once the leader is seen gone, nothing more is signalled**: members still listed
/// under its number cannot be told from a later session that reused it, so
/// the sweep only waits for the session to be seen empty, and otherwise
/// answers unproven. On Linux, the leader's pid held by a process with
/// another start time means the session emptied before (the kernel hands out
/// no pid that is still a session's id), which answers stopped without a
/// signal; elsewhere it answers unproven. A member that started before the
/// leader cannot be of this launch, and is never signalled. The daemon's own
/// session is never swept, and a session seen empty is not swept again.
#[cfg(unix)]
fn sweep_session<V: ProcessView>(
    view: &V,
    leader: crate::platform::proc::SessionMember,
    bound: Duration,
    poll: Duration,
) -> bool {
    use crate::platform::proc::{Opened, ProcessProbe};
    let sid = leader.pid;
    if view.own_session() == Some(sid) {
        return false;
    }
    let deadline = Instant::now() + bound;
    let present =
        || matches!(view.probe(sid), ProcessProbe::Started(start) if start == leader.start);
    // The leader, stopped, once a handle on it could be had.
    let mut held: Option<V::Handle> = None;
    let mut first = true;
    loop {
        if !first {
            if Instant::now() >= deadline {
                // The stopped leader is not left behind, while it is still
                // the one that was stopped.
                if let Some(handle) = &held
                    && present()
                {
                    view.kill(handle);
                }
                return false;
            }
            std::thread::sleep(poll);
        }
        first = false;
        match view.probe(sid) {
            ProcessProbe::Started(start) if start == leader.start => {}
            ProcessProbe::Started(_) => return view.reissue_proves_empty(),
            // Gone: what is listed under its number is not attributable.
            ProcessProbe::Absent | ProcessProbe::Exited => {
                if view.members(sid).is_some_and(|members| members.is_empty()) {
                    return true;
                }
                continue;
            }
            ProcessProbe::Unknown => continue,
        }
        if held.is_none()
            && let Opened::Handle(handle) = view.open(sid)
            && view.identity(&handle) == Some((leader.start, sid))
        {
            view.stop(&handle);
            held = Some(handle);
        }
        let listed = view.members(sid);
        if !present() {
            continue;
        }
        let Some(listed) = listed else {
            continue;
        };
        if listed.is_empty() {
            return true;
        }
        let others: Vec<_> = listed.iter().filter(|m| m.pid != sid).collect();
        if others.is_empty() {
            match &held {
                Some(handle) => view.kill(handle),
                None => {
                    if let Opened::Handle(handle) = view.open(sid)
                        && view.identity(&handle) == Some((leader.start, sid))
                    {
                        view.kill(&handle);
                    }
                }
            }
            continue;
        }
        for member in others {
            if member.start < leader.start {
                continue;
            }
            if let Opened::Handle(handle) = view.open(member.pid)
                && view.identity(&handle) == Some((member.start, sid))
            {
                view.kill(&handle);
            }
        }
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

/// Why [`confirm_fresh`] gave up: the refusal to answer, and the
/// conversation the agent reported, when it reported one.
#[cfg(unix)]
#[derive(Debug)]
struct Unfresh {
    reason: RestoreRefusal,
    seen: Option<String>,
}

/// Audit B3: wait at most `bound` for the launch shim to settle a start
/// over's fresh-start request (`ack`: the held directory and the token), and
/// for an agent started under `new_session` (Claude Code) to report that
/// conversation as its first. `Ok` with the conversation confirmed, if any.
/// `command-reopens` when the shim refused because the agent's real
/// arguments select an existing conversation; `not-fresh` when the shim was
/// not reached (the agent reported a conversation first, or nothing came in
/// time), refused the new id, the agent exited, or it reported another
/// conversation than the new one.
#[cfg(unix)]
async fn confirm_fresh(
    registry: &AgentPtyRegistry,
    pane: &str,
    agent_id: &str,
    ack: (&crate::platform::trusted_dir::TrustedDir, &str),
    new_session: Option<&str>,
    bound: Duration,
) -> Result<Option<String>, Unfresh> {
    use crate::telemetry::shim::{AckState, ack_outcome, ack_state};
    let (acks, token) = ack;
    let deadline = Instant::now() + bound;
    let not_fresh = |seen| Unfresh {
        reason: RestoreRefusal::NotFresh,
        seen,
    };
    loop {
        let state = ack_state(acks, token);
        // Round-4 B2: read before any answer, so a refusal keeps where the
        // agent went.
        let seen = registry
            .restore_session_seen(pane)
            .filter(|(generation, _)| generation == agent_id)
            .map(|(_, seen)| seen);
        if state == AckState::Refused {
            let reason = match ack_outcome(acks, token) {
                ShimOutcome::SessionIdInvalid => RestoreRefusal::NotFresh,
                _ => RestoreRefusal::CommandReopens,
            };
            return Err(Unfresh { reason, seen });
        }
        match (state, new_session, seen) {
            (AckState::Resumed, None, seen) => return Ok(seen),
            (AckState::Resumed, Some(new), Some(seen)) => {
                return if seen == new {
                    Ok(Some(seen))
                } else {
                    Err(not_fresh(Some(seen)))
                };
            }
            // A conversation reported before the shim checked anything: the
            // launch did not go through it.
            (AckState::Unclaimed | AckState::Pending, _, Some(seen)) => {
                return Err(not_fresh(Some(seen)));
            }
            _ => {}
        }
        if registry.agent_record_any(agent_id).is_none() || registry.pane_occupant_has_exited(pane)
        {
            return Err(not_fresh(None));
        }
        if Instant::now() >= deadline {
            return Err(not_fresh(None));
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
        let first = InFlight::take(&attempt, None, None).expect("free");
        assert_eq!(
            InFlight::take(&attempt, None, None).err(),
            Some(RestoreRefusal::AlreadyRestoring)
        );
        drop(first);
        assert!(InFlight::take(&attempt, None, None).is_ok());
    }

    /// PRD #1790 stage 3 (audit "later slices" 3): two different attempts
    /// resuming into one conversation are not restored at once.
    #[test]
    fn two_attempts_into_one_conversation_are_not_restored_at_once() {
        const SESSION: &str = "fedcba98-7654-4321-8fab-0123456789ab";
        let one = AttemptId::parse("r-session-test/1").expect("an attempt id");
        let two = AttemptId::parse("r-session-test/2").expect("an attempt id");
        let first = InFlight::take(&one, Some(SESSION), None).expect("free");
        assert_eq!(
            InFlight::take(&two, Some(SESSION), None).err(),
            Some(RestoreRefusal::SessionInUse)
        );
        drop(first);
        assert!(InFlight::take(&two, Some(SESSION), None).is_ok());
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
        let first = claim::take(state.path(), &one, Some(SESSION), None).expect("free");
        assert_eq!(
            claim::take(state.path(), &one, None, None).err(),
            Some(RestoreRefusal::AlreadyRestoring)
        );
        assert_eq!(
            claim::take(state.path(), &two, Some(SESSION), None).err(),
            Some(RestoreRefusal::SessionInUse)
        );
        // The refused claim of `two` released `two`'s attempt again.
        drop(claim::take(state.path(), &two, None, None).expect("two's attempt is free"));
        drop(first);
        let dir = state.path().join(claim::CLAIMS_DIR);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "a released claim leaves nothing behind"
        );
        assert!(claim::take(state.path(), &two, Some(SESSION), None).is_ok());
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
        let held = claim::take(state.path(), &attempt, None, None).expect("free");
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
        assert!(claim::take(state.path(), &attempt, None, None).is_ok());
    }

    /// One row of [`FakeView`]'s table: start time, session, zombie.
    #[cfg(unix)]
    type Row = (u64, u32, bool);

    /// A process table a test controls: what each pid holds now (its start
    /// time, its session, and whether it exited and waits to be reaped),
    /// and every signal the sweep sent. Never a real process.
    #[cfg(unix)]
    struct FakeView {
        own: u32,
        reissue_proves_empty: bool,
        /// pid -> (start, session, zombie), as the table reads now.
        table: std::cell::RefCell<HashMap<u32, Row>>,
        /// What `members` answers, in turn, instead of the table's running
        /// members, while any is left.
        listings: std::cell::RefCell<Vec<Vec<crate::platform::proc::SessionMember>>>,
        /// Applied to the table as the first listing is taken: what changes
        /// while the sweep lists (`None` removes the pid).
        on_list: std::cell::RefCell<Vec<(u32, Option<Row>)>>,
        /// Applied to the table after a handle is opened, before its
        /// identity is read: a pid reissued in that window.
        on_open: std::cell::RefCell<HashMap<u32, Row>>,
        /// Pids no pinned handle can be opened on (a full descriptor table).
        unusable: HashSet<u32>,
        stopped: std::cell::RefCell<Vec<u32>>,
        killed: std::cell::RefCell<Vec<(u32, u64)>>,
    }

    #[cfg(unix)]
    impl FakeView {
        fn new(table: &[(u32, u64, u32)]) -> Self {
            FakeView {
                own: 1,
                reissue_proves_empty: true,
                table: std::cell::RefCell::new(
                    table
                        .iter()
                        .map(|&(p, st, sid)| (p, (st, sid, false)))
                        .collect(),
                ),
                listings: std::cell::RefCell::new(Vec::new()),
                on_list: std::cell::RefCell::new(Vec::new()),
                on_open: std::cell::RefCell::new(HashMap::new()),
                unusable: HashSet::new(),
                stopped: std::cell::RefCell::new(Vec::new()),
                killed: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn listing(self, members: &[(u32, u64)]) -> Self {
            self.listings.borrow_mut().push(
                members
                    .iter()
                    .map(|&(pid, start)| crate::platform::proc::SessionMember { pid, start })
                    .collect(),
            );
            self
        }

        fn zombie(self, pid: u32) -> Self {
            if let Some(entry) = self.table.borrow_mut().get_mut(&pid) {
                entry.2 = true;
            }
            self
        }

        fn signalled(&self) -> bool {
            !self.killed.borrow().is_empty() || !self.stopped.borrow().is_empty()
        }
    }

    #[cfg(unix)]
    impl ProcessView for FakeView {
        type Handle = u32;

        fn own_session(&self) -> Option<u32> {
            Some(self.own)
        }

        fn probe(&self, pid: u32) -> crate::platform::proc::ProcessProbe {
            match self.table.borrow().get(&pid) {
                Some((start, _, _)) => crate::platform::proc::ProcessProbe::Started(*start),
                None => crate::platform::proc::ProcessProbe::Absent,
            }
        }

        fn reissue_proves_empty(&self) -> bool {
            self.reissue_proves_empty
        }

        fn members(&self, sid: u32) -> Option<Vec<crate::platform::proc::SessionMember>> {
            for (pid, now) in self.on_list.borrow_mut().drain(..) {
                match now {
                    Some(now) => self.table.borrow_mut().insert(pid, now),
                    None => self.table.borrow_mut().remove(&pid),
                };
            }
            let mut listings = self.listings.borrow_mut();
            if !listings.is_empty() {
                return Some(listings.remove(0));
            }
            let mut members: Vec<_> = self
                .table
                .borrow()
                .iter()
                .filter(|(_, (_, s, zombie))| *s == sid && !zombie)
                .map(|(&pid, &(start, _, _))| crate::platform::proc::SessionMember { pid, start })
                .collect();
            members.sort_by_key(|m| m.pid);
            Some(members)
        }

        fn open(&self, pid: u32) -> crate::platform::proc::Opened<u32> {
            use crate::platform::proc::Opened;
            if let Some(now) = self.on_open.borrow_mut().remove(&pid) {
                self.table.borrow_mut().insert(pid, now);
            }
            if self.unusable.contains(&pid) {
                return Opened::Unusable;
            }
            if self.table.borrow().contains_key(&pid) {
                Opened::Handle(pid)
            } else {
                Opened::Absent
            }
        }

        fn identity(&self, pid: &u32) -> Option<(u64, u32)> {
            self.table
                .borrow()
                .get(pid)
                .map(|&(start, sid, _)| (start, sid))
        }

        fn stop(&self, pid: &u32) {
            self.stopped.borrow_mut().push(*pid);
        }

        fn kill(&self, pid: &u32) {
            // Killed and reaped at once.
            let start = self
                .table
                .borrow_mut()
                .remove(pid)
                .map(|(start, _, _)| start);
            self.killed.borrow_mut().push((*pid, start.unwrap_or(0)));
        }
    }

    #[cfg(unix)]
    fn leader(pid: u32, start: u64) -> crate::platform::proc::SessionMember {
        crate::platform::proc::SessionMember { pid, start }
    }

    #[cfg(unix)]
    const TICK: Duration = Duration::from_millis(1);

    #[cfg(unix)]
    const LONG: Duration = Duration::from_secs(5);

    /// PRD #1790 stage 3 (audit F4): the sweep never signals the daemon's
    /// own session, and answers an empty session as stopped.
    #[cfg(unix)]
    #[test]
    fn the_sweep_spares_the_daemons_own_session_and_answers_an_empty_one() {
        let own = FakeView::new(&[(1, 10, 1)]);
        assert!(!sweep_session(&own, leader(1, 10), TICK, TICK));
        assert!(!own.signalled());
        let empty = FakeView::new(&[]);
        assert!(sweep_session(&empty, leader(50, 100), TICK, TICK));
        let real = crate::platform::proc::session_of(std::process::id()).expect("a session");
        let start = crate::platform::proc::process_start_of(real).unwrap_or(0);
        assert!(!sweep_session(&OsView, leader(real, start), TICK, TICK));
    }

    /// PRD #1790 stage 3 (audit F4a, round-4 F5): with its leader there, the
    /// sweep stops the leader first, kills the launch's other members, then
    /// the leader, and answers stopped once nothing in the session can run.
    /// A leader that exited and waits to be reaped still names the session:
    /// its members are killed.
    #[cfg(unix)]
    #[test]
    fn the_sweep_stops_the_leader_and_kills_it_last() {
        let view = FakeView::new(&[(50, 100, 50), (60, 120, 50), (61, 130, 50)]);
        assert!(sweep_session(&view, leader(50, 100), LONG, TICK));
        assert_eq!(*view.stopped.borrow(), vec![50]);
        assert_eq!(
            *view.killed.borrow(),
            vec![(60, 120), (61, 130), (50, 100)],
            "the leader last"
        );

        let reaping = FakeView::new(&[(50, 100, 50), (60, 120, 50)]).zombie(50);
        assert!(sweep_session(&reaping, leader(50, 100), LONG, TICK));
        assert_eq!(*reaping.killed.borrow(), vec![(60, 120)]);
    }

    /// PRD #1790 stage 3 (audit F4a, S2, round-4 F5): the sweep signals the
    /// launch's own generation of the session and nothing that merely
    /// carries its numbers.
    #[cfg(unix)]
    #[test]
    fn the_sweep_signals_only_the_launchs_own_processes() {
        // The reviewer's counterexample: leader 50 (start 100) is gone, and
        // 61 (start 950) is in a session numbered 50 again. Not signalled,
        // and not proven stopped.
        let recycled = FakeView::new(&[(61, 950, 50)]);
        assert!(!sweep_session(&recycled, leader(50, 100), TICK, TICK));
        assert!(!recycled.signalled(), "{:?}", recycled.killed);

        // Pid 50 is held by another leader (start 900), running or exited
        // and not reaped: the launch's session emptied before (Linux), so
        // nothing is signalled and it is stopped.
        for zombie in [false, true] {
            let mut reissued = FakeView::new(&[(50, 900, 50), (61, 950, 50)]);
            if zombie {
                reissued = reissued.zombie(50);
            }
            assert!(sweep_session(&reissued, leader(50, 100), TICK, TICK));
            assert!(!reissued.signalled());
            // Where a reissue proves nothing, it is not proven stopped either.
            let mut elsewhere = FakeView::new(&[(50, 900, 50), (61, 950, 50)]);
            elsewhere.reissue_proves_empty = false;
            assert!(!sweep_session(&elsewhere, leader(50, 100), TICK, TICK));
            assert!(!elsewhere.signalled());
        }

        // The leader is reaped and its number reused while the members are
        // listed: that listing is not the launch's, and nothing is signalled.
        let vanishing = FakeView::new(&[(50, 100, 50), (60, 120, 50)]);
        vanishing.on_list.borrow_mut().extend([
            (50, None),
            (60, None),
            (61, Some((950, 50, false))),
        ]);
        assert!(!sweep_session(&vanishing, leader(50, 100), TICK, TICK));
        assert!(
            vanishing.killed.borrow().is_empty(),
            "{:?}",
            vanishing.killed
        );

        // Member 60 exits after the listing and its pid is handed to an
        // unrelated process (start 5000, another session) before the handle
        // is opened: the identity check refuses it.
        let racing = FakeView::new(&[(50, 100, 50)]).listing(&[(50, 100), (60, 120)]);
        racing.on_open.borrow_mut().insert(60, (5000, 7, false));
        assert!(sweep_session(&racing, leader(50, 100), LONG, TICK));
        assert_eq!(*racing.killed.borrow(), vec![(50, 100)], "never 60");
        // Same start, another session: refused too.
        let moved = FakeView::new(&[(50, 100, 50), (60, 120, 77)]).listing(&[(50, 100), (60, 120)]);
        assert!(sweep_session(&moved, leader(50, 100), LONG, TICK));
        assert_eq!(*moved.killed.borrow(), vec![(50, 100)], "never 60");

        // A listed member that started before the leader is not the
        // launch's: never signalled, so the stop is not proven; the leader is
        // killed when the bound passes.
        let older = FakeView::new(&[(50, 100, 50), (40, 90, 50)]);
        assert!(!sweep_session(&older, leader(50, 100), TICK, TICK));
        assert_eq!(*older.killed.borrow(), vec![(50, 100)], "never 40");

        // Seen empty once (the leader exited, not reaped), then the number
        // shows members again: not swept.
        let late = FakeView::new(&[(50, 100, 50)])
            .zombie(50)
            .listing(&[])
            .listing(&[(70, 3000)]);
        assert!(sweep_session(&late, leader(50, 100), LONG, TICK));
        assert!(late.killed.borrow().is_empty());
    }

    /// PRD #1790 stage 3 (round-4 F5): a member no pinned handle can be
    /// opened on (a full descriptor table, a refused call) is never
    /// signalled by its pid: the stop is not proven, and only the stopped
    /// leader is killed once the bound passes.
    #[cfg(unix)]
    #[test]
    fn a_member_with_no_pinned_handle_is_not_signalled() {
        let mut view = FakeView::new(&[(50, 100, 50), (60, 120, 50)]);
        view.unusable.insert(60);
        assert!(!sweep_session(&view, leader(50, 100), TICK, TICK));
        assert_eq!(*view.stopped.borrow(), vec![50]);
        assert_eq!(*view.killed.borrow(), vec![(50, 100)], "never 60");
        // A leader no handle can be had on is not stopped, and the session is
        // still swept while it is there to name it.
        let mut leaderless = FakeView::new(&[(50, 100, 50), (60, 120, 50)]);
        leaderless.unusable.insert(50);
        assert!(!sweep_session(&leaderless, leader(50, 100), TICK, TICK));
        assert!(leaderless.stopped.borrow().is_empty());
        assert_eq!(*leaderless.killed.borrow(), vec![(60, 120)]);
    }

    /// PRD #1790 stage 3 (audit B1): a restored agent is in the conversation
    /// its restore confirmed, from the durable successor link alone, before
    /// its own session report exists; it stops being so once it ended, its
    /// run is gone, or it reports another conversation.
    #[test]
    fn a_confirmed_successor_owns_its_conversation_before_its_own_report() {
        use crate::work_journal::{EndHow, JournalRead, Line};
        const SESSION: &str = "fedcba98-7654-4321-8fab-0123456789ab";
        const OTHER: &str = "abcdef01-7654-4321-8fab-0123456789ab";
        let work = WorkId::parse("w-abcdefgh-0000000000000001").expect("a work id");
        let a = AttemptId::parse("r-old/1").unwrap();
        let b = AttemptId::parse("r-old/2").unwrap();
        let successor = AttemptId::parse("r-live/1").unwrap();
        let line = |run: &str, seq: u64, event: Event| Line {
            v: 1,
            ts_ms: seq,
            run: run.to_string(),
            seq,
            event,
        };
        let edge = Event::AttemptRestored {
            work: work.clone(),
            from: a.clone(),
            to: successor.clone(),
            how: RestoredHow::Resumed,
            session: Some(SESSION.into()),
        };
        let live = |run: &str| run == "r-live";
        let mut read = JournalRead {
            lines: vec![line("r-live", 1, edge)],
            ..JournalRead::default()
        };
        assert!(Sessions::of(&read).in_use(&b, SESSION, &live));
        assert!(!Sessions::of(&read).in_use(&b, OTHER, &live));
        assert!(!Sessions::of(&read).in_use(&b, SESSION, &|_: &str| false));
        // It reports another conversation: it left this one.
        read.lines.push(line(
            "r-live",
            2,
            Event::AttemptSession {
                work: work.clone(),
                attempt: successor.clone(),
                session: OTHER.into(),
                agent: crate::event::AgentType::ClaudeCode,
            },
        ));
        assert!(!Sessions::of(&read).in_use(&b, SESSION, &live));
        assert!(Sessions::of(&read).in_use(&b, OTHER, &live));
        read.lines.push(line(
            "r-live",
            3,
            Event::AttemptEnded {
                work,
                attempt: successor,
                how: EndHow::Exited,
                code: None,
                cause: None,
            },
        ));
        assert!(!Sessions::of(&read).in_use(&b, OTHER, &live));
    }

    /// PRD #1790 stage 3 (audit B2): an agent a failed restore could not be
    /// proven to stop fences the conversations it may be writing to, from
    /// every item, until its launch is proven gone; its attempt ending in
    /// the journal does not lift that.
    #[test]
    fn an_unproven_stop_fences_its_conversation_until_its_launch_is_gone() {
        use crate::work_journal::{EndHow, JournalRead, Line};
        const SESSION: &str = "fedcba98-7654-4321-8fab-0123456789ab";
        let work = WorkId::parse("w-abcdefgh-0000000000000001").expect("a work id");
        let a = AttemptId::parse("r-old/1").unwrap();
        let successor = AttemptId::parse("r-live/1").unwrap();
        let line = |seq: u64, event: Event| Line {
            v: 1,
            ts_ms: seq,
            run: "r-live".into(),
            seq,
            event,
        };
        let read = JournalRead {
            lines: vec![
                line(
                    1,
                    Event::RestoreFailed {
                        work: work.clone(),
                        attempt: a,
                        failure: RestoreFailure::Unknown,
                        shim: None,
                        to: Some(successor.clone()),
                        sessions: vec![SESSION.into()],
                        unknown_session: false,
                    },
                ),
                line(
                    2,
                    Event::AttemptEnded {
                        work: work.clone(),
                        attempt: successor.clone(),
                        how: EndHow::Exited,
                        code: None,
                        cause: None,
                    },
                ),
            ],
            ..JournalRead::default()
        };
        let sessions = Sessions::of(&read);
        let record = |pid: Option<u32>, pid_start: Option<u64>| LaunchRecord {
            v: crate::launch_records::SCHEMA_VERSION,
            work: work.clone(),
            attempt: successor.clone(),
            command: None,
            shell: None,
            agent_type: None,
            cwd: Some("/w".into()),
            cwd_moved_from: None,
            name: None,
            role: None,
            orchestration: None,
            orchestration_id: None,
            ts_ms: 1,
            pid,
            pid_start,
        };
        assert!(
            sessions.fenced(SESSION, None, &[]),
            "no record to check: fenced"
        );
        assert!(
            sessions.fenced(SESSION, None, &[record(None, None)]),
            "a process it cannot check: fenced"
        );
        assert!(!sessions.fenced("another", None, &[record(None, None)]));
        #[cfg(target_os = "linux")]
        {
            let me = std::process::id();
            let start = crate::platform::proc::process_start_of(me);
            assert!(
                sessions.fenced(SESSION, None, &[record(Some(me), start)]),
                "a process still running: fenced"
            );
            assert!(
                !sessions.fenced(SESSION, None, &[record(Some(i32::MAX as u32), Some(1))]),
                "a launch proven gone lifts it"
            );
        }
    }

    /// PRD #1790 stage 3 (round-4 B2): an agent a failed restore could not
    /// be proven to stop, and whose conversation the deck does not know,
    /// fences every conversation of agents of its kind, and only of its kind
    /// (both kinds known), until its launch is proven gone.
    #[test]
    fn a_survivor_in_an_unknown_conversation_fences_its_whole_kind() {
        use crate::event::AgentType;
        use crate::work_journal::{JournalRead, Line};
        let work = WorkId::parse("w-abcdefgh-0000000000000001").expect("a work id");
        let successor = AttemptId::parse("r-live/1").unwrap();
        let read = JournalRead {
            lines: vec![Line {
                v: 1,
                ts_ms: 1,
                run: "r-live".into(),
                seq: 1,
                event: Event::RestoreFailed {
                    work: work.clone(),
                    attempt: AttemptId::parse("r-old/1").unwrap(),
                    failure: RestoreFailure::Refused,
                    shim: None,
                    to: Some(successor.clone()),
                    sessions: vec!["asked-for".into()],
                    unknown_session: true,
                },
            }],
            ..JournalRead::default()
        };
        let sessions = Sessions::of(&read);
        let record = |agent_type: Option<AgentType>| LaunchRecord {
            v: crate::launch_records::SCHEMA_VERSION,
            work: work.clone(),
            attempt: successor.clone(),
            command: None,
            shell: None,
            agent_type,
            cwd: Some("/w".into()),
            cwd_moved_from: None,
            name: None,
            role: None,
            orchestration: None,
            orchestration_id: None,
            ts_ms: 1,
            pid: None,
            pid_start: None,
        };
        let claude = Some(&AgentType::ClaudeCode);
        let claude_record = [record(Some(AgentType::ClaudeCode))];
        assert!(sessions.fenced("any-other", claude, &claude_record));
        assert!(
            !sessions.fenced("any-other", Some(&AgentType::Codex), &claude_record),
            "another kind's conversation is not its"
        );
        assert!(
            sessions.fenced("asked-for", Some(&AgentType::Codex), &claude_record),
            "a conversation it named is fenced whatever the kind"
        );
        assert!(
            sessions.fenced("any-other", claude, &[record(None)]),
            "its kind unknown"
        );
        assert!(
            sessions.fenced("any-other", None, &claude_record),
            "ours unknown"
        );
        assert!(
            sessions.fenced("any-other", claude, &[]),
            "no record to check"
        );
        #[cfg(target_os = "linux")]
        {
            let mut gone = record(Some(AgentType::ClaudeCode));
            gone.pid = Some(i32::MAX as u32);
            gone.pid_start = Some(1);
            assert!(
                !sessions.fenced("any-other", claude, &[gone]),
                "proven gone"
            );
        }
    }

    /// PRD #1790 stage 3 (round-4 B2): what a failed start fences: the
    /// conversations it named, and its kind only when where it went is not
    /// known and a restore of that kind exists.
    #[test]
    fn a_fence_covers_its_kind_only_when_its_conversation_is_unknown() {
        use crate::event::AgentType;
        let known = Fence::new(["s".to_string(), "u".to_string(), "s".to_string()])
            .unknown_unless(true)
            .of(Some(&AgentType::ClaudeCode));
        assert_eq!(known.sessions, ["s", "u"]);
        assert!(!known.unknown);
        let lost = Fence::new(["s".to_string()]).unknown_unless(false);
        assert!(lost.clone().of(Some(&AgentType::ClaudeCode)).unknown);
        assert!(lost.clone().of(None).unknown, "a kind not known");
        assert!(
            !lost.of(Some(&AgentType::OpenCode)).unknown,
            "no restore of its kind exists to fence"
        );
    }

    /// PRD #1790 stage 3 (round-4 B2): what [`keep_fenced`] holds when a
    /// failure could be neither proven stopped nor recorded: every
    /// conversation it names and, for an unknown one, its kind, against a
    /// restore in this daemon ([`InFlight`]) and in another one on the state
    /// directory ([`claim`]).
    #[cfg(unix)]
    #[test]
    fn a_fence_that_could_not_be_recorded_holds_its_conversations_and_kind() {
        use crate::event::AgentType;
        let state = crate::test_temp::tempdir().unwrap();
        // Two kinds no other test fences, so the daemon-wide set this test
        // fills does not refuse another test's restores.
        let fenced_kind = AgentType::Pi;
        let other_kind = AgentType::Devin;
        const KEPT: &str = "a7000000-89ab-4cde-8fab-0123456789ab";
        let attempt = AttemptId::parse("r-fence/1").unwrap();
        let other = AttemptId::parse("r-fence/2").unwrap();
        keep_fenced(
            None,
            Some(state.path()),
            &Fence {
                sessions: vec![KEPT.into()],
                unknown: true,
            },
            Some(&fenced_kind),
        );
        assert_eq!(
            InFlight::take(&other, Some(KEPT), Some(&other_kind)).err(),
            Some(RestoreRefusal::SessionInUse),
            "its conversation, in this daemon"
        );
        assert_eq!(
            claim::take(state.path(), &other, Some(KEPT), Some(&other_kind)).err(),
            Some(RestoreRefusal::SessionInUse),
            "its conversation, from another daemon"
        );
        assert_eq!(
            InFlight::take(&other, Some("elsewhere"), Some(&fenced_kind)).err(),
            Some(RestoreRefusal::SessionInUse),
            "its kind, in this daemon"
        );
        assert_eq!(
            claim::take(state.path(), &other, Some("elsewhere"), Some(&fenced_kind)).err(),
            Some(RestoreRefusal::SessionInUse),
            "its kind, from another daemon"
        );
        // Another kind's restore elsewhere, and a start over, go on.
        assert!(claim::take(state.path(), &other, Some("elsewhere"), Some(&other_kind)).is_ok());
        assert!(claim::take(state.path(), &attempt, None, Some(&fenced_kind)).is_ok());
        assert!(InFlight::take(&attempt, None, Some(&fenced_kind)).is_ok());
    }

    /// PRD #1790 stage 3 (round-4 F1): journal retention keeps a restore's
    /// safety evidence while the successor it names is not proven gone.
    /// Small limits and a compaction of the sealed segment between the first
    /// restore and a second request: a confirmed successor's link (the
    /// `journal/restore/026` shape, its own session report withheld) and an
    /// unproven stop's fence (the `/027` shape, the successor's attempt ended
    /// in the journal) survive with the work that started the successor, and
    /// the second request's read still finds the conversation in use. Once
    /// the successor is proven gone, they go like any finished work.
    #[test]
    fn compaction_keeps_restore_evidence_while_its_successor_may_run() {
        use crate::deck_identity::DeckId;
        use crate::work_journal::{
            EndHow, JournalOptions, Launch, Line, Refs, Retention, WorkJournal,
        };
        const SESSION: &str = "c0000000-89ab-4cde-8fab-0123456789ab";
        const OTHER: &str = "c1000000-89ab-4cde-8fab-0123456789ab";
        const NOW: u64 = 1_800_000_000_000;
        let old = NOW - 40 * 24 * 60 * 60 * 1000;
        let state = crate::test_temp::tempdir().unwrap();
        let home = std::path::PathBuf::from("/home/someone");
        let work = |n: u64| WorkId::parse(&format!("w-abcdefgh-{n:016x}")).unwrap();
        let attempt = |n: u64| AttemptId::new("r-dead", n).unwrap();
        let mut events = Vec::new();
        let opened = |w: WorkId| Event::Opened {
            work: w,
            launch: Launch::Pane,
            parent: None,
            refs: Box::new(Refs::default()),
            workspace: None,
        };
        let started = |w: WorkId, n: u64| Event::AttemptStarted {
            work: w,
            attempt: attempt(n),
            pane: "3".into(),
            agent: n,
            role: None,
        };
        let ended = |w: WorkId, n: u64| Event::AttemptEnded {
            work: w,
            attempt: attempt(n),
            how: EndHow::Exited,
            code: None,
            cause: None,
        };
        // The /026 shape: work 1's attempt 1 was restored as attempt 2, in
        // work 2, which still runs and has reported no session.
        events.extend([
            opened(work(1)),
            started(work(1), 1),
            ended(work(1), 1),
            Event::AttemptRestored {
                work: work(1),
                from: attempt(1),
                to: attempt(2),
                how: RestoredHow::Resumed,
                session: Some(SESSION.into()),
            },
            Event::Closed {
                work: work(1),
                how: CloseHow::Restored,
            },
            opened(work(2)),
            started(work(2), 2),
        ]);
        // The /027 shape: work 3's attempt 3 failed to restore as attempt 4,
        // in work 4, whose stop was not proven; attempt 4 ended.
        events.extend([
            opened(work(3)),
            started(work(3), 3),
            ended(work(3), 3),
            Event::RestoreFailed {
                work: work(3),
                attempt: attempt(3),
                failure: RestoreFailure::SessionMismatch,
                shim: None,
                to: Some(attempt(4)),
                sessions: vec![OTHER.into()],
                unknown_session: false,
            },
            opened(work(4)),
            started(work(4), 4),
            ended(work(4), 4),
        ]);
        // An unrelated finished work, which these limits let go.
        events.extend([
            opened(work(5)),
            Event::Closed {
                work: work(5),
                how: CloseHow::PaneClosed,
            },
        ]);
        let dir = state.path().join(crate::work_journal::DIR_NAME);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut text = String::new();
        for (seq, event) in events.into_iter().enumerate() {
            let seq = seq as u64 + 1;
            let line = Line {
                v: 1,
                ts_ms: old + seq,
                run: "r-dead".into(),
                seq,
                event,
            };
            text.push_str(
                std::str::from_utf8(
                    &crate::work_journal::encode_line_with_home(&line, &home).unwrap(),
                )
                .unwrap(),
            );
            text.push('\n');
        }
        std::fs::write(dir.join("run-r-dead.jsonl"), text).unwrap();
        let journal = WorkJournal::try_open_with(
            state.path(),
            "r-now",
            &DeckId::parse("abcdefgh").unwrap(),
            JournalOptions {
                home,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        let small = Retention {
            max_age: Duration::from_secs(1),
            max_finished_works: 0,
        };
        let kept = |state: &Path| {
            let read = crate::work_journal::read_journal(state).unwrap();
            let mut works: Vec<WorkId> =
                read.lines.iter().map(|l| l.event.work().clone()).collect();
            works.sort();
            works.dedup();
            (read, works)
        };

        let report = journal
            .compact_guarded(NOW, small, &|_: &AttemptId| false)
            .unwrap();
        assert!(
            report.consumed_files > 0,
            "the sealed segment was folded: {report:?}"
        );
        let (read, works) = kept(state.path());
        assert_eq!(
            works,
            vec![work(1), work(2), work(3), work(4)],
            "only the unrelated finished work went"
        );
        let sessions = Sessions::of(&read);
        let second = AttemptId::parse("r-dead/9").unwrap();
        assert!(
            sessions.in_use(&second, SESSION, &|_: &str| true),
            "the confirmed successor still owns its conversation"
        );
        assert!(
            sessions.fenced(OTHER, Some(&crate::event::AgentType::ClaudeCode), &[]),
            "the unproven stop still fences its conversation"
        );

        // Proven gone: the evidence goes with its finished works. Work 2
        // still runs, so it stays.
        journal
            .compact_guarded(NOW, small, &|_: &AttemptId| true)
            .unwrap();
        let (_, works) = kept(state.path());
        assert_eq!(works, vec![work(2)]);
    }

    /// PRD #1790 stage 3 (review R#3, audit S1/F7): a launch of a live run,
    /// not ended, in the same folder is newer work there, whichever spelling
    /// names that folder: the same path, a symlink to it, a path through
    /// `..`. One that ended, of a run that is gone, or in another folder is
    /// not; a live launch whose folder cannot be read counts as in it.
    #[test]
    fn newer_live_work_in_the_same_folder_makes_it_in_use() {
        use crate::work_journal::{EndHow, JournalRead, Line};
        let root = crate::test_temp::tempdir().unwrap();
        let dir = root.path().join("w");
        let other_dir = root.path().join("other");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        let w = dir.to_str().unwrap().to_string();
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
        let base = vec![record(&target, &w), record(&ended, &w)];
        assert!(!sessions.folder_in_use(&target, &w, &base, &live));
        let with = |cwd: &str| {
            let mut launches = base.clone();
            launches.push(record(&newer, cwd));
            launches
        };
        assert!(!sessions.folder_in_use(&target, &w, &with(other_dir.to_str().unwrap()), &live));
        assert!(sessions.folder_in_use(&target, &w, &with(&w), &live));
        assert!(!sessions.folder_in_use(&target, &w, &with(&w), &|_: &str| false));
        let dotted = format!("{w}/sub/..");
        assert!(
            sessions.folder_in_use(&target, &w, &with(&dotted), &live),
            "a `..` spelling"
        );
        #[cfg(unix)]
        {
            let link = root.path().join("link");
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            assert!(
                sessions.folder_in_use(&target, &w, &with(link.to_str().unwrap()), &live),
                "a symlink to the folder"
            );
        }
        let gone = root.path().join("gone");
        assert!(
            sessions.folder_in_use(&target, &w, &with(gone.to_str().unwrap()), &live),
            "a live launch's unreadable folder counts as this one"
        );
        // Round-4 S1: a file is no folder to tell this one apart from.
        let file = root.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(
            sessions.folder_in_use(&target, &w, &with(file.to_str().unwrap()), &live),
            "a live launch whose recorded folder is not a directory counts as this one"
        );
        // Round-4 S1: a live launch with no folder recorded, its prepared
        // folder moved or never resolved, counts as in this one too.
        for moved in [None, Some("/somewhere/else".to_string())] {
            let mut launches = base.clone();
            let mut unknown = record(&newer, &w);
            unknown.cwd = None;
            unknown.cwd_moved_from = moved;
            launches.push(unknown);
            assert!(
                sessions.folder_in_use(&target, &w, &launches, &live),
                "a live launch with no recorded folder counts as this one"
            );
            assert!(!sessions.folder_in_use(&target, &w, &launches, &|_: &str| false));
        }
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
