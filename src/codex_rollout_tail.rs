//! Read a Codex session log for the outcome of a running turn (issues #714,
//! #1359).
//!
//! Codex reports a failed turn through no hook at all: an errored turn runs no
//! `Stop` hook, and there is no failure hook. What it does do is write the
//! failure into its rollout JSONL — a `task_complete` record carrying an
//! `error`, whose `codex_error_info` is `usage_limit_exceeded` for a provider
//! usage limit (reported as a quota block) and something else for every other
//! failure (reported as an error). So the daemon reads that file, and only
//! while a turn is running.
//!
//! * **Arming.** Codex's own `SessionStart` and `UserPromptSubmit` hooks carry
//!   the rollout's `transcript_path`, and `UserPromptSubmit` the `turn_id`. The
//!   hook CLI forwards them as [`CODEX_TRANSCRIPT_PATH_METADATA_KEY`] /
//!   [`CODEX_TURN_ID_METADATA_KEY`], and the daemon arms a watch for that turn —
//!   only for an event whose pane and agent name the pane's LIVE owner, so a
//!   payload can never make the daemon read a file on another pane's behalf.
//! * **Reading.** [`CodexRolloutTailers::tick`] polls every armed tailer every
//!   [`POLL_INTERVAL`] rather than subscribing to file events: the daemon runs
//!   on Linux, macOS and Windows, the file is append-only, and two seconds is
//!   noise against a block that lasts hours. Each tick reads at most
//!   [`MAX_READ_PER_TICK`] per tailer, and a watch starts
//!   [`BACK_WINDOW`] before the end of the file, which covers the race where
//!   Codex writes the failure before the daemon handles the hook. The pure
//!   record classifier is [`crate::quota_signals::CodexTurnWatch`].
//! * **Disarming.** On the watched turn's `task_complete` (with or without an
//!   error), a Codex `Stop` naming that turn and carrying its final reply, or
//!   a newer arm. A `Stop` without the reply leaves the turn armed, since its
//!   `task_complete` is then the only report of the reply (PRD #1497). That is
//!   every `Stop` a hook CLI older than PRD #1497 sends, since it attaches no
//!   reply, so under such a hook each turn is normally disarmed by its
//!   `task_complete`. A watch whose `task_complete` does not come is retired,
//!   delivering nothing (PRD #1497 re-audit R3), by whichever comes first of:
//!   - the watched turn's `turn_aborted` record (an interrupted turn);
//!   - [`DRAIN_AFTER_STOP`] after a reply-less `Stop` naming the watched turn
//!     ([`ArmCommand::StoppedWithoutReply`]): the turn is no longer running,
//!     so only its `task_complete` is still awaited. A turn with no `Stop` yet
//!     is never timed out, however long it runs;
//!   - the rollout's path no longer naming the file the watch holds open —
//!     unlinked, or replaced by another file — noticed by the first tick
//!     after it happens (Unix compares device and inode; elsewhere only a path
//!     that no longer resolves is noticed, and a replaced one is left to the
//!     other bounds);
//!   - the agent's next arm for another turn, a new rollout path, or the
//!     agent's exit.
//!
//!   Retiring for either of the first two timed reasons (the drain running
//!   out, the path no longer naming the file) is not immediate: the watch
//!   first reads what the held file already contains, up to its length at the
//!   moment retirement was decided, at most [`MAX_READ_PER_TICK`] per tick,
//!   and is closed once it reaches that length (re-audit R3.1). So a
//!   completion already written when retirement is decided is still read,
//!   however much lies before it, while bytes appended after that moment are
//!   not waited for. A line still being written at that moment — its newline
//!   past the boundary — is the one record this gives up. A held file found
//!   shorter than that length — truncated since, whether below or above the
//!   point already read, including a truncation that cuts a read short — is
//!   closed at once, with nothing more read from it.
//!
//!   The `Stop` and the arm of one turn come from separate hook processes, so
//!   they may reach the daemon in either order. Each agent therefore keeps a
//!   short record of its turns that have ended ([`MAX_ENDED_TURNS`] per agent,
//!   dropped with the agent): a reply-less `Stop` that arrives before its
//!   turn's arm starts that turn's drain when the arm comes, a repeated arm of
//!   the watched turn changes nothing, and an arm of a turn already over
//!   (completed, aborted, disarmed or retired) opens no new watch
//!   (re-audit R3.2). That reordering horizon is [`MAX_ENDED_TURNS`] ended
//!   turns per agent: an arm arriving after more of that agent's turns have
//!   ended than the record holds finds no record of its turn and is treated
//!   as a new turn — it opens a watch nothing will drain, held at most until the
//!   agent's next arm or its exit, and it replaces whatever watch the agent
//!   then had, a newer turn's included. See [`MAX_ENDED_TURNS`].
//!
//!   A turn that never reaches a `Stop` and writes no record (a Codex that
//!   crashed mid-turn) is bounded by the last of these alone: there is one
//!   tailer per agent and one watch per tailer, so an agent holds at most one
//!   open rollout. Disarming closes the file and keeps only its path, so a
//!   disarmed Codex pane holds no file descriptor; the next arm re-opens it,
//!   validating it again. A tailer is dropped when its agent is no longer the
//!   live owner of its pane, and the whole set when the daemon's monitor task
//!   is aborted.
//! * **Path safety**, checked every time a path is (re)opened
//!   ([`open_rollout`]): absolute, no `..`, canonicalizes, the canonical file
//!   name is `rollout-*.jsonl`, opened read-only (Unix: non-blocking and without
//!   following a final symlink), and the opened file is a regular file (Unix:
//!   owned by the daemon's uid). The file handle is then held and reused for
//!   the armed turn, so a swap of the path during it has nothing to race. A
//!   refused path is logged at `debug` and never retried.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use crate::quota_signals::{CodexLineOutcome, CodexTurnWatch, FailureOutcome};

/// `AgentEvent.metadata` key on a Codex `SessionStart` / `UserPromptSubmit`
/// event carrying the hook payload's `transcript_path` — the rollout to read.
pub const CODEX_TRANSCRIPT_PATH_METADATA_KEY: &str = "codex_transcript_path";

/// `AgentEvent.metadata` key on a Codex `UserPromptSubmit` event carrying the
/// hook payload's `turn_id` — the turn to watch.
pub const CODEX_TURN_ID_METADATA_KEY: &str = "codex_turn_id";

/// The longest rollout path [`CODEX_TRANSCRIPT_PATH_METADATA_KEY`] may carry;
/// a longer one is neither forwarded by the hook CLI nor admitted by the daemon.
pub const MAX_METADATA_BYTES: usize = 4096;

/// The longest turn id [`CODEX_TURN_ID_METADATA_KEY`] may carry. Codex's turn
/// ids are UUIDs; this leaves room for any other shape without letting a raw
/// hook frame park megabytes in the arm queue.
pub const MAX_TURN_ID_BYTES: usize = 256;

/// Whether `path` may be carried as [`CODEX_TRANSCRIPT_PATH_METADATA_KEY`]:
/// non-empty and at most [`MAX_METADATA_BYTES`]. Checked by the hook CLI before
/// forwarding and again by the daemon's admission of every raw frame, since the
/// hook socket accepts events from any same-uid producer, not only the CLI.
pub fn admissible_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= MAX_METADATA_BYTES
}

/// Whether `turn_id` may be carried as [`CODEX_TURN_ID_METADATA_KEY`]:
/// non-empty and at most [`MAX_TURN_ID_BYTES`]. See [`admissible_path`].
pub fn admissible_turn_id(turn_id: &str) -> bool {
    !turn_id.is_empty() && turn_id.len() <= MAX_TURN_ID_BYTES
}

/// How often the daemon polls its armed tailers.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long a watch waits for its turn's `task_complete` after a reply-less
/// `Stop` named that turn ([`ArmCommand::StoppedWithoutReply`]) before it is
/// retired with nothing delivered (PRD #1497 re-audit R3).
///
/// Measured on the development machine on 2026-10-09 over 312 Codex turns
/// whose `Stop` reached the daemon (`deck.log`'s `Received event … Idle`
/// against the `task_complete` record's own `timestamp` in its rollout): the
/// record was written a median of 3 ms after the daemon received the `Stop`,
/// 1.7 s at the 99th percentile and 2.7 s at the most. Thirty seconds is about
/// ten times that maximum. The drain bounds only the wait for a record not yet
/// WRITTEN: when it runs out, what the file already holds is still read, up to
/// its length at that moment, before the watch closes (re-audit R3.1), so an
/// unread backlog does not race it however many ticks it takes.
pub const DRAIN_AFTER_STOP: Duration = Duration::from_secs(30);

/// The most one tailer reads in one tick.
pub const MAX_READ_PER_TICK: u64 = 1024 * 1024;

/// How far before the end of the file a newly armed watch starts reading.
pub const BACK_WINDOW: u64 = 256 * 1024;

/// The longest line buffered across reads; a longer one is skipped through its
/// newline without being parsed. The records that matter are small; only
/// `response_item` lines with large tool output get this long.
pub const MAX_LINE_BYTES: usize = 256 * 1024;

/// Bound on the NUMBER of queued, not-yet-applied arm commands — fed from hook
/// events, so the queue may not grow without limit.
const MAX_PENDING: usize = 4096;

/// Bound on the total BYTES of queued arm commands ([`ArmCommand::byte_len`]).
/// Admission already bounds the path and turn id, but the count bound alone
/// would still let a burst of maximal commands hold tens of megabytes until the
/// next poll; this caps it regardless of what the strings carry.
const MAX_PENDING_BYTES: usize = 1024 * 1024;

/// How many ended turns ([`EndedTurn`]) are remembered per agent — the
/// tailer's reordering horizon. The out-of-order arrivals they exist for are a
/// turn's `Stop` overtaking that same turn's arm; past this many the oldest
/// is forgotten.
///
/// An arm delayed past that horizon — behind this many later turn ends of the
/// same agent — finds no record of its turn and is treated as a new turn: it
/// opens a watch that no drain bounds, held (one file descriptor) at most until the
/// agent's next arm or its exit, and it replaces the watch the agent had at
/// that moment, which may be a newer turn's. That is accepted rather than
/// ordered: it needs one arm of a sequential Codex session overtaken by four
/// of that session's later turn ends, and its cost is the residual already
/// accepted for a Codex that crashes before its `Stop`.
pub const MAX_ENDED_TURNS: usize = 4;

/// Bounds on the remembered refused paths, by count and by total bytes. Past
/// either the set is cleared, which costs at most one re-validation per path.
const MAX_REFUSED: usize = 1024;
const MAX_REFUSED_BYTES: usize = 1024 * 1024;

/// What the hook loop asks of the tailers. Queued by the hook loop
/// ([`CodexRolloutArms::push`]) and applied by the monitor task at its next
/// tick, so the hook loop never touches a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArmCommand {
    /// A Codex event that named a rollout and/or a turn.
    Arm(ArmRequest),
    /// A Codex `Stop` for `turn_id` that carried the turn's final reply: that
    /// turn ended normally and its reply is reported. A watch for any other
    /// turn is left armed; an arm of `turn_id` arriving afterwards opens none.
    Disarm {
        pane_id: String,
        agent_id: String,
        turn_id: String,
    },
    /// A Codex `Stop` for `turn_id` that carried no reply: the turn has
    /// stopped running, so its watch keeps waiting for the `task_complete`
    /// only for [`DRAIN_AFTER_STOP`] (PRD #1497 re-audit R3), counted from
    /// this `Stop` even when the turn's arm arrives after it. A watch for any
    /// other turn is left as it is.
    StoppedWithoutReply {
        pane_id: String,
        agent_id: String,
        turn_id: String,
    },
}

impl ArmCommand {
    /// The bytes this command's strings hold — what the queue's byte bound
    /// counts.
    pub fn byte_len(&self) -> usize {
        match self {
            ArmCommand::Disarm {
                pane_id,
                agent_id,
                turn_id,
            }
            | ArmCommand::StoppedWithoutReply {
                pane_id,
                agent_id,
                turn_id,
            } => pane_id.len() + agent_id.len() + turn_id.len(),
            ArmCommand::Arm(req) => {
                req.pane_id.len()
                    + req.agent_id.len()
                    + req.session_id.len()
                    + req.path.as_ref().map_or(0, String::len)
                    + req.turn_id.as_ref().map_or(0, String::len)
            }
        }
    }
}

/// One arm, from a Codex `SessionStart` (path only) or `UserPromptSubmit`
/// (path and turn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmRequest {
    pub pane_id: String,
    pub agent_id: String,
    /// The arming hook's session id, which the failure event is filed under.
    pub session_id: String,
    /// The rollout path, when this event carried one.
    pub path: Option<String>,
    /// The turn to watch, when this event carried one.
    pub turn_id: Option<String>,
}

/// A failed turn read from a rollout, for the daemon to report: a quota block
/// or, for any other failure, an error (issue #1359).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTurnFailure {
    pub pane_id: String,
    pub agent_id: String,
    pub session_id: String,
    /// The failed turn — what [`CodexRolloutArms::supersedes`] compares a
    /// newer arm against.
    pub turn_id: String,
    pub outcome: FailureOutcome,
    /// The `task_complete` error message, unscrubbed.
    pub message: Option<String>,
}

/// PRD #1497: a watched turn's final reply read from its rollout's
/// `task_complete`, for the daemon to publish to reading subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTurnReply {
    pub pane_id: String,
    pub agent_id: String,
    pub reply: crate::daemon_protocol::FinalReply,
}

/// The queue between the hook loop and the monitor task.
#[derive(Debug, Default)]
pub struct CodexRolloutArms {
    pending: std::sync::Mutex<PendingArms>,
}

#[derive(Debug, Default)]
struct PendingArms {
    commands: VecDeque<ArmCommand>,
    /// The sum of [`ArmCommand::byte_len`] over `commands`.
    bytes: usize,
}

impl CodexRolloutArms {
    /// Queue `command`. Past [`MAX_PENDING`] commands or [`MAX_PENDING_BYTES`]
    /// the oldest queued commands are dropped — a newer arm for the same agent
    /// supersedes them anyway. A command larger than the byte bound on its own
    /// is dropped instead of queued.
    pub fn push(&self, command: ArmCommand) {
        let size = command.byte_len();
        if size > MAX_PENDING_BYTES {
            tracing::debug!(
                bytes = size,
                "codex rollout: dropped an arm command larger than the queue's byte bound"
            );
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        while pending.commands.len() >= MAX_PENDING || pending.bytes + size > MAX_PENDING_BYTES {
            let Some(oldest) = pending.commands.pop_front() else {
                break;
            };
            pending.bytes -= oldest.byte_len();
        }
        pending.bytes += size;
        pending.commands.push_back(command);
    }

    /// Take every queued command, oldest first.
    pub fn drain(&self) -> Vec<ArmCommand> {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.bytes = 0;
        pending.commands.drain(..).collect()
    }

    /// Whether a queued, not-yet-applied arm names a turn of `agent_id`'s other
    /// than `turn_id` — so a failure the monitor found for `turn_id` belongs
    /// to a turn Codex has already moved past (issue #1359). The hook loop
    /// queues a prompt's arm BEFORE it applies the prompt to the card, so a
    /// prompt already on the card is in this queue until the next tick drains
    /// it.
    pub fn supersedes(&self, agent_id: &str, turn_id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .commands
            .iter()
            .any(|command| match command {
                ArmCommand::Arm(req) => {
                    req.agent_id == agent_id && req.turn_id.as_deref().is_some_and(|t| t != turn_id)
                }
                ArmCommand::Disarm { .. } | ArmCommand::StoppedWithoutReply { .. } => false,
            })
    }

    /// The total bytes currently queued. For tests and diagnostics.
    pub fn queued_bytes(&self) -> usize {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).bytes
    }
}

/// The rollout file a tailer holds open, and where it has read to.
#[derive(Debug)]
struct OpenRollout {
    file: File,
    offset: u64,
    /// The start of a line whose newline has not been read yet.
    partial: Vec<u8>,
    /// Skipping to the next newline: the read started mid-line, or a line
    /// outgrew [`MAX_LINE_BYTES`].
    discarding: bool,
}

#[derive(Debug)]
struct Tailer {
    pane_id: String,
    session_id: String,
    path: String,
    /// The open rollout — `Some` only while a turn is armed and the path has
    /// been validated for it; disarming closes it.
    open: Option<OpenRollout>,
    /// The watched turn; `None` when no turn is armed, and then the file is
    /// not read at all.
    watch: Option<CodexTurnWatch>,
    /// Start the next read [`BACK_WINDOW`] before the end of the file.
    rewind: bool,
    /// When a reply-less `Stop` for the watched turn was applied — the start
    /// of its [`DRAIN_AFTER_STOP`]. `None` while the turn has reached no such
    /// `Stop`; set from the agent's [`EndedTurn`] record by a new watch, and
    /// kept by a repeated arm of the same turn.
    stopped_at: Option<Instant>,
    /// Retirement has been decided: the held file is read up to this offset —
    /// its length when retirement was decided — and the watch is then
    /// closed (re-audit R3.1).
    retiring: Option<u64>,
}

impl Tailer {
    /// End the watch and close the file, keeping only the path.
    fn close_watch(&mut self) {
        self.watch = None;
        self.open = None;
        self.stopped_at = None;
        self.retiring = None;
    }
}

/// How a turn of an agent's ended, as far as the tailer has been told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnEnd {
    /// A reply-less `Stop` named it at this instant; its `task_complete` may
    /// still come, for [`DRAIN_AFTER_STOP`].
    Stopped(Instant),
    /// It is over: completed, aborted, disarmed by a reply-bearing `Stop`, or
    /// retired. A later arm of it opens no watch.
    Over,
}

/// One ended turn of an agent's.
#[derive(Debug)]
struct EndedTurn {
    turn_id: String,
    end: TurnEnd,
}

/// An agent's most recent ended turns, newest last, at most
/// [`MAX_ENDED_TURNS`]. Kept apart from the agent's [`Tailer`] because a
/// `Stop` may come before the arm that would create it; dropped once the
/// agent is no longer its pane's live owner. The registry `agent_id` keying it
/// is per spawn, so a respawned agent starts with none.
#[derive(Debug)]
struct EndedTurns {
    pane_id: String,
    turns: VecDeque<EndedTurn>,
}

impl EndedTurns {
    fn get(&self, turn_id: &str) -> Option<TurnEnd> {
        self.turns
            .iter()
            .find(|t| t.turn_id == turn_id)
            .map(|t| t.end)
    }
}

/// Every Codex agent's tailer, keyed by the registry `agent_id` (so a respawn
/// in the same pane is a new key). Owned by the daemon's monitor task.
#[derive(Debug, Default)]
pub struct CodexRolloutTailers {
    tailers: HashMap<String, Tailer>,
    refused: HashSet<String>,
    /// The sum of the lengths of `refused`.
    refused_bytes: usize,
    /// PRD #1497: replies found by [`Self::tick`], until
    /// [`Self::take_replies`]. At most one per armed tailer per tick.
    replies: Vec<CodexTurnReply>,
    /// Each agent's recently ended turns (re-audit R3.2).
    ended: HashMap<String, EndedTurns>,
}

/// Record that `agent_id`'s `turn_id` ended as `end`. A turn already `Over`
/// stays over, and a repeated reply-less `Stop` keeps the first one's instant,
/// so neither a duplicate nor a late command can extend a drain.
fn note_end(
    ended: &mut HashMap<String, EndedTurns>,
    pane_id: &str,
    agent_id: &str,
    turn_id: &str,
    end: TurnEnd,
) {
    let entry = ended
        .entry(agent_id.to_owned())
        .or_insert_with(|| EndedTurns {
            pane_id: pane_id.to_owned(),
            turns: VecDeque::new(),
        });
    if entry.pane_id != pane_id {
        // Not this agent's pane: the record would never be pruned with it.
        return;
    }
    if let Some(turn) = entry.turns.iter_mut().find(|t| t.turn_id == turn_id) {
        if end == TurnEnd::Over {
            turn.end = TurnEnd::Over;
        }
        return;
    }
    entry.turns.push_back(EndedTurn {
        turn_id: turn_id.to_owned(),
        end,
    });
    while entry.turns.len() > MAX_ENDED_TURNS {
        entry.turns.pop_front();
    }
}

impl CodexRolloutTailers {
    /// Apply one command from the hook loop.
    pub fn apply(&mut self, command: ArmCommand) {
        match command {
            ArmCommand::Disarm {
                pane_id,
                agent_id,
                turn_id,
            } => {
                note_end(
                    &mut self.ended,
                    &pane_id,
                    &agent_id,
                    &turn_id,
                    TurnEnd::Over,
                );
                if let Some(tailer) = self.tailers.get_mut(&agent_id)
                    && tailer
                        .watch
                        .as_ref()
                        .is_some_and(|w| w.turn_id() == turn_id)
                {
                    tailer.close_watch();
                }
            }
            ArmCommand::StoppedWithoutReply {
                pane_id,
                agent_id,
                turn_id,
            } => {
                // Recorded whether or not the turn is armed yet: its arm may
                // still be on its way (re-audit R3.2).
                note_end(
                    &mut self.ended,
                    &pane_id,
                    &agent_id,
                    &turn_id,
                    TurnEnd::Stopped(Instant::now()),
                );
                if let Some(tailer) = self.tailers.get_mut(&agent_id)
                    && tailer
                        .watch
                        .as_ref()
                        .is_some_and(|w| w.turn_id() == turn_id)
                {
                    let at = match self.ended.get(&agent_id).and_then(|e| e.get(&turn_id)) {
                        Some(TurnEnd::Stopped(at)) => at,
                        _ => Instant::now(),
                    };
                    // The first such Stop starts the drain; a repeat does not
                    // extend it.
                    tailer.stopped_at.get_or_insert(at);
                }
            }
            ArmCommand::Arm(mut req) => {
                let ended_as = req.turn_id.as_deref().and_then(|turn| {
                    self.ended
                        .get(&req.agent_id)
                        .filter(|e| e.pane_id == req.pane_id)
                        .and_then(|e| e.get(turn))
                });
                if ended_as == Some(TurnEnd::Over) {
                    // That turn is over: a late or repeated arm of it must not
                    // open a watch nothing will ever end (re-audit R3.2). The
                    // path and session it carries still apply.
                    req.turn_id = None;
                }
                let existing = self
                    .tailers
                    .get(&req.agent_id)
                    .filter(|t| t.pane_id == req.pane_id);
                let path = match (&req.path, existing) {
                    (Some(path), _) => path.clone(),
                    (None, Some(t)) => t.path.clone(),
                    (None, None) => return,
                };
                let tailer = match self.tailers.remove(&req.agent_id) {
                    Some(t) if t.pane_id == req.pane_id && t.path == path => t,
                    _ => Tailer {
                        pane_id: req.pane_id.clone(),
                        session_id: req.session_id.clone(),
                        path,
                        open: None,
                        watch: None,
                        rewind: false,
                        stopped_at: None,
                        retiring: None,
                    },
                };
                let mut tailer = tailer;
                tailer.session_id = req.session_id;
                if let Some(turn) = req.turn_id {
                    let already_watching =
                        tailer.watch.as_ref().is_some_and(|w| w.turn_id() == turn);
                    // A repeated arm of the watched turn changes nothing: its
                    // drain, its read position and any retirement in progress
                    // are kept (re-audit R3.2).
                    if !already_watching {
                        tailer.watch = Some(CodexTurnWatch::new(turn));
                        tailer.rewind = true;
                        tailer.retiring = None;
                        tailer.stopped_at = match ended_as {
                            Some(TurnEnd::Stopped(at)) => Some(at),
                            _ => None,
                        };
                    }
                }
                self.tailers.insert(req.agent_id, tailer);
            }
        }
    }

    /// PRD #1497: the final replies the ticks so far found in watched turns'
    /// `task_complete` records — successful and failed — oldest first.
    pub fn take_replies(&mut self) -> Vec<CodexTurnReply> {
        std::mem::take(&mut self.replies)
    }

    /// Whether `agent_id` has a turn armed. For tests and diagnostics.
    pub fn is_armed(&self, agent_id: &str) -> bool {
        self.tailers
            .get(agent_id)
            .is_some_and(|t| t.watch.is_some())
    }

    /// Whether `agent_id`'s tailer holds its rollout open. For tests and
    /// diagnostics.
    pub fn holds_file(&self, agent_id: &str) -> bool {
        self.tailers.get(agent_id).is_some_and(|t| t.open.is_some())
    }

    /// Whether any tailer exists for `agent_id`. For tests and diagnostics.
    pub fn has_tailer(&self, agent_id: &str) -> bool {
        self.tailers.contains_key(agent_id)
    }

    /// One poll: drop every tailer whose `(pane_id, agent_id)` is no longer a
    /// live owner, then read what each armed tailer's rollout has gained since
    /// the last tick and return the failed turns found. A watch still armed
    /// after its read is then marked for retirement if its rollout's path no
    /// longer names the open file, or if its [`DRAIN_AFTER_STOP`] has run
    /// out, and closed once its reads reach the file's length at that moment.
    /// Does file I/O; the daemon runs it on a blocking thread, outside every
    /// lock of its own.
    pub fn tick(&mut self, is_live_owner: impl Fn(&str, &str) -> bool) -> Vec<CodexTurnFailure> {
        self.tick_at(Instant::now(), is_live_owner)
    }

    /// [`Self::tick`] with `now` as the current time, which the drain after a
    /// reply-less `Stop` is measured against.
    pub fn tick_at(
        &mut self,
        now: Instant,
        is_live_owner: impl Fn(&str, &str) -> bool,
    ) -> Vec<CodexTurnFailure> {
        self.tailers
            .retain(|agent_id, tailer| is_live_owner(&tailer.pane_id, agent_id));
        self.ended
            .retain(|agent_id, ended| is_live_owner(&ended.pane_id, agent_id));
        let mut failures = Vec::new();
        let mut over: Vec<(String, String, String)> = Vec::new();
        for (agent_id, tailer) in self.tailers.iter_mut() {
            if tailer.watch.is_none() {
                continue;
            }
            if tailer.open.is_none() {
                if self.refused.contains(&tailer.path) {
                    tailer.watch = None;
                    continue;
                }
                match open_rollout(&tailer.path) {
                    Ok(file) => {
                        tailer.open = Some(OpenRollout {
                            file,
                            offset: 0,
                            partial: Vec::new(),
                            discarding: false,
                        });
                    }
                    Err(why) => {
                        tracing::debug!(
                            agent_id = %agent_id,
                            path = %tailer.path.escape_debug(),
                            reason = why,
                            "codex rollout: refused a transcript path; not retried"
                        );
                        if self.refused.len() >= MAX_REFUSED
                            || self.refused_bytes + tailer.path.len() > MAX_REFUSED_BYTES
                        {
                            self.refused.clear();
                            self.refused_bytes = 0;
                        }
                        if tailer.path.len() <= MAX_REFUSED_BYTES
                            && self.refused.insert(tailer.path.clone())
                        {
                            self.refused_bytes += tailer.path.len();
                        }
                        tailer.watch = None;
                        continue;
                    }
                }
            }
            let turn_id = tailer
                .watch
                .as_ref()
                .map_or_else(String::new, |w| w.turn_id().to_owned());
            let (found, reply) = read_tailer(tailer);
            if let Some(reply) = reply {
                self.replies.push(CodexTurnReply {
                    pane_id: tailer.pane_id.clone(),
                    agent_id: agent_id.clone(),
                    reply,
                });
            }
            if let Some((outcome, message)) = found {
                failures.push(CodexTurnFailure {
                    pane_id: tailer.pane_id.clone(),
                    agent_id: agent_id.clone(),
                    session_id: tailer.session_id.clone(),
                    turn_id: turn_id.clone(),
                    outcome,
                    message,
                });
            }
            if tailer.watch.is_none() {
                // The read reached the turn's own task_complete or
                // turn_aborted.
                tailer.stopped_at = None;
                tailer.retiring = None;
                over.push((tailer.pane_id.clone(), agent_id.clone(), turn_id));
                continue;
            }
            if tailer.retiring.is_none() {
                let reason = if tailer.stopped_at.is_some_and(|stopped| {
                    now.saturating_duration_since(stopped) >= DRAIN_AFTER_STOP
                }) {
                    Some("no task_complete within the drain after a reply-less Stop")
                } else if tailer
                    .open
                    .as_ref()
                    .is_some_and(|open| !path_still_names(&tailer.path, &open.file))
                {
                    Some("the rollout was unlinked or replaced")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    // Read what the held file holds NOW before closing it,
                    // however many ticks that takes; what is appended later
                    // is not waited for (re-audit R3.1).
                    let boundary = tailer
                        .open
                        .as_ref()
                        .and_then(|open| open.file.metadata().ok())
                        .map_or(0, |meta| meta.len());
                    tracing::debug!(
                        agent_id = %agent_id,
                        reason,
                        boundary,
                        "codex rollout: retiring a watch once what its file holds is read"
                    );
                    tailer.retiring = Some(boundary);
                }
            }
            if let Some(boundary) = tailer.retiring
                && tailer
                    .open
                    .as_ref()
                    .is_none_or(|open| open.offset >= boundary)
            {
                tracing::debug!(
                    agent_id = %agent_id,
                    "codex rollout: retired a watch without a task_complete"
                );
                tailer.close_watch();
                over.push((tailer.pane_id.clone(), agent_id.clone(), turn_id));
            }
        }
        for (pane_id, agent_id, turn_id) in over {
            note_end(
                &mut self.ended,
                &pane_id,
                &agent_id,
                &turn_id,
                TurnEnd::Over,
            );
        }
        failures
    }
}

type FoundFailure = (FailureOutcome, Option<String>);

/// Whether a read starting at `start` begins part-way through a line, so its
/// first segment is the tail of a record and must be dropped. False at the
/// start of the file and when the byte before `start` is a newline — a window
/// that opens exactly on a record boundary keeps that record, which may be the
/// very `task_complete` it was opened to catch. A byte that cannot be read
/// answers true: dropping one line costs less than parsing half of one.
fn opens_mid_record(file: &File, start: u64) -> bool {
    if start == 0 {
        return false;
    }
    let mut file = file;
    let mut byte = [0u8; 1];
    !(file.seek(SeekFrom::Start(start - 1)).is_ok()
        && file.read_exact(&mut byte).is_ok()
        && byte[0] == b'\n')
}

/// Read what `tailer`'s rollout gained since its last read, feeding each
/// complete line to its watch. Returns the failure the watch found, if any,
/// and the watched turn's final reply once its `task_complete` is read.
fn read_tailer(
    tailer: &mut Tailer,
) -> (
    Option<FoundFailure>,
    Option<crate::daemon_protocol::FinalReply>,
) {
    let Some(open) = tailer.open.as_mut() else {
        return (None, None);
    };
    let len = match open.file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => {
            tailer.open = None;
            return (None, None);
        }
    };
    // A retiring watch reads only up to the length its file had when
    // retirement was decided. A file now shorter than that was truncated
    // since, so the boundary can never be reached and what was meant to be
    // read is at least partly gone: the watch is closed at once, wherever
    // the new length falls relative to the read position (re-audit R3.1).
    let end = match tailer.retiring {
        Some(boundary) if len < boundary => {
            tailer.open = None;
            return (None, None);
        }
        Some(boundary) => boundary,
        None => len,
    };
    if tailer.retiring.is_none() && (len < open.offset || tailer.rewind) {
        // Truncated (or a fresh watch): start one window back from the end.
        let start = len.saturating_sub(BACK_WINDOW);
        if len < open.offset || start > open.offset || tailer.rewind {
            open.offset = start;
            open.partial.clear();
            open.discarding = opens_mid_record(&open.file, start);
        }
        tailer.rewind = false;
    }
    let want = end.saturating_sub(open.offset).min(MAX_READ_PER_TICK);
    if want == 0 {
        return (None, None);
    }
    let mut buf = Vec::with_capacity(want as usize);
    if open.file.seek(SeekFrom::Start(open.offset)).is_err()
        || (&open.file).take(want).read_to_end(&mut buf).is_err()
    {
        tailer.open = None;
        return (None, None);
    }
    open.offset += buf.len() as u64;
    // A read that came back short of `want` hit the end of the file below the
    // length just checked: the file was truncated between the check and the
    // read. For a retiring watch that is the truncation above, so what was
    // read is still fed to the watch and the file is then closed.
    let shrank_while_retiring = tailer.retiring.is_some() && (buf.len() as u64) < want;

    let Some(watch) = tailer.watch.as_mut() else {
        return (None, None);
    };
    let mut found = None;
    let mut ended = false;
    let mut start = 0;
    for (i, _) in buf.iter().enumerate().filter(|(_, b)| **b == b'\n') {
        let segment = &buf[start..i];
        start = i + 1;
        if open.discarding {
            open.discarding = false;
            open.partial.clear();
            continue;
        }
        if ended {
            continue;
        }
        if open.partial.len() + segment.len() > MAX_LINE_BYTES {
            open.partial.clear();
            continue;
        }
        let outcome = if open.partial.is_empty() {
            watch.observe_line(segment)
        } else {
            open.partial.extend_from_slice(segment);
            let line = std::mem::take(&mut open.partial);
            watch.observe_line(&line)
        };
        match outcome {
            CodexLineOutcome::Nothing => {}
            CodexLineOutcome::TurnEnded | CodexLineOutcome::TurnAborted => ended = true,
            CodexLineOutcome::Failed { outcome, message } => {
                found = Some((outcome, message));
                ended = true;
            }
        }
    }
    let rest = &buf[start..];
    if !open.discarding {
        if open.partial.len() + rest.len() > MAX_LINE_BYTES {
            open.partial.clear();
            open.discarding = true;
        } else {
            open.partial.extend_from_slice(rest);
        }
    }
    let reply = watch.take_reply();
    if ended {
        tailer.watch = None;
        tailer.open = None;
    } else if shrank_while_retiring {
        tailer.open = None;
    }
    (found, reply)
}

/// Whether `path` still names `file`, the rollout a watch holds open: false
/// once the file was unlinked (it has no links left) or the path resolves to
/// nothing or, on Unix, to another file (device and inode differ). Elsewhere a
/// path replaced by another file still answers true.
fn path_still_names(path: &str, file: &File) -> bool {
    let Ok(at_path) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let Ok(held) = file.metadata() else {
            return false;
        };
        held.nlink() > 0 && held.dev() == at_path.dev() && held.ino() == at_path.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (file, at_path);
        true
    }
}

/// Validate and open the rollout at `path` — see the module doc's path-safety
/// list. `Err` names the check that refused it.
pub fn open_rollout(path: &str) -> Result<File, &'static str> {
    let given = Path::new(path);
    if !given.is_absolute() {
        return Err("not absolute");
    }
    if given.components().any(|c| c == Component::ParentDir) {
        return Err("contains ..");
    }
    let canonical = std::fs::canonicalize(given).map_err(|_| "does not resolve")?;
    let name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("no file name")?;
    if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
        return Err("not a rollout-*.jsonl file");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(&canonical).map_err(|_| "cannot open")?;
    let meta = file.metadata().map_err(|_| "cannot stat")?;
    if !meta.is_file() {
        return Err("not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: `getuid` has no preconditions and cannot fail.
        if meta.uid() != unsafe { libc::getuid() } {
            return Err("not owned by the daemon's user");
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota_block::BlockedKind;
    use spec::spec;
    use std::io::Write as _;

    const TURN: &str = "turn-714";

    fn arm(agent: &str, path: &Path, turn: Option<&str>) -> ArmCommand {
        ArmCommand::Arm(ArmRequest {
            pane_id: format!("pane-{agent}"),
            agent_id: agent.to_string(),
            session_id: format!("session-{agent}"),
            path: Some(path.to_string_lossy().into_owned()),
            turn_id: turn.map(str::to_owned),
        })
    }

    fn failure_lines(turn: &str) -> String {
        format!(
            concat!(
                r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"{t}"}}}}"#,
                "\n",
                r#"{{"type":"event_msg","payload":{{"type":"token_count","turn_id":"{t}","rate_limits":{{"credits":{{"has_credits":false}},"rate_limit_reached_type":"workspace_member_credits_depleted"}}}}}}"#,
                "\n",
                r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{t}","error":{{"message":"out of credits","codex_error_info":"usage_limit_exceeded"}}}}}}"#,
                "\n"
            ),
            t = turn
        )
    }

    fn append(path: &Path, text: &[u8]) {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap()
            .write_all(text)
            .unwrap();
    }

    fn live(_: &str, _: &str) -> bool {
        true
    }

    /// Scenario: Arm Codex rollout tailers against files in a temp dir and
    /// poll them. A usage-limit failure for the armed turn is reported once as
    /// a block and disarms, and a non-quota failure is reported once as an
    /// error; a dead owner's tailer is dropped; a FIFO, a wrong file name and
    /// a `..` path are refused; a tick reads at most 1 MiB, a 300 KiB line is
    /// skipped, and a new arm starts 256 KiB before the end.
    #[spec("status/blocked/013")]
    #[test]
    fn status_blocked_013_codex_rollout_tailer_is_bounded_and_path_safe() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-09-26T04-19-10-a.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");

        // Armed turn, then the failure: one block, then the watch is gone.
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("a", &rollout, Some(TURN)));
        assert!(tailers.tick(live).is_empty());
        append(&rollout, failure_lines(TURN).as_bytes());
        let blocks = tailers.tick(live);
        assert_eq!(
            blocks,
            vec![CodexTurnFailure {
                pane_id: "pane-a".into(),
                agent_id: "a".into(),
                session_id: "session-a".into(),
                turn_id: TURN.into(),
                outcome: FailureOutcome::Blocked {
                    kind: BlockedKind::CreditsDepleted,
                    resets_at_ms: None,
                },
                message: Some("out of credits".into()),
            }]
        );
        assert!(!tailers.is_armed("a"), "a completed turn disarms");
        append(&rollout, failure_lines(TURN).as_bytes());
        assert!(tailers.tick(live).is_empty(), "reported once");

        // Issue #1359: any other failed turn is reported once, as an error.
        tailers.apply(arm("a", &rollout, Some("turn-err")));
        assert!(tailers.tick(live).is_empty());
        append(
            &rollout,
            concat!(
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-err"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-err","error":{"message":"model not supported","codex_error_info":"other"}}}"#,
                "\n"
            )
            .as_bytes(),
        );
        assert_eq!(
            tailers.tick(live),
            vec![CodexTurnFailure {
                pane_id: "pane-a".into(),
                agent_id: "a".into(),
                session_id: "session-a".into(),
                turn_id: "turn-err".into(),
                outcome: FailureOutcome::Error,
                message: Some("model not supported".into()),
            }]
        );
        assert!(!tailers.is_armed("a"), "a failed turn disarms");
        assert!(tailers.tick(live).is_empty(), "reported once");

        // Another turn's failure is not ours; a Stop for the watched turn
        // disarms, and a late Stop for an earlier turn does not.
        tailers.apply(arm("a", &rollout, Some("turn-2")));
        append(&rollout, failure_lines("turn-other").as_bytes());
        assert!(tailers.tick(live).is_empty());
        tailers.apply(ArmCommand::Disarm {
            pane_id: "pane-a".into(),
            agent_id: "a".into(),
            turn_id: TURN.into(),
        });
        assert!(tailers.is_armed("a"), "a Stop for another turn");
        tailers.apply(ArmCommand::Disarm {
            pane_id: "pane-a".into(),
            agent_id: "a".into(),
            turn_id: "turn-2".into(),
        });
        assert!(!tailers.is_armed("a"));

        // The failure may be written before the arm is handled: the back
        // window covers it.
        tailers.apply(arm("a", &rollout, Some("turn-3")));
        append(&rollout, failure_lines("turn-3").as_bytes());
        tailers.apply(arm("a", &rollout, Some("turn-3")));
        assert_eq!(tailers.tick(live).len(), 1);

        // Arming without a path is a no-op for an unknown agent; a dead owner's
        // tailer is dropped.
        tailers.apply(ArmCommand::Arm(ArmRequest {
            pane_id: "pane-x".into(),
            agent_id: "x".into(),
            session_id: "s".into(),
            path: None,
            turn_id: Some(TURN.into()),
        }));
        assert!(!tailers.has_tailer("x"));
        tailers.apply(arm("a", &rollout, Some("turn-4")));
        tailers.tick(|_, _| false);
        assert!(!tailers.has_tailer("a"), "a dead owner's tailer is dropped");

        // Refusals.
        let wrong_name = dir.path().join("notes.jsonl");
        append(&wrong_name, failure_lines(TURN).as_bytes());
        assert_eq!(
            open_rollout(&wrong_name.to_string_lossy()).unwrap_err(),
            "not a rollout-*.jsonl file"
        );
        let dotdot = format!("{}/sub/../{}", dir.path().display(), "rollout-x.jsonl");
        assert_eq!(open_rollout(&dotdot).unwrap_err(), "contains ..");
        assert_eq!(
            open_rollout("rollout-rel.jsonl").unwrap_err(),
            "not absolute"
        );
        #[cfg(unix)]
        {
            let fifo = dir.path().join("rollout-fifo.jsonl");
            let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
            // SAFETY: a valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            let link = dir.path().join("rollout-link.jsonl");
            std::os::unix::fs::symlink(&fifo, &link).unwrap();
            assert_eq!(
                open_rollout(&link.to_string_lossy()).unwrap_err(),
                "not a regular file",
                "a symlink to a FIFO is refused without blocking"
            );
            // Through the tailer: refused, disarmed, and never retried.
            let mut t = CodexRolloutTailers::default();
            t.apply(arm("f", &link, Some(TURN)));
            assert!(t.tick(live).is_empty());
            assert!(!t.is_armed("f"));
            t.apply(arm("f", &link, Some(TURN)));
            assert!(t.tick(live).is_empty());
            assert!(t.refused.contains(&link.to_string_lossy().into_owned()));
        }

        // Bounds: 1 MiB per tick, a 300 KiB line skipped, the back window.
        let big = dir.path().join("rollout-big.jsonl");
        let mut body = Vec::new();
        body.extend(std::iter::repeat_n(b'x', 300 * 1024));
        body.push(b'\n');
        append(&big, &body);
        let mut t = CodexRolloutTailers::default();
        t.apply(arm("b", &big, Some(TURN)));
        assert!(t.tick(live).is_empty());
        let offset = |t: &CodexRolloutTailers| t.tailers["b"].open.as_ref().unwrap().offset;
        assert_eq!(
            offset(&t),
            (300 * 1024 + 1) as u64,
            "the first arm starts at len - 256 KiB and reads to the end"
        );
        // A 300 KiB line straddling the watch, then the real failure after it.
        let mut long = br#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-714","error":{"codex_error_info":"usage_limit_exceeded"},"pad":""#.to_vec();
        long.extend(std::iter::repeat_n(b'y', 300 * 1024));
        long.extend_from_slice(b"\"}}\n");
        append(&big, &long);
        assert!(t.tick(live).is_empty(), "an overlong line is never parsed");
        assert!(t.is_armed("b"));
        // 3 MiB of filler: read in 1 MiB steps.
        let filler_line = [&[b'z'; 1023][..], b"\n"].concat();
        let filler: Vec<u8> = filler_line.repeat(3 * 1024);
        append(&big, &filler);
        let before = offset(&t);
        t.tick(live);
        assert_eq!(offset(&t) - before, MAX_READ_PER_TICK);
        append(&big, failure_lines(TURN).as_bytes());
        let mut found = Vec::new();
        for _ in 0..4 {
            found.extend(t.tick(live));
        }
        assert_eq!(found.len(), 1, "the failure after the filler is found");
    }

    /// Issue #714 (Qodo on PR #1346): a back window that starts EXACTLY on a
    /// record boundary keeps the record that starts there. The first segment
    /// is dropped only when the byte before the window is not a newline, i.e.
    /// when the window really did open mid-record; here that record is the
    /// armed turn's quota `task_complete`, and dropping it would leave a
    /// blocked Codex agent showing as working.
    #[test]
    fn a_back_window_starting_on_a_record_boundary_keeps_that_record() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-09-26T06-00-00-d.jsonl");
        let record = format!(
            concat!(
                r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{t}","#,
                r#""error":{{"message":"out of credits","codex_error_info":"usage_limit_exceeded"}}}}}}"#,
                "\n"
            ),
            t = TURN
        );
        // Everything from the record to the end is exactly BACK_WINDOW bytes,
        // so `len - BACK_WINDOW` is the record's first byte.
        let window = BACK_WINDOW as usize;
        let mut tail = record.clone().into_bytes();
        let filler_line = [&[b'z'; 1023][..], b"\n"].concat();
        while window - tail.len() > filler_line.len() {
            tail.extend_from_slice(&filler_line);
        }
        let rest = window - tail.len();
        tail.extend(std::iter::repeat_n(b'z', rest - 1));
        tail.push(b'\n');
        assert_eq!(tail.len(), window);
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        append(&rollout, &filler_line.repeat(4));
        append(&rollout, &tail);

        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("d", &rollout, Some(TURN)));
        let blocks = tailers.tick(live);
        assert_eq!(
            blocks.len(),
            1,
            "the quota record at the back window's first byte was discarded as a partial line"
        );

        // Control: a window that opens one byte INTO that record still drops
        // the fragment rather than parsing half a line.
        let straddled = dir.path().join("rollout-2026-09-26T06-00-01-e.jsonl");
        append(&straddled, b"{\"type\":\"session_meta\"}\n");
        append(&straddled, &filler_line.repeat(4));
        append(&straddled, b"{");
        append(&straddled, &tail);
        let mut t = CodexRolloutTailers::default();
        t.apply(arm("e", &straddled, Some(TURN)));
        assert!(
            t.tick(live).is_empty(),
            "a window opening mid-record must drop that record's fragment"
        );
    }

    /// PRD #1497: a watched turn's `task_complete` hands its final reply to the
    /// daemon — a successful one and a failed one alike — once, with the
    /// tailer's pane and agent; another turn's completion, and one with no
    /// reply, hand nothing.
    #[test]
    fn a_watched_turns_task_complete_yields_its_reply_once() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-08T05-00-00-r.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();

        tailers.apply(arm("r", &rollout, Some("turn-ok")));
        append(
            &rollout,
            concat!(
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"other","last_agent_message":"not ours"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-ok","last_agent_message":"All tests pass."}}"#,
                "\n"
            )
            .as_bytes(),
        );
        assert!(
            tailers.tick(live).is_empty(),
            "a successful turn is no failure"
        );
        let replies = tailers.take_replies();
        assert_eq!(
            replies,
            vec![CodexTurnReply {
                pane_id: "pane-r".into(),
                agent_id: "r".into(),
                reply: crate::daemon_protocol::FinalReply {
                    turn_id: Some("turn-ok".into()),
                    text: "All tests pass.".into(),
                    failed: false,
                },
            }]
        );
        assert!(tailers.take_replies().is_empty(), "taken once");
        assert!(!tailers.is_armed("r"), "the completed turn disarms");

        // A failed turn's reply is marked failed and still reported as a failure.
        tailers.apply(arm("r", &rollout, Some("turn-bad")));
        append(
            &rollout,
            concat!(
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-bad","last_agent_message":"I could not finish.","error":{"message":"boom","codex_error_info":"other"}}}"#,
                "\n"
            )
            .as_bytes(),
        );
        assert_eq!(tailers.tick(live).len(), 1);
        let replies = tailers.take_replies();
        assert_eq!(replies.len(), 1);
        assert!(replies[0].reply.failed);
        assert_eq!(replies[0].reply.text, "I could not finish.");

        // A completion with no reply hands an empty one, marked failed here:
        // the turn ended with nothing to read (audit A2), and is still
        // reported as a failure.
        tailers.apply(arm("r", &rollout, Some("turn-quiet")));
        append(&rollout, failure_lines("turn-quiet").as_bytes());
        assert_eq!(tailers.tick(live).len(), 1);
        let replies = tailers.take_replies();
        assert_eq!(replies.len(), 1);
        assert!(replies[0].reply.is_empty());
        assert!(replies[0].reply.failed);
        assert_eq!(replies[0].reply.turn_id.as_deref(), Some("turn-quiet"));
    }

    /// Issue #714 (audit A3): disarming — by a matching `Stop` or by the turn's
    /// own `task_complete` — closes the rollout, keeping only its path, and the
    /// next arm re-opens it through the full path validation.
    #[test]
    fn a_disarmed_tailer_closes_its_file_and_revalidates_on_the_next_arm() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-09-26T05-00-00-c.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let is_open = |t: &CodexRolloutTailers| t.tailers["c"].open.is_some();

        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("c", &rollout, Some(TURN)));
        assert!(tailers.tick(live).is_empty());
        assert!(is_open(&tailers), "an armed turn holds the rollout open");

        // A matching Stop closes it; the path is kept.
        tailers.apply(ArmCommand::Disarm {
            pane_id: "pane-c".into(),
            agent_id: "c".into(),
            turn_id: TURN.into(),
        });
        assert!(
            !is_open(&tailers),
            "a Stop for the watched turn closes the file"
        );
        assert!(tailers.has_tailer("c"));
        assert!(tailers.tick(live).is_empty());
        assert!(!is_open(&tailers), "a tick with no watch opens nothing");

        // An arm with no path reuses the kept one and re-opens it.
        tailers.apply(ArmCommand::Arm(ArmRequest {
            pane_id: "pane-c".into(),
            agent_id: "c".into(),
            session_id: "session-c".into(),
            path: None,
            turn_id: Some("turn-2".into()),
        }));
        assert!(tailers.tick(live).is_empty());
        assert!(is_open(&tailers), "the next arm re-opens the kept path");

        // The turn's own task_complete closes it too.
        append(&rollout, failure_lines("turn-2").as_bytes());
        assert_eq!(tailers.tick(live).len(), 1);
        assert!(!tailers.is_armed("c"));
        assert!(!is_open(&tailers), "a completed turn closes the file");

        // The re-open validates again: the same path now resolves to a file
        // that is not a rollout, so the next arm is refused, not read.
        #[cfg(unix)]
        {
            let notes = dir.path().join("notes.jsonl");
            append(&notes, failure_lines("turn-3").as_bytes());
            std::fs::remove_file(&rollout).unwrap();
            std::os::unix::fs::symlink(&notes, &rollout).unwrap();
            tailers.apply(arm("c", &rollout, Some("turn-3")));
            assert!(
                tailers.tick(live).is_empty(),
                "the swapped path is not read"
            );
            assert!(!tailers.is_armed("c"));
            assert!(!is_open(&tailers));
            assert!(
                tailers
                    .refused
                    .contains(&rollout.to_string_lossy().into_owned())
            );
        }
    }

    fn stopped(agent: &str, turn: &str) -> ArmCommand {
        ArmCommand::StoppedWithoutReply {
            pane_id: format!("pane-{agent}"),
            agent_id: agent.into(),
            turn_id: turn.into(),
        }
    }

    fn aborted(turn: &str) -> String {
        format!(
            concat!(
                r#"{{"type":"event_msg","payload":{{"type":"turn_aborted","turn_id":"{t}","reason":"interrupted","started_at":1785699168,"completed_at":1785699173,"duration_ms":5090}}}}"#,
                "\n"
            ),
            t = turn
        )
    }

    fn completed(turn: &str, text: &str) -> String {
        format!(
            concat!(
                r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{t}","last_agent_message":"{m}"}}}}"#,
                "\n"
            ),
            t = turn,
            m = text
        )
    }

    /// PRD #1497: one tailer per agent and one watch per tailer, replaced by
    /// the agent's next arm or by a new rollout path, and dropped with the
    /// agent. So turns that never write a record (a Codex that crashed
    /// mid-turn) leave an agent holding at most one open rollout, and none
    /// once it exits.
    #[test]
    fn a_watch_whose_task_complete_never_comes_is_bounded_per_agent() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T05-00-00-s.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let open_files =
            |t: &CodexRolloutTailers| t.tailers.values().filter(|t| t.open.is_some()).count();
        let mut tailers = CodexRolloutTailers::default();

        // Many turns that never complete: still one tailer, one file.
        for turn in 1..200 {
            tailers.apply(arm("s", &rollout, Some(&format!("turn-{turn}"))));
            assert!(tailers.tick(live).is_empty());
        }
        assert!(tailers.is_armed("s"));
        assert_eq!(tailers.tailers.len(), 1);
        assert_eq!(open_files(&tailers), 1);

        // A new rollout (a new Codex session) replaces the tailer.
        let rotated = dir.path().join("rollout-2026-10-09T06-00-00-s.jsonl");
        append(&rotated, b"{\"type\":\"session_meta\"}\n");
        tailers.apply(arm("s", &rotated, Some("turn-new")));
        assert!(tailers.tick(live).is_empty());
        assert_eq!(tailers.tailers.len(), 1);
        assert_eq!(tailers.tailers["s"].path, rotated.to_string_lossy());
        assert_eq!(open_files(&tailers), 1);

        // The agent exits (or crashes): its tailer, file and watch go.
        assert!(tailers.tick(|_, _| false).is_empty());
        assert!(!tailers.has_tailer("s"));
        assert_eq!(open_files(&tailers), 0);
    }

    /// PRD #1497 re-audit R3: after a reply-less `Stop` for the watched turn,
    /// the watch waits [`DRAIN_AFTER_STOP`] for the turn's `task_complete` and
    /// is then retired: the file is closed, and the turn's later
    /// `task_complete` hands no reply and no failure. A repeated `Stop` does
    /// not extend the drain, and a `Stop` for another turn does not start one.
    #[test]
    fn a_reply_less_stop_retires_the_watch_after_the_drain() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T07-00-00-d.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("d", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());

        tailers.apply(stopped("d", "turn-other"));
        assert!(
            tailers.tailers["d"].stopped_at.is_none(),
            "a Stop for another turn starts no drain"
        );
        let before = Instant::now();
        tailers.apply(stopped("d", "turn-1"));
        let started = tailers.tailers["d"].stopped_at.expect("the drain started");
        assert!(started >= before);
        tailers.apply(stopped("d", "turn-1"));
        assert_eq!(
            tailers.tailers["d"].stopped_at,
            Some(started),
            "a repeated Stop does not extend the drain"
        );

        assert!(
            tailers
                .tick_at(started + DRAIN_AFTER_STOP - Duration::from_secs(1), live)
                .is_empty()
        );
        assert!(tailers.is_armed("d"), "still within the drain");
        assert!(tailers.holds_file("d"));

        assert!(tailers.tick_at(started + DRAIN_AFTER_STOP, live).is_empty());
        assert!(!tailers.is_armed("d"), "the drain ran out");
        assert!(!tailers.holds_file("d"), "and the file is closed");
        assert!(tailers.take_replies().is_empty(), "nothing is delivered");

        // The turn's task_complete arriving afterwards is not read.
        append(&rollout, completed("turn-1", "Late.").as_bytes());
        append(&rollout, failure_lines("turn-1").as_bytes());
        assert!(tailers.tick(live).is_empty());
        assert!(tailers.take_replies().is_empty());
        assert!(!tailers.holds_file("d"));

        // A new watch starts with no drain, even after one ran out.
        tailers.apply(arm("d", &rollout, Some("turn-2")));
        assert!(tailers.tailers["d"].stopped_at.is_none());
    }

    /// PRD #1497 re-audit R3: a turn that has reached no `Stop` is still
    /// running, and is never timed out however long it runs; its
    /// `task_complete` still yields its reply once and disarms.
    #[test]
    fn a_turn_with_no_stop_is_never_drained() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T08-00-00-n.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("n", &rollout, Some("turn-1")));
        let now = Instant::now();
        for days in [0u64, 1, 30, 365] {
            let later = now + Duration::from_secs(days * 24 * 3600) + DRAIN_AFTER_STOP;
            assert!(tailers.tick_at(later, live).is_empty());
            assert!(tailers.is_armed("n"), "a running turn after {days} days");
            assert!(tailers.holds_file("n"));
        }

        append(
            &rollout,
            completed("turn-1", "All 42 tests pass.").as_bytes(),
        );
        assert!(tailers.tick(live).is_empty());
        let replies = tailers.take_replies();
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0].reply.text, "All 42 tests pass.");
        assert!(!tailers.is_armed("n"));
        assert!(!tailers.holds_file("n"));
    }

    /// PRD #1497 re-audit R3: an interrupted turn writes `turn_aborted`, not a
    /// `task_complete`. That record, for the watched turn, retires the watch
    /// and closes the file with nothing delivered — the hub announces no
    /// interrupted turn — whether or not a reply-less `Stop` came first.
    /// Another turn's `turn_aborted` leaves the watch armed.
    #[test]
    fn a_turn_aborted_record_disarms_and_closes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T09-00-00-a.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();

        tailers.apply(arm("a", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        append(&rollout, aborted("turn-other").as_bytes());
        assert!(tailers.tick(live).is_empty());
        assert!(tailers.is_armed("a"), "another turn's abort is not ours");

        append(&rollout, aborted("turn-1").as_bytes());
        assert!(tailers.tick(live).is_empty(), "an abort is no failure");
        assert!(tailers.take_replies().is_empty(), "and hands no reply");
        assert!(!tailers.is_armed("a"));
        assert!(!tailers.holds_file("a"));

        // After a reply-less Stop, likewise, well inside the drain.
        tailers.apply(arm("a", &rollout, Some("turn-2")));
        assert!(tailers.tick(live).is_empty());
        tailers.apply(stopped("a", "turn-2"));
        append(&rollout, aborted("turn-2").as_bytes());
        assert!(tailers.tick(live).is_empty());
        assert!(tailers.take_replies().is_empty());
        assert!(!tailers.is_armed("a"));
        assert!(!tailers.holds_file("a"));
    }

    /// PRD #1497 re-audit R3: a watched rollout that is unlinked, or whose path
    /// is replaced by another file, while its agent stays alive is closed by
    /// the next tick rather than held — what the old file still had to say is
    /// read first. The replacement is not read for the old watch.
    #[cfg(unix)]
    #[test]
    fn an_unlinked_or_replaced_rollout_is_closed_on_the_next_tick() {
        let dir = tempfile::tempdir().unwrap();

        // Unlinked.
        let rollout = dir.path().join("rollout-2026-10-09T10-00-00-u.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("u", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        assert!(tailers.holds_file("u"));
        std::fs::remove_file(&rollout).unwrap();
        assert!(tailers.tick(live).is_empty());
        assert!(!tailers.holds_file("u"), "an unlinked rollout is closed");
        assert!(!tailers.is_armed("u"));
        assert!(tailers.take_replies().is_empty());

        // Replaced: the path now names another file, which carries the
        // watched turn's completion; the old file is closed and the new one
        // is not read for this watch.
        let rollout = dir.path().join("rollout-2026-10-09T10-00-01-r.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("r", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        assert!(tailers.holds_file("r"));
        let replacement = dir.path().join("replacement.tmp");
        append(
            &replacement,
            completed("turn-1", "From the new file.").as_bytes(),
        );
        std::fs::rename(&replacement, &rollout).unwrap();
        assert!(tailers.tick(live).is_empty());
        assert!(!tailers.holds_file("r"), "a replaced rollout is closed");
        assert!(!tailers.is_armed("r"));
        assert!(tailers.take_replies().is_empty());

        // A completion the old file got before the swap is still read.
        let rollout = dir.path().join("rollout-2026-10-09T10-00-02-k.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("k", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        append(&rollout, completed("turn-1", "Written before.").as_bytes());
        std::fs::remove_file(&rollout).unwrap();
        assert!(tailers.tick(live).is_empty());
        let replies = tailers.take_replies();
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0].reply.text, "Written before.");
        assert!(!tailers.holds_file("k"));
    }

    /// `n` bytes of newline-terminated filler lines, no record among them.
    fn filler(n: usize) -> Vec<u8> {
        let line = [&[b'z'; 1023][..], b"\n"].concat();
        let mut out = line.repeat(n / line.len());
        let rest = n - out.len();
        if rest > 0 {
            out.extend(std::iter::repeat_n(b'z', rest - 1));
            out.push(b'\n');
        }
        assert_eq!(out.len(), n);
        out
    }

    fn offset_of(t: &CodexRolloutTailers, agent: &str) -> u64 {
        t.tailers[agent].open.as_ref().unwrap().offset
    }

    /// PRD #1497 re-audit R3.1: a watched rollout unlinked, or replaced, while
    /// more than one tick's read of it is still unread is not closed after
    /// that one read: the old file is read up to the length it had when the
    /// retirement was decided, over as many ticks as that takes, so the
    /// watched turn's completion past the first MiB is delivered once. Bytes
    /// appended to the old file after that moment are not waited for.
    #[cfg(unix)]
    #[test]
    fn a_retired_rollout_is_read_to_its_length_before_it_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        for replace in [false, true] {
            let rollout = dir.path().join(format!(
                "rollout-2026-10-09T11-00-0{}-b.jsonl",
                u8::from(replace)
            ));
            append(&rollout, b"{\"type\":\"session_meta\"}\n");
            let mut tailers = CodexRolloutTailers::default();
            tailers.apply(arm("b", &rollout, Some("turn-1")));
            assert!(tailers.tick(live).is_empty());
            append(&rollout, &filler(3 * 1024 * 1024 / 2));
            append(&rollout, completed("turn-1", "Beyond one read.").as_bytes());
            if replace {
                let other = dir.path().join("replacement.tmp");
                append(&other, completed("turn-1", "From the new file.").as_bytes());
                std::fs::rename(&other, &rollout).unwrap();
            } else {
                std::fs::remove_file(&rollout).unwrap();
            }

            assert!(tailers.tick(live).is_empty());
            assert!(tailers.take_replies().is_empty(), "one MiB read so far");
            assert!(
                tailers.holds_file("b"),
                "replace={replace}: retired after one read, before the completion"
            );
            assert!(tailers.tick(live).is_empty());
            let replies = tailers.take_replies();
            assert_eq!(replies.len(), 1, "replace={replace}: {replies:?}");
            assert_eq!(replies[0].reply.text, "Beyond one read.");
            assert!(!tailers.holds_file("b"));
            assert!(!tailers.is_armed("b"));
            assert!(tailers.tick(live).is_empty());
            assert!(tailers.take_replies().is_empty(), "delivered once");
        }

        // A writer that keeps appending to the unlinked file is not waited
        // for: the watch closes at the length the file had when retirement was
        // decided.
        let rollout = dir.path().join("rollout-2026-10-09T11-00-09-w.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("w", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        writer.write_all(&filler(3 * 1024 * 1024 / 2)).unwrap();
        std::fs::remove_file(&rollout).unwrap();
        assert!(tailers.tick(live).is_empty());
        let boundary = tailers.tailers["w"].retiring.expect("retirement decided");
        writer.write_all(&filler(4 * 1024 * 1024)).unwrap();
        writer
            .write_all(completed("turn-1", "Too late.").as_bytes())
            .unwrap();
        assert!(tailers.tick(live).is_empty());
        assert!(
            !tailers.holds_file("w"),
            "closed at the boundary ({boundary}) while the writer kept going"
        );
        assert!(tailers.take_replies().is_empty());
    }

    /// PRD #1497 re-audit R3.1: when the drain after a reply-less `Stop` runs
    /// out with more of the file still unread than the ticks so far have read,
    /// the completion already written in that backlog is still read before
    /// the watch closes.
    #[test]
    fn a_drain_that_runs_out_still_reads_the_written_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T12-00-00-q.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("q", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        tailers.apply(stopped("q", "turn-1"));
        let started = tailers.tailers["q"].stopped_at.unwrap();
        append(&rollout, &filler(5 * 1024 * 1024 / 2));
        append(
            &rollout,
            completed("turn-1", "Behind a backlog.").as_bytes(),
        );

        let mut at = started + DRAIN_AFTER_STOP - POLL_INTERVAL;
        assert!(tailers.tick_at(at, live).is_empty());
        at += POLL_INTERVAL;
        assert!(tailers.tick_at(at, live).is_empty());
        assert!(tailers.tailers["q"].retiring.is_some(), "the drain ran out");
        assert!(tailers.take_replies().is_empty(), "two MiB read so far");
        assert!(tailers.holds_file("q"));
        at += POLL_INTERVAL;
        assert!(tailers.tick_at(at, live).is_empty());
        let replies = tailers.take_replies();
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0].reply.text, "Behind a backlog.");
        assert!(!tailers.holds_file("q"));
    }

    /// PRD #1497 re-audit R3.1: a completion whose line straddles the
    /// one-read budget — its start read in one tick, its newline in the next
    /// — is still assembled and read when retirement is decided in between.
    #[test]
    fn a_completion_split_at_the_read_budget_survives_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T13-00-00-p.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("p", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        tailers.apply(stopped("p", "turn-1"));
        let started = tailers.tailers["p"].stopped_at.unwrap();
        let before = offset_of(&tailers, "p");
        append(&rollout, &filler(MAX_READ_PER_TICK as usize - 100));
        let record = completed("turn-1", "Split across two reads.");
        assert!(record.len() > 100);
        append(&rollout, record.as_bytes());

        assert!(tailers.tick_at(started + DRAIN_AFTER_STOP, live).is_empty());
        assert_eq!(offset_of(&tailers, "p") - before, MAX_READ_PER_TICK);
        assert_eq!(
            tailers.tailers["p"].open.as_ref().unwrap().partial.len(),
            100,
            "the record's first 100 bytes wait for its newline"
        );
        assert!(tailers.tailers["p"].retiring.is_some());
        assert!(tailers.take_replies().is_empty());
        assert!(
            tailers
                .tick_at(started + DRAIN_AFTER_STOP + POLL_INTERVAL, live)
                .is_empty()
        );
        let replies = tailers.take_replies();
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0].reply.text, "Split across two reads.");
        assert!(!tailers.holds_file("p"));
    }

    /// PRD #1497 audit R3.1 follow-up: a held rollout truncated, while its
    /// watch is retiring, to a length at or above what was already read but
    /// below the length retirement was decided at, is closed on the next tick
    /// — not held waiting for a boundary the file can no longer reach — with
    /// no later arm, `Stop` or agent exit involved.
    #[test]
    fn a_retiring_rollout_truncated_short_of_its_boundary_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        // How far past the read position the file is cut: above it, or
        // exactly at it.
        for past_offset in [MAX_READ_PER_TICK / 2, 0] {
            let rollout = dir
                .path()
                .join(format!("rollout-2026-10-09T15-00-00-t{past_offset}.jsonl"));
            append(&rollout, b"{\"type\":\"session_meta\"}\n");
            let mut tailers = CodexRolloutTailers::default();
            tailers.apply(arm("t", &rollout, Some("turn-1")));
            assert!(tailers.tick(live).is_empty());
            tailers.apply(stopped("t", "turn-1"));
            let started = tailers.tailers["t"].stopped_at.unwrap();
            append(&rollout, &filler(3 * 1024 * 1024));

            // The drain runs out: retirement is decided at about 3 MiB, and
            // this tick reads the first MiB of it.
            let mut at = started + DRAIN_AFTER_STOP;
            assert!(tailers.tick_at(at, live).is_empty());
            let boundary = tailers.tailers["t"].retiring.expect("retirement decided");
            let offset = offset_of(&tailers, "t");
            assert!(offset < boundary);

            let new_len = offset + past_offset;
            assert!(new_len < boundary);
            std::fs::OpenOptions::new()
                .write(true)
                .open(&rollout)
                .unwrap()
                .set_len(new_len)
                .unwrap();

            at += POLL_INTERVAL;
            assert!(tailers.tick_at(at, live).is_empty());
            assert!(
                !tailers.holds_file("t"),
                "past_offset={past_offset}: closed once the file is short of {boundary}"
            );
            assert!(!tailers.is_armed("t"));
            assert!(tailers.take_replies().is_empty());

            // The turn is over: a late arm of it opens nothing.
            tailers.apply(arm("t", &rollout, Some("turn-1")));
            assert!(!tailers.is_armed("t"));
        }
    }

    /// PRD #1497 re-audit R3.2: a reply-less `Stop` that reaches the daemon
    /// before its turn's arm — with no watch yet, or while the agent's watch
    /// is for another turn — starts that turn's drain when the arm comes,
    /// counted from the `Stop`, rather than being forgotten and leaving the
    /// turn held with no bound.
    #[test]
    fn a_stop_before_its_arm_starts_the_drain_when_the_arm_comes() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T14-00-00-o.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");

        for watching_another in [false, true] {
            let mut tailers = CodexRolloutTailers::default();
            if watching_another {
                tailers.apply(arm("o", &rollout, Some("turn-0")));
                assert!(tailers.tick(live).is_empty());
            }
            let before = Instant::now();
            tailers.apply(stopped("o", "turn-1"));
            if watching_another {
                assert!(
                    tailers.tailers["o"].stopped_at.is_none(),
                    "another turn's Stop leaves the running watch alone"
                );
            }
            tailers.apply(arm("o", &rollout, Some("turn-1")));
            let started = tailers.tailers["o"]
                .stopped_at
                .expect("the earlier Stop starts the drain");
            assert!(started >= before && started <= Instant::now());
            assert!(
                tailers
                    .tick_at(started + DRAIN_AFTER_STOP - Duration::from_secs(1), live)
                    .is_empty()
            );
            assert!(tailers.is_armed("o"));
            assert!(tailers.tick_at(started + DRAIN_AFTER_STOP, live).is_empty());
            assert!(
                !tailers.is_armed("o"),
                "watching_another={watching_another}: retired by the drain"
            );
            assert!(!tailers.holds_file("o"));
        }
    }

    /// PRD #1497 re-audit R3.2: a repeated arm of the turn being watched —
    /// during its drain, or after the watch was retired, completed or
    /// disarmed — neither restarts the drain nor opens a fresh watch, while
    /// an arm of a genuinely different turn starts a watch with no drain.
    #[test]
    fn a_repeated_arm_of_the_same_turn_keeps_or_stays_ended() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T15-00-00-m.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();

        // During the drain: the drain and the read position are kept.
        tailers.apply(arm("m", &rollout, Some("turn-1")));
        assert!(tailers.tick(live).is_empty());
        tailers.apply(stopped("m", "turn-1"));
        let started = tailers.tailers["m"].stopped_at.unwrap();
        let offset = offset_of(&tailers, "m");
        tailers.apply(arm("m", &rollout, Some("turn-1")));
        assert_eq!(tailers.tailers["m"].stopped_at, Some(started));
        assert!(!tailers.tailers["m"].rewind, "the read position is kept");
        assert!(tailers.tick_at(started, live).is_empty());
        assert_eq!(offset_of(&tailers, "m"), offset);
        assert!(tailers.tick_at(started + DRAIN_AFTER_STOP, live).is_empty());
        assert!(!tailers.is_armed("m"), "retired on the original drain");

        // After retirement: no fresh watch, and nothing read for it.
        tailers.apply(arm("m", &rollout, Some("turn-1")));
        assert!(!tailers.is_armed("m"));
        append(&rollout, completed("turn-1", "Late.").as_bytes());
        assert!(tailers.tick(live).is_empty());
        assert!(tailers.take_replies().is_empty());
        assert!(!tailers.holds_file("m"));

        // A different turn starts with no drain, even after a Stop for the
        // turn before it.
        tailers.apply(arm("m", &rollout, Some("turn-2")));
        assert!(tailers.is_armed("m"));
        assert!(tailers.tailers["m"].stopped_at.is_none());

        // After completion: an arm of the completed turn opens no watch.
        append(&rollout, completed("turn-2", "Done.").as_bytes());
        assert!(tailers.tick(live).is_empty());
        assert_eq!(tailers.take_replies().len(), 1);
        tailers.apply(arm("m", &rollout, Some("turn-2")));
        assert!(!tailers.is_armed("m"));

        // After a reply-bearing Stop, before or after the arm: likewise.
        tailers.apply(ArmCommand::Disarm {
            pane_id: "pane-m".into(),
            agent_id: "m".into(),
            turn_id: "turn-3".into(),
        });
        tailers.apply(arm("m", &rollout, Some("turn-3")));
        assert!(!tailers.is_armed("m"), "a Disarm before its arm");
    }

    /// PRD #1497 re-audit R3.2: a reply-less `Stop` for another turn never
    /// touches a running watch, which stays armed with no timeout; and the
    /// ended-turn record is bounded per agent and dropped with the agent.
    #[test]
    fn another_turns_stop_leaves_a_running_watch_and_the_record_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-2026-10-09T16-00-00-k.jsonl");
        append(&rollout, b"{\"type\":\"session_meta\"}\n");
        let mut tailers = CodexRolloutTailers::default();
        tailers.apply(arm("k", &rollout, Some("turn-live")));
        assert!(tailers.tick(live).is_empty());
        for i in 0..50 {
            tailers.apply(stopped("k", &format!("turn-{i}")));
        }
        assert!(tailers.tailers["k"].stopped_at.is_none());
        let far = Instant::now() + Duration::from_secs(365 * 24 * 3600);
        assert!(tailers.tick_at(far, live).is_empty());
        assert!(tailers.is_armed("k"), "untouched by other turns' Stops");
        assert!(tailers.holds_file("k"));
        assert_eq!(tailers.ended["k"].turns.len(), MAX_ENDED_TURNS);

        // A Stop from an agent with no tailer is dropped with the agent.
        tailers.apply(stopped("gone", "turn-1"));
        assert!(tailers.ended.contains_key("gone"));
        tailers.tick(|_, agent| agent != "gone");
        assert!(!tailers.ended.contains_key("gone"));
        assert!(tailers.ended.contains_key("k"));
    }

    /// Issue #714 (audit A1): the arm queue is bounded by total bytes as well
    /// as by count, and the refused-path set by count and by bytes.
    #[test]
    fn the_arm_queue_and_the_refused_set_are_bounded() {
        let big = |i: usize| {
            ArmCommand::Arm(ArmRequest {
                pane_id: "p".into(),
                agent_id: format!("a{i}"),
                session_id: "s".repeat(64 * 1024),
                path: Some("/x/rollout-a.jsonl".into()),
                turn_id: Some("t".into()),
            })
        };
        let arms = CodexRolloutArms::default();
        for i in 0..64 {
            arms.push(big(i));
            assert!(arms.queued_bytes() <= MAX_PENDING_BYTES);
        }
        let queued = arms.drain();
        assert!(!queued.is_empty());
        assert!(queued.len() < 64, "the byte bound dropped the oldest");
        assert_eq!(queued.last(), Some(&big(63)), "the newest is kept");
        assert_eq!(arms.queued_bytes(), 0);

        // One command larger than the whole bound is not queued at all.
        arms.push(ArmCommand::Disarm {
            pane_id: "pane-a".into(),
            agent_id: "a".into(),
            turn_id: "t".repeat(MAX_PENDING_BYTES + 1),
        });
        assert!(arms.drain().is_empty());

        // The count bound still holds for small commands.
        for i in 0..MAX_PENDING + 10 {
            arms.push(ArmCommand::Disarm {
                pane_id: String::new(),
                agent_id: format!("a{i}"),
                turn_id: "t".into(),
            });
        }
        assert_eq!(arms.drain().len(), MAX_PENDING);

        // The refused set: each missing path is refused and remembered, and
        // the set never passes either bound.
        let dir = tempfile::tempdir().unwrap();
        let mut tailers = CodexRolloutTailers::default();
        for i in 0..MAX_REFUSED + 5 {
            let path = dir.path().join(format!("rollout-missing-{i}.jsonl"));
            tailers.apply(arm(&format!("r{i}"), &path, Some(TURN)));
            tailers.tick(live);
            assert!(tailers.refused.len() <= MAX_REFUSED);
            assert!(tailers.refused_bytes <= MAX_REFUSED_BYTES);
            assert_eq!(
                tailers.refused_bytes,
                tailers.refused.iter().map(String::len).sum::<usize>()
            );
        }
        let long_dir = dir.path().join("d".repeat(200));
        let mut tailers = CodexRolloutTailers::default();
        for i in 0..MAX_REFUSED {
            let path = format!(
                "{}/{}/rollout-{i}.jsonl",
                long_dir.display(),
                "e".repeat(3000)
            );
            tailers.apply(arm(&format!("l{i}"), Path::new(&path), Some(TURN)));
            tailers.tick(live);
            assert!(tailers.refused_bytes <= MAX_REFUSED_BYTES);
        }
    }
}
