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
//!   error), a Codex `Stop` naming that turn, or a newer arm. Disarming closes
//!   the file and keeps only its path, so an idle Codex pane holds no file
//!   descriptor; the next arm re-opens it, validating it again. A tailer is
//!   dropped when its agent is no longer the live owner of its pane, and the
//!   whole set when the daemon's monitor task is aborted.
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
use std::time::Duration;

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
    /// A Codex `Stop` for `turn_id`: that turn ended normally. A watch for any
    /// other turn is left armed.
    Disarm { agent_id: String, turn_id: String },
}

impl ArmCommand {
    /// The bytes this command's strings hold — what the queue's byte bound
    /// counts.
    pub fn byte_len(&self) -> usize {
        match self {
            ArmCommand::Disarm { agent_id, turn_id } => agent_id.len() + turn_id.len(),
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
    pub outcome: FailureOutcome,
    /// The `task_complete` error message, unscrubbed.
    pub message: Option<String>,
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
}

/// Every Codex agent's tailer, keyed by the registry `agent_id` (so a respawn
/// in the same pane is a new key). Owned by the daemon's monitor task.
#[derive(Debug, Default)]
pub struct CodexRolloutTailers {
    tailers: HashMap<String, Tailer>,
    refused: HashSet<String>,
    /// The sum of the lengths of `refused`.
    refused_bytes: usize,
}

impl CodexRolloutTailers {
    /// Apply one command from the hook loop.
    pub fn apply(&mut self, command: ArmCommand) {
        match command {
            ArmCommand::Disarm { agent_id, turn_id } => {
                if let Some(tailer) = self.tailers.get_mut(&agent_id)
                    && tailer
                        .watch
                        .as_ref()
                        .is_some_and(|w| w.turn_id() == turn_id)
                {
                    tailer.watch = None;
                    tailer.open = None;
                }
            }
            ArmCommand::Arm(req) => {
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
                    },
                };
                let mut tailer = tailer;
                tailer.session_id = req.session_id;
                if let Some(turn) = req.turn_id {
                    tailer.watch = Some(CodexTurnWatch::new(turn));
                    tailer.rewind = true;
                }
                self.tailers.insert(req.agent_id, tailer);
            }
        }
    }

    /// Whether `agent_id` has a turn armed. For tests and diagnostics.
    pub fn is_armed(&self, agent_id: &str) -> bool {
        self.tailers
            .get(agent_id)
            .is_some_and(|t| t.watch.is_some())
    }

    /// Whether any tailer exists for `agent_id`. For tests and diagnostics.
    pub fn has_tailer(&self, agent_id: &str) -> bool {
        self.tailers.contains_key(agent_id)
    }

    /// One poll: drop every tailer whose `(pane_id, agent_id)` is no longer a
    /// live owner, then read what each armed tailer's rollout has gained since
    /// the last tick and return the failed turns found. Does file I/O; the
    /// daemon runs it on a blocking thread, outside every lock of its own.
    pub fn tick(&mut self, is_live_owner: impl Fn(&str, &str) -> bool) -> Vec<CodexTurnFailure> {
        self.tailers
            .retain(|agent_id, tailer| is_live_owner(&tailer.pane_id, agent_id));
        let mut failures = Vec::new();
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
            if let Some((outcome, message)) = read_tailer(tailer) {
                failures.push(CodexTurnFailure {
                    pane_id: tailer.pane_id.clone(),
                    agent_id: agent_id.clone(),
                    session_id: tailer.session_id.clone(),
                    outcome,
                    message,
                });
            }
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
/// complete line to its watch. Returns the failure the watch found, if any.
fn read_tailer(tailer: &mut Tailer) -> Option<FoundFailure> {
    let open = tailer.open.as_mut()?;
    let len = match open.file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => {
            tailer.open = None;
            return None;
        }
    };
    if len < open.offset || tailer.rewind {
        // Truncated (or a fresh watch): start one window back from the end.
        let start = len.saturating_sub(BACK_WINDOW);
        if len < open.offset || start > open.offset || tailer.rewind {
            open.offset = start;
            open.partial.clear();
            open.discarding = opens_mid_record(&open.file, start);
        }
        tailer.rewind = false;
    }
    let want = (len - open.offset).min(MAX_READ_PER_TICK);
    if want == 0 {
        return None;
    }
    let mut buf = Vec::with_capacity(want as usize);
    if open.file.seek(SeekFrom::Start(open.offset)).is_err()
        || (&open.file).take(want).read_to_end(&mut buf).is_err()
    {
        tailer.open = None;
        return None;
    }
    open.offset += buf.len() as u64;

    let watch = tailer.watch.as_mut()?;
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
            CodexLineOutcome::TurnEnded => ended = true,
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
    if ended {
        tailer.watch = None;
        tailer.open = None;
    }
    found
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
            agent_id: "a".into(),
            turn_id: TURN.into(),
        });
        assert!(tailers.is_armed("a"), "a Stop for another turn");
        tailers.apply(ArmCommand::Disarm {
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
            agent_id: "a".into(),
            turn_id: "t".repeat(MAX_PENDING_BYTES + 1),
        });
        assert!(arms.drain().is_empty());

        // The count bound still holds for small commands.
        for i in 0..MAX_PENDING + 10 {
            arms.push(ArmCommand::Disarm {
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
