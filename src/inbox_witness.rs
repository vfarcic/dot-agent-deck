//! Issue #2341: Claude Code's prompt hook commits a deck message only once
//! the session transcript shows Claude took the prompt that carried it, and
//! since issue #2346 Codex's two hand-overs do the same (below).
//!
//! A `UserPromptSubmit` hook that writes a block in full has not delivered
//! it: Claude Code can still discard the prompt after the hook exits — when
//! another `UserPromptSubmit` hook blocks it, say — and then the model never
//! sees the block. What Claude does keep is its transcript: a prompt it takes
//! is appended as a `user` entry, and the context a hook added as an
//! `attachment` of type `hook_additional_context`, about a tenth of a second
//! after the hooks exit (measured on Claude Code 2.1.325). A blocked prompt
//! leaves only a `system` entry quoting it, and no hook context.
//!
//! So when the hook payload names a transcript that already exists, the hook
//! commits nothing itself. It hands its blocks over as before, records them
//! `handed` with their caps files written (as a once-waiter's are), and starts
//! a witness: this binary as `inbox witness`, detached, with the hook's
//! receipt connection as its stdin, so the deck keeps holding what the hook
//! fetched while the witness watches. The witness reads what Claude appends
//! after the point the hook started, commits each exact attempt a `user`
//! entry or a hook context carries whole, through its caps file
//! ([`crate::inbox_prompt::commit_exact`]), and reports the batch on the
//! hook's connection. An attempt it does not see is reported only `handed`;
//! once the connection closes the deck checks it in the spool and hands it
//! out again, marked as a possible repeat.
//!
//! A once-waiter's attempts that the prompt itself carries (a native wake)
//! are left to the witness the same way: the turn clock holds them while it
//! watches.
//!
//! A transcript that does not exist yet is watched from its start: Claude
//! Code creates it, and its folder, only when it records a session's first
//! prompt, blocked or taken. If it never appears while the witness watches,
//! the session keeps none, nothing can witness, and the witness commits what
//! the hook wrote whole. Where nothing can witness from the start — the
//! payload names no `.jsonl` transcript, the witness cannot be started, or
//! the deck runs without a receipt spool — the hook commits what it wrote,
//! as it did before.
//!
//! Issue #2347: the same witness also settles the turn the hook reported.
//! The hook's `Thinking` starts a turn on the card and on the deck's turn
//! clock, and a prompt Claude drops runs no turn and fires no `Stop`, so
//! nothing would end it. So in a deck pane every Claude Code prompt whose
//! payload names a `.jsonl` transcript gets a witness, deck message or not,
//! and the hook's `Thinking` carries a nonce for that prompt
//! ([`crate::event::PROMPT_TURN_METADATA_KEY`]). When the transcript records
//! that exact prompt as blocked by a hook ([`PromptTurn::blocked_in`])
//! before anything shows Claude took a prompt, the witness reports the turn
//! as over: an `Idle` carrying the nonce, which the daemon applies only
//! while that prompt's turn is still the pane's latest news
//! (`crate::state::AppState::admit_prompt_not_taken`). Measured on Claude
//! Code 2.1.328, for a hook that exits 2 and for one that prints
//! `{"decision":"block"}`: the entry is a `system` entry whose
//! `promptSubmitEnding.ending.kind` is `blocked_by_hook` and whose content
//! ends with `Original prompt: ` and the prompt exactly as the hook received
//! it; no `Stop` hook ran.
//!
//! Issue #2346: Codex's two hand-overs, its prompt hook's context and its
//! `Stop` hook's block reason, go through the same witness, reading the
//! session's rollout file, which Codex's hook payloads name as
//! `transcript_path` too. Measured on Codex 0.164.0 (the PR's description
//! has the runs):
//!
//! - Codex writes the rollout, at that path, before its first prompt's
//!   hooks run, so the mark is the file's length then.
//! - A prompt it takes is a `response_item` whose `payload` is a `message`
//!   of role `user`, its text an `input_text` part; the context a prompt
//!   hook added follows as a `message` of role `developer` (its
//!   `content_item_kinds` `hooks.additional_context`), about 0.2 s after the
//!   hooks start.
//! - A `Stop` block reason it goes on with is a `message` of role `user`
//!   whose text is the reason inside `<hook_prompt hook_run_id="stop:…">` and
//!   `</hook_prompt>`, with `&`, `<` and `>` escaped as XML entities. A
//!   block's opener and closer hold none of those, so the block is still
//!   found whole.
//! - A prompt another `UserPromptSubmit` hook blocks, by its JSON decision or
//!   by exiting 2, leaves no `user` message, but Codex still records the
//!   context the deck's hook added as the `developer` message, and the model
//!   sees it on its next turn (asked, it named that context's sentinel). So
//!   that context is committed: the model has it.
//! - A turn the user interrupts (Esc) after the deck's hook printed and
//!   before Codex took the output leaves neither the prompt, the context nor
//!   the `Stop` reason, only a `turn_aborted` record: what the hook printed
//!   is then not committed, and is handed out again.
//!
//! So for Codex a `user` or `developer` message carrying a block whole is
//! what the model took ([`entry_texts`]). A witness for Codex carries no
//! [`PromptTurn`]: the turn half above reads Claude Code's entries only.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::InboxAttemptRef;

/// The longest a witness watches a transcript for the attempts it was
/// handed.
pub const WITNESS_MAX: Duration = Duration::from_secs(20);

/// Issue #2347: the longest a witness watches for its prompt's fate. Claude
/// writes it once every `UserPromptSubmit` hook has finished, so a slow
/// sibling hook delays it; past this the turn is left to the next event, as
/// it was before.
pub const TURN_WATCH_MAX: Duration = Duration::from_secs(120);

/// How often a witness reads the transcript once [`WITNESS_MAX`] has passed.
const SLOW_POLL: Duration = Duration::from_millis(250);

/// The opening of the `system` entry's content Claude Code writes for a
/// prompt a `UserPromptSubmit` hook blocked.
const BLOCKED_OPENER: &str = "UserPromptSubmit operation blocked by hook";

/// What precedes the blocked prompt itself in that entry's content.
const ORIGINAL_PROMPT: &str = "Original prompt: ";

/// How often a witness reads what the transcript gained.
const POLL: Duration = Duration::from_millis(50);

/// The most transcript bytes one witness reads.
const MAX_READ: u64 = 64 * 1024 * 1024;

/// The longest transcript line a witness parses; a longer one is skipped
/// whole.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// What a transcript line must hold to be worth parsing at all.
const NEEDLE: &[u8] = crate::inbox_hub::BLOCK_OPENER.as_bytes();

/// Where a session transcript ended when the hook started: what Claude takes
/// from the hook's prompt is appended after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptMark {
    pub path: PathBuf,
    pub offset: u64,
}

impl TranscriptMark {
    /// The mark of a Claude Code or Codex hook payload's `transcript_path`:
    /// an absolute path to a `.jsonl` file, at its current length, or at 0
    /// when it does not exist yet (a session's first prompt: Claude Code
    /// creates the transcript, and its folder, only once it records that
    /// prompt, blocked or taken). `None` for anything else, and the hook then
    /// commits what it writes itself (module docs).
    pub fn of_payload(payload: &Value) -> Option<TranscriptMark> {
        let path = PathBuf::from(payload.get("transcript_path")?.as_str()?);
        if !path.is_absolute() || path.extension() != Some(std::ffi::OsStr::new("jsonl")) {
            return None;
        }
        match std::fs::metadata(&path) {
            Ok(metadata) => metadata.is_file().then_some(TranscriptMark {
                path,
                offset: metadata.len(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Some(TranscriptMark { path, offset: 0 })
            }
            Err(_) => None,
        }
    }
}

/// Issue #2347: the prompt a witness settles the turn of, and what it reports
/// if Claude drops it. Its nonce names the prompt to the daemon, it is no
/// credential: the report goes out with the pane's own token, read from the
/// environment the witness inherits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTurn {
    /// [`crate::event::PROMPT_TURN_METADATA_KEY`]'s value on the hook's
    /// `Thinking`.
    pub nonce: String,
    /// The session the hook's `Thinking` reported.
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The prompt's length in bytes.
    pub prompt_len: usize,
    /// The prompt's SHA-256, in hex: the prompt itself can be too long for a
    /// command line.
    pub prompt_sha256: String,
}

impl PromptTurn {
    /// The turn of `prompt`, with a fresh nonce; `None` when the system has
    /// no randomness to give one.
    pub fn new(prompt: &str, session_id: &str, cwd: Option<String>) -> Option<PromptTurn> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).ok()?;
        Some(PromptTurn {
            nonce: hex(&bytes),
            session_id: session_id.to_string(),
            cwd,
            prompt_len: prompt.len(),
            prompt_sha256: sha256_hex(prompt.as_bytes()),
        })
    }

    /// Whether `entry` is Claude Code's record of this very prompt blocked
    /// by a hook: a `system` entry, marked blocked by its structured ending
    /// or its opening words, whose content ends with [`ORIGINAL_PROMPT`] and
    /// the prompt exactly.
    pub fn blocked_in(&self, entry: &Value) -> bool {
        if entry.get("type").and_then(Value::as_str) != Some("system") {
            return false;
        }
        let Some(content) = entry.get("content").and_then(Value::as_str) else {
            return false;
        };
        let ending = entry
            .get("promptSubmitEnding")
            .and_then(|ending| ending.get("ending"))
            .and_then(|ending| ending.get("kind"))
            .and_then(Value::as_str);
        if ending != Some("blocked_by_hook") && !content.starts_with(BLOCKED_OPENER) {
            return false;
        }
        let Some(start) = content.len().checked_sub(self.prompt_len) else {
            return false;
        };
        let (Some(head), Some(prompt)) = (content.get(..start), content.get(start..)) else {
            return false;
        };
        head.ends_with(ORIGINAL_PROMPT) && sha256_hex(prompt.as_bytes()) == self.prompt_sha256
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What a witness is started with, on its command line. It holds attempt
/// names, a path and a prompt's digest and nonce, nothing secret: the caps
/// stay in the spool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub transcript: TranscriptMark,
    /// How long after it starts the witness stops watching, in
    /// milliseconds.
    pub within_ms: u64,
    /// The hook's doorbell batch, when the hook handed the witness its
    /// receipt connection (as stdin) to report it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<crate::inbox_oneshot::DeferredReport>,
    /// The next line on that connection has to start with a newline.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    /// A receipt the hook could not publish, for the batch's report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
    /// A once-waiter's attempts whose blocks the prompt itself carried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native: Vec<InboxAttemptRef>,
    /// Issue #2347: the prompt whose turn the witness settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<PromptTurn>,
    /// Issue #2346: the receipts sender the witness commits as, the hook's
    /// own: Claude Code's prompt hook, or Codex's hooks.
    #[serde(default = "claude_hooks")]
    pub sender: crate::event::InboxOneShotSender,
}

fn claude_hooks() -> crate::event::InboxOneShotSender {
    crate::event::InboxOneShotSender::ClaudeHooks
}

impl Plan {
    /// Every attempt the witness looks for.
    fn expected(&self) -> Vec<InboxAttemptRef> {
        let mut out: Vec<InboxAttemptRef> = Vec::new();
        let batch = self
            .batch
            .iter()
            .flat_map(|batch| batch.written.iter().chain(&batch.possible));
        for at in batch.chain(&self.native) {
            if !out.contains(at) {
                out.push(at.clone());
            }
        }
        out
    }
}

/// How long a witness may watch when the deck holds what the hook fetched
/// for `hold` from when the hook registered: well inside that hold, so the
/// witness reports before the deck gives the attempt up, and at most
/// [`WITNESS_MAX`].
pub fn window(hold: Duration) -> Duration {
    (hold * 2 / 3).min(WITNESS_MAX)
}

/// The texts of one transcript entry that are in the model's context because
/// the agent took them. For Claude Code: a `user` entry's prompt (a string,
/// or its `text` parts), and the context a hook added (`attachment` of type
/// `hook_additional_context`); nothing else counts: a `system` entry that
/// quotes a blocked prompt, a queued notification, a request log. For Codex
/// (issue #2346): the `input_text` parts of a `response_item` `message` of
/// role `user` (a prompt, or a `Stop` reason it went on with) or `developer`
/// (a hook's context); nothing else counts: the model's own messages, a tool
/// call's output, an `event_msg` echoing an item.
pub fn entry_texts(entry: &Value) -> Vec<&str> {
    match entry.get("type").and_then(Value::as_str) {
        Some("response_item") => entry
            .get("payload")
            .filter(|payload| {
                payload.get("type").and_then(Value::as_str) == Some("message")
                    && matches!(
                        payload.get("role").and_then(Value::as_str),
                        Some("user" | "developer")
                    )
            })
            .and_then(|payload| payload.get("content"))
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default(),
        Some("user") => entry
            .get("message")
            .and_then(|message| message.get("content"))
            .map(strings)
            .unwrap_or_default(),
        Some("attachment") => entry
            .get("attachment")
            .filter(|attachment| {
                attachment.get("type").and_then(Value::as_str) == Some("hook_additional_context")
            })
            .and_then(|attachment| attachment.get("content"))
            .map(strings)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A transcript value's strings: itself, or the strings and `text` parts
/// of an array.
fn strings(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.as_str()),
                Value::Object(part) if part.get("type").and_then(Value::as_str) == Some("text") => {
                    part.get("text").and_then(Value::as_str)
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The exact attempts one transcript line shows the agent took, whole
/// ([`crate::inbox_prompt::delivered`] over [`entry_texts`]).
pub fn witnessed_in_line(line: &[u8]) -> Vec<InboxAttemptRef> {
    if line.len() > MAX_LINE || !contains(line, NEEDLE) {
        return Vec::new();
    }
    let Ok(entry) = serde_json::from_slice::<Value>(line) else {
        return Vec::new();
    };
    let mut out: Vec<InboxAttemptRef> = Vec::new();
    for text in entry_texts(&entry) {
        for at in crate::inbox_prompt::delivered(text) {
            if !out.contains(&at) {
                out.push(at);
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Issue #2347: what the transcript has shown of the witnessed prompt's fate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFate {
    /// Nothing yet.
    Unknown,
    /// Claude took a prompt: a `user` entry that is not Claude's own meta
    /// note, an `assistant` entry, or a `UserPromptSubmit` hook's context
    /// came first. A turn is running, and its `Stop` ends it.
    Taken,
    /// Claude recorded this prompt as blocked by a hook before anything
    /// showed it took one: no turn runs.
    Blocked,
}

/// Reads what a transcript gains past a mark, a whole line at a time.
pub struct Scanner {
    mark: TranscriptMark,
    /// The transcript existed at some read.
    present: bool,
    partial: Vec<u8>,
    read: u64,
    skipping: bool,
    /// Issue #2347: the prompt whose fate is read, if any.
    turn: Option<PromptTurn>,
    fate: TurnFate,
}

impl Scanner {
    pub fn new(mark: TranscriptMark) -> Scanner {
        Scanner {
            mark,
            present: false,
            partial: Vec::new(),
            read: 0,
            skipping: false,
            turn: None,
            fate: TurnFate::Unknown,
        }
    }

    /// A scanner that also reads `turn`'s fate ([`Self::fate`]).
    pub fn with_turn(mark: TranscriptMark, turn: Option<PromptTurn>) -> Scanner {
        Scanner {
            turn,
            ..Scanner::new(mark)
        }
    }

    /// What the lines read so far show of the prompt's fate; always
    /// [`TurnFate::Unknown`] for a scanner with no prompt.
    pub fn fate(&self) -> TurnFate {
        self.fate
    }

    /// Issue #2347: settle the prompt's fate from one whole line, if it is
    /// still unknown.
    fn read_fate(&mut self, line: &[u8]) {
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        if self.fate != TurnFate::Unknown || line.len() > MAX_LINE {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        // A session's first prompt also creates its transcript, and Claude
        // writes the `SessionStart` hooks' context into it just ahead of
        // that prompt's own entries, taken or blocked (measured on 2.1.328):
        // that context says nothing about the prompt.
        let taken = match entry.get("type").and_then(Value::as_str) {
            Some("user") => entry.get("isMeta").and_then(Value::as_bool) != Some(true),
            Some("assistant") => true,
            Some("attachment") => entry.get("attachment").is_some_and(|attachment| {
                attachment.get("type").and_then(Value::as_str) == Some("hook_additional_context")
                    && attachment.get("hookEvent").and_then(Value::as_str)
                        == Some("UserPromptSubmit")
            }),
            _ => false,
        };
        if taken {
            self.fate = TurnFate::Taken;
        } else if turn.blocked_in(&entry) {
            self.fate = TurnFate::Blocked;
        }
    }

    /// Read what was appended since the last call and add every attempt its
    /// whole lines show to `found`. A transcript that shrank below the mark,
    /// or went, shows nothing more; reading stops at [`MAX_READ`].
    pub fn poll(&mut self, found: &mut Vec<InboxAttemptRef>) {
        let Some(mut file) = open_regular(&self.mark.path) else {
            return;
        };
        self.present = true;
        let start = self.mark.offset + self.read;
        if file.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        let room = MAX_READ.saturating_sub(self.read);
        let mut chunk = Vec::new();
        if file.take(room).read_to_end(&mut chunk).is_err() {
            return;
        }
        self.read += chunk.len() as u64;
        let mut rest = chunk.as_slice();
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            let (piece, tail) = rest.split_at(end);
            rest = &tail[1..];
            if self.skipping {
                self.skipping = false;
                self.partial.clear();
                continue;
            }
            self.partial.extend_from_slice(piece);
            for at in witnessed_in_line(&self.partial) {
                if !found.contains(&at) {
                    found.push(at);
                }
            }
            let line = std::mem::take(&mut self.partial);
            self.read_fate(&line);
        }
        if !self.skipping {
            self.partial.extend_from_slice(rest);
            if self.partial.len() > MAX_LINE {
                self.partial.clear();
                self.skipping = true;
            }
        }
    }
}

/// What [`watch`] saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watched {
    /// The attempts the transcript showed the agent took, by the deadline.
    Found(Vec<InboxAttemptRef>),
    /// The transcript never existed while the witness watched: Claude
    /// Code keeps none for this session, so nothing can witness.
    NoTranscript,
}

/// `path` opened for reading only if it is a regular file. The open does
/// not block: the path was accepted while it may not have existed, and a
/// FIFO created there in the meantime must not hold the witness past its
/// deadline.
fn open_regular(path: &std::path::Path) -> Option<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

/// Watch the transcript until every attempt in `expected` is found or
/// `deadline` passes.
pub fn watch(mark: TranscriptMark, expected: &[InboxAttemptRef], deadline: Instant) -> Watched {
    watch_with(&mut Scanner::new(mark), expected, deadline)
}

/// [`watch`] on a scanner the caller keeps reading afterwards. A prompt the
/// scanner reads as blocked ends the watch at once: nothing it carried can
/// reach the model any more.
pub fn watch_with(
    scanner: &mut Scanner,
    expected: &[InboxAttemptRef],
    deadline: Instant,
) -> Watched {
    let mut seen = Vec::new();
    loop {
        scanner.poll(&mut seen);
        let found: Vec<InboxAttemptRef> = expected
            .iter()
            .filter(|at| seen.contains(at))
            .cloned()
            .collect();
        let now = Instant::now();
        if found.len() == expected.len() || scanner.fate() == TurnFate::Blocked {
            return Watched::Found(found);
        }
        if now >= deadline {
            return if scanner.present {
                Watched::Found(found)
            } else {
                Watched::NoTranscript
            };
        }
        std::thread::sleep(POLL.min(deadline - now));
    }
}

/// Issue #2347: keep reading until the scanner's prompt is taken or
/// blocked, or `deadline` passes, and answer its fate. A transcript that has
/// not appeared by [`WITNESS_MAX`] after `started` is not waited for: the
/// session keeps none.
pub fn settle_turn(scanner: &mut Scanner, started: Instant, deadline: Instant) -> TurnFate {
    let mut ignored = Vec::new();
    loop {
        scanner.poll(&mut ignored);
        ignored.clear();
        let now = Instant::now();
        let gone = !scanner.present && now >= started + WITNESS_MAX;
        if scanner.fate() != TurnFate::Unknown || now >= deadline || gone {
            return scanner.fate();
        }
        let poll = if now < started + WITNESS_MAX {
            POLL
        } else {
            SLOW_POLL
        };
        std::thread::sleep(poll.min(deadline - now));
    }
}

/// Start the witness for `plan`, detached from the hook: its own process
/// group, nothing on the hook's stdout or stderr, so Claude Code goes on as
/// soon as the hook exits. `connection` is the hook's receipt socket, the
/// witness's stdin.
#[cfg(unix)]
pub fn spawn(
    plan: &Plan,
    connection: Option<std::os::unix::net::UnixStream>,
) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let exe = std::env::current_exe()?;
    let plan = serde_json::to_string(plan).map_err(std::io::Error::other)?;
    let stdin = match connection {
        Some(stream) => Stdio::from(std::os::fd::OwnedFd::from(stream)),
        None => Stdio::null(),
    };
    std::process::Command::new(exe)
        .args(["inbox", "witness", "--plan"])
        .arg(plan)
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

/// `dot-agent-deck inbox witness --plan <json>`: watch, commit what the agent
/// took, then report the hook's batch on the connection it was handed.
pub fn run(plan: &str) -> ExitCode {
    let Ok(plan) = serde_json::from_str::<Plan>(plan) else {
        return ExitCode::FAILURE;
    };
    #[cfg(unix)]
    {
        run_unix(plan);
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        let _ = plan;
        ExitCode::FAILURE
    }
}

#[cfg(unix)]
fn run_unix(plan: Plan) {
    use std::io::Write;
    use std::os::fd::AsFd;
    let started = Instant::now();
    let connection = plan.batch.as_ref().and_then(|_| {
        let fd = std::io::stdin().as_fd().try_clone_to_owned().ok()?;
        let stream = std::os::unix::net::UnixStream::from(fd);
        // Whatever the deck sends meanwhile is read and dropped, so its
        // writes never stall on a peer that is not reading.
        if let Ok(mut reader) = stream.try_clone() {
            std::thread::spawn(move || {
                let mut sink = [0u8; 4096];
                while matches!(reader.read(&mut sink), Ok(n) if n > 0) {}
            });
        }
        Some(stream)
    });
    let deadline = started + Duration::from_millis(plan.within_ms);
    let expected = plan.expected();
    let mut scanner = Scanner::with_turn(plan.transcript.clone(), plan.turn.clone());
    // A session that keeps no transcript can't witness anything: what the
    // hook wrote whole is committed then, as a hook with no transcript
    // commits it.
    let found = match watch_with(&mut scanner, &expected, deadline) {
        Watched::Found(found) => found,
        Watched::NoTranscript => plan
            .batch
            .iter()
            .flat_map(|batch| batch.written.iter())
            .chain(&plan.native)
            .cloned()
            .collect(),
    };
    // Nothing waits on the witness, so a commit the deck did not answer in
    // time, or whose connection was lost, is tried once more. What still
    // fails goes to the deck as the batch's `degraded` notice, which the
    // agent's card shows: its stderr reaches nobody (issue #2341 review).
    let commit = || {
        crate::inbox_prompt::commit_exact_quietly(
            found.clone(),
            plan.sender,
            Instant::now() + crate::inbox_prompt::prompt_receipts_wait(),
        )
    };
    let mut committed = commit();
    if matches!(
        committed,
        crate::inbox_prompt::Committed::Lost | crate::inbox_prompt::Committed::TimedOut
    ) {
        committed = commit();
    }
    let degraded = plan
        .degraded
        .clone()
        .or_else(|| crate::inbox_prompt::committed_notice(committed).map(str::to_string));
    if let (Some(batch), Some(mut stream)) = (plan.batch.as_ref(), connection) {
        let line = format!(
            "{}{}\n",
            if plan.cut { "\n" } else { "" },
            batch.control_line(degraded)
        );
        let _ = stream.set_write_timeout(Some(crate::inbox_oneshot::RECEIPTS_ANSWER_WAIT));
        let _ = stream.write_all(line.as_bytes());
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
    // Issue #2347: the batch is settled first, so whatever a dropped prompt
    // carried is waiting again by the time its turn reads over.
    if let Some(turn) = plan.turn.as_ref()
        && settle_turn(&mut scanner, started, started + TURN_WATCH_MAX) == TurnFate::Blocked
    {
        crate::hook::report_prompt_not_taken(turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "m-abcdefgh-0123456789abcdef";

    fn block(id: &str, a: u32, e: u32) -> String {
        format!(
            "[dot-agent-deck message {id} delivery {a}.{e} from the deck]\nbody\n[end of dot-agent-deck message {id}]"
        )
    }

    fn at(id: &str, a: u32, e: u32) -> InboxAttemptRef {
        InboxAttemptRef {
            id: id.to_string(),
            a,
            e,
        }
    }

    fn line(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    /// Scenario: Read transcript lines of every kind Claude Code writes around a prompt that carries a deck block: a hook's context attachment, a user prompt as a string and as text parts, a system entry quoting a blocked prompt, a queued notification, another attachment, and a request log. Only the hook context and the user prompts count, and only for a block that is whole.
    #[test]
    fn only_a_taken_prompt_or_a_hooks_context_witnesses_a_block() {
        let whole = block(ID, 1, 2);
        let cases = [
            (
                json!({"type":"attachment","attachment":{"type":"hook_additional_context","content":[format!("framing\n{whole}")],"hookEvent":"UserPromptSubmit"}}),
                true,
            ),
            (
                json!({"type":"user","message":{"role":"user","content":whole}}),
                true,
            ),
            (
                json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":whole}]}}),
                true,
            ),
            (
                json!({"type":"system","content":format!("UserPromptSubmit operation blocked by hook:\nno\n\nOriginal prompt: {whole}")}),
                false,
            ),
            (json!({"type":"queue-operation","content":whole}), false),
            (
                json!({"type":"attachment","attachment":{"type":"session_context","content":[whole]}}),
                false,
            ),
            (
                json!({"type":"api-request-blob","message":{"content":whole}}),
                false,
            ),
            (
                json!({"type":"user","message":{"content":format!("[dot-agent-deck message {ID} delivery 1.2 from x]\nno closer")}}),
                false,
            ),
            (
                json!({"type":"user","message":{"content":[{"type":"tool_result","content":whole}]}}),
                false,
            ),
        ];
        for (entry, counts) in cases {
            let found = witnessed_in_line(&line(entry.clone()));
            let want = if counts { vec![at(ID, 1, 2)] } else { vec![] };
            assert_eq!(found, want, "{entry}");
        }
        assert!(witnessed_in_line(b"{not json [dot-agent-deck message ").is_empty());
    }

    /// A Codex 0.164.0 rollout `response_item` message of `role` carrying
    /// `text`, as measured (issue #2346).
    fn codex_message(role: &str, text: &str, kind: &str, hook: bool) -> Value {
        json!({"timestamp":"2026-11-19T23:10:00.420Z","type":"response_item",
            "payload":{"type":"message","id":"msg_x","role":role,
                "content":[{"type":"input_text","text":text}],
                "internal_chat_message_metadata_passthrough":{"turn_id":"t",
                    "content_item_kinds":[kind],
                    "content_item_metadata":[{"provenance":{"type":if hook {"harness"} else {"user"},"hook":hook}}]}}})
    }

    /// Scenario: Read Codex rollout lines of every kind Codex 0.164.0 writes around a hand-over carrying a deck block: the prompt as a user message, a prompt hook's context as a developer message, a `Stop` reason it went on with as a user message wrapping the reason in `<hook_prompt>` with XML-escaped text, the model's own reply quoting the block, a tool's output, the `event_msg` items echoing the prompt and the `Stop` reason, and the `turn_aborted` note an interrupt leaves. Only the three messages the model took count, and only for a block that is whole.
    #[test]
    fn only_what_codex_took_witnesses_a_block() {
        let whole = block(ID, 1, 2);
        let escaped = format!(
            "<hook_prompt hook_run_id=\"stop:2:/h/hooks.json\">framing\n[dot-agent-deck message {ID} delivery 1.2 from a &lt;b&gt;&amp;\"x'&lt;/b&gt; deck]\nReply &lt;i&gt;&amp;amp;&lt;/i&gt;\n[end of dot-agent-deck message {ID}]</hook_prompt>"
        );
        let counted = [
            codex_message("user", &format!("a prompt {whole}"), "user.text", false),
            codex_message(
                "developer",
                &format!("framing\n{whole}"),
                "hooks.additional_context",
                true,
            ),
            codex_message("user", &escaped, "unknown", true),
        ];
        for entry in counted {
            assert_eq!(
                witnessed_in_line(&line(entry.clone())),
                vec![at(ID, 1, 2)],
                "{entry}"
            );
        }
        let not_counted = [
            json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":whole}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"developer",
                "content":[{"type":"output_text","text":whole}]}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output",
                "call_id":"c","output":whole}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage",
                "content":[{"type":"text","text":whole}]}}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"HookPrompt",
                "fragments":[{"text":whole,"hookRunId":"stop:2:/h/hooks.json"}]}}}),
            json!({"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}),
            codex_message(
                "user",
                &format!("[dot-agent-deck message {ID} delivery 1.2 from x]\nno closer"),
                "user.text",
                false,
            ),
        ];
        for entry in not_counted {
            assert!(
                witnessed_in_line(&line(entry.clone())).is_empty(),
                "{entry}"
            );
        }
    }

    /// Scenario: Read a witness plan written without a sender, as a hook from before issue #2346 starts one, and one written for Codex's hooks. The first commits as Claude Code's prompt hook; the second keeps Codex's hooks through its command line.
    #[test]
    fn a_plan_names_the_sender_it_commits_as() {
        let mark = TranscriptMark {
            path: PathBuf::from("/t.jsonl"),
            offset: 3,
        };
        let plan: Plan = serde_json::from_value(json!({"transcript":mark,"within_ms":5})).unwrap();
        assert_eq!(plan.sender, crate::event::InboxOneShotSender::ClaudeHooks);
        let codex = Plan {
            sender: crate::event::InboxOneShotSender::CodexHooks,
            ..plan
        };
        let back: Plan = serde_json::from_str(&serde_json::to_string(&codex).unwrap()).unwrap();
        assert_eq!(back, codex);
    }

    /// Scenario: A FIFO stands where the transcript was to be created. The scanner opens it without blocking, reads nothing from it, and a watch for an attempt still ends at its deadline.
    #[cfg(unix)]
    #[test]
    fn a_fifo_in_the_transcripts_place_does_not_hold_the_witness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        let watched = watch(
            TranscriptMark { path, offset: 0 },
            &[at(ID, 1, 1)],
            start + Duration::from_millis(300),
        );
        assert_eq!(watched, Watched::NoTranscript);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// Scenario: A transcript holds an earlier prompt carrying a block before the hook's mark, then gains a hook context for a second attempt in two writes, the first ending mid-line. The scanner reports only the attempt appended after the mark, and only once its line is complete.
    #[test]
    fn the_scanner_reads_only_whole_lines_after_the_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut before = line(json!({"type":"user","message":{"content":block(ID,1,1)}}));
        before.push(b'\n');
        std::fs::write(&path, &before).unwrap();
        let mark = TranscriptMark {
            path: path.clone(),
            offset: before.len() as u64,
        };
        let mut scanner = Scanner::new(mark);
        let mut found = Vec::new();
        scanner.poll(&mut found);
        assert!(found.is_empty(), "nothing before the mark counts");

        let mut after = line(
            json!({"type":"attachment","attachment":{"type":"hook_additional_context","content":[block(ID,1,2)]}}),
        );
        after.push(b'\n');
        let (head, tail) = after.split_at(after.len() / 2);
        let append = |bytes: &[u8]| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(bytes)
                .unwrap();
        };
        append(head);
        scanner.poll(&mut found);
        assert!(found.is_empty(), "half a line shows nothing");
        append(tail);
        scanner.poll(&mut found);
        assert_eq!(found, vec![at(ID, 1, 2)]);
    }

    /// Scenario: Watch for two attempts: one the transcript gains, and one it never does. The watch answers the first once the deadline passes; watching for the first alone answers as soon as its line is read; watching a transcript that never appears answers that there is none.
    #[test]
    fn watch_answers_what_it_found_by_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, b"").unwrap();
        let mark = TranscriptMark {
            path: path.clone(),
            offset: 0,
        };
        let mut entry = line(json!({"type":"user","message":{"content":block(ID,2,1)}}));
        entry.push(b'\n');
        std::fs::write(&path, &entry).unwrap();
        let start = Instant::now();
        let found = watch(
            mark.clone(),
            &[at(ID, 2, 1), at(ID, 2, 2)],
            start + Duration::from_millis(300),
        );
        assert_eq!(found, Watched::Found(vec![at(ID, 2, 1)]));
        assert!(start.elapsed() >= Duration::from_millis(300));
        let start = Instant::now();
        let found = watch(mark, &[at(ID, 2, 1)], start + Duration::from_secs(10));
        assert_eq!(found, Watched::Found(vec![at(ID, 2, 1)]));
        assert!(start.elapsed() < Duration::from_secs(5));
        let missing = TranscriptMark {
            path: dir.path().join("never.jsonl"),
            offset: 0,
        };
        assert_eq!(
            watch(
                missing,
                &[at(ID, 2, 1)],
                Instant::now() + Duration::from_millis(200)
            ),
            Watched::NoTranscript,
            "a transcript that never appears witnesses nothing"
        );
    }

    /// Scenario: Read the transcript mark of hook payloads naming an existing transcript, one not created yet (in a folder not created yet), a relative path, a path that is not a `.jsonl` file, a directory, and of one naming none. The existing file gives a mark at its current length and the missing one a mark at 0; the others give none.
    #[test]
    fn a_mark_needs_a_transcript_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, b"0123456789").unwrap();
        assert_eq!(
            TranscriptMark::of_payload(&json!({"transcript_path": path})),
            Some(TranscriptMark {
                path: path.clone(),
                offset: 10
            })
        );
        let missing = dir.path().join("not-yet").join("session.jsonl");
        assert_eq!(
            TranscriptMark::of_payload(&json!({"transcript_path": missing})),
            Some(TranscriptMark {
                path: missing.clone(),
                offset: 0
            })
        );
        let folder = dir.path().join("folder.jsonl");
        std::fs::create_dir(&folder).unwrap();
        for payload in [
            json!({"transcript_path": "t.jsonl"}),
            json!({"transcript_path": dir.path().join("t.txt")}),
            json!({"transcript_path": folder}),
            json!({}),
        ] {
            assert_eq!(TranscriptMark::of_payload(&payload), None, "{payload}");
        }
    }

    /// The `system` entry Claude Code 2.1.328 writes for `prompt` blocked by
    /// a hook that printed `reason` (measured, issue #2347).
    fn blocked_entry(reason: &str, prompt: &str) -> Value {
        json!({"type":"system","subtype":"informational","level":"warning",
            "content":format!("{BLOCKED_OPENER}:\n{reason}\n\n{ORIGINAL_PROMPT}{prompt}"),
            "preventContinuation":true,
            "promptSubmitEnding":{"ending":{"kind":"blocked_by_hook","reason":reason},"promptKept":false}})
    }

    /// Scenario: A prompt with leading, trailing and inner whitespace is blocked, and the transcript entry is read as Claude Code 2.1.328 writes it, for a hook's JSON decision and for an exit-2 hook (whose reason names the command and ends in an extra blank line), and as an older entry with only the opening words. Each is this prompt blocked; the same entry quoting another prompt, a longer prompt ending in it, the prompt with a character more, a user entry holding the prompt, and a system entry that is not a block are not.
    #[test]
    fn a_prompt_blocked_by_a_hook_is_recognised_as_this_very_prompt() {
        let prompt = "  first line\n\nsecond line with \t tab  \n";
        let turn = PromptTurn::new(prompt, "s", None).unwrap();
        assert_eq!(turn.nonce.len(), 32);
        assert_ne!(
            PromptTurn::new(prompt, "s", None).unwrap().nonce,
            turn.nonce
        );
        let mut legacy = blocked_entry("no", prompt);
        legacy.as_object_mut().unwrap().remove("promptSubmitEnding");
        for entry in [
            blocked_entry("json says no", prompt),
            blocked_entry("[/x/exit2.sh]: policy refused\n", prompt),
            legacy,
        ] {
            assert!(turn.blocked_in(&entry), "{entry}");
        }
        let mut not_a_block = blocked_entry("no", prompt);
        not_a_block["content"] = json!(format!("Something else\n\n{ORIGINAL_PROMPT}{prompt}"));
        not_a_block
            .as_object_mut()
            .unwrap()
            .remove("promptSubmitEnding");
        for entry in [
            blocked_entry("no", "another prompt"),
            blocked_entry("no", &format!("more {prompt}")),
            blocked_entry("no", &format!("{prompt}x")),
            json!({"type":"user","message":{"role":"user","content":prompt}}),
            not_a_block,
            json!({"type":"system","content":7}),
        ] {
            assert!(!turn.blocked_in(&entry), "{entry}");
        }
    }

    /// Scenario: Settle a prompt's fate from a transcript that gains, after the hook's mark, a `SessionStart` hook's context, a meta user entry and then its blocked entry, as a session's first prompt does; one that gains a user entry first and the blocked entry after; one that gains only a prompt hook's context; and one that gains nothing. The first is blocked at once and ends a watch for an attempt early, the second and third are taken, and the last is unknown when its deadline passes.
    #[test]
    fn a_prompts_fate_is_whatever_the_transcript_shows_first() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = "deck line";
        let turn = PromptTurn::new(prompt, "s", None).unwrap();
        let case = |name: &str, entries: &[Value]| {
            let path = dir.path().join(format!("{name}.jsonl"));
            let mut earlier = line(json!({"type":"user","message":{"content":"earlier"}}));
            earlier.push(b'\n');
            let mut bytes = earlier.clone();
            for entry in entries {
                bytes.extend(line(entry.clone()));
                bytes.push(b'\n');
            }
            std::fs::write(&path, &bytes).unwrap();
            Scanner::with_turn(
                TranscriptMark {
                    path,
                    offset: earlier.len() as u64,
                },
                Some(turn.clone()),
            )
        };
        let start = Instant::now();
        let mut blocked = case(
            "blocked",
            &[
                json!({"type":"attachment","attachment":{"type":"hook_additional_context",
                    "content":["deck framing"],"hookName":"SessionStart","hookEvent":"SessionStart"}}),
                json!({"type":"user","isMeta":true,"message":{"content":"caveat"}}),
                blocked_entry("no", prompt),
            ],
        );
        assert_eq!(
            watch_with(
                &mut blocked,
                &[at(ID, 1, 1)],
                start + Duration::from_secs(10)
            ),
            Watched::Found(vec![])
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a block ends the watch"
        );
        assert_eq!(blocked.fate(), TurnFate::Blocked);
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut taken = case(
            "taken",
            &[
                json!({"type":"user","message":{"content":prompt}}),
                blocked_entry("no", prompt),
            ],
        );
        assert_eq!(settle_turn(&mut taken, start, deadline), TurnFate::Taken);
        let mut context = case(
            "context",
            &[
                json!({"type":"attachment","attachment":{"type":"hook_additional_context",
                "content":["x"],"hookEvent":"UserPromptSubmit"}}),
            ],
        );
        assert_eq!(settle_turn(&mut context, start, deadline), TurnFate::Taken);
        let mut quiet = case("quiet", &[json!({"type":"queue-operation"})]);
        assert_eq!(settle_turn(&mut quiet, start, deadline), TurnFate::Unknown);
        assert!(Instant::now() >= deadline);
        let mut none = Scanner::new(TranscriptMark {
            path: dir.path().join("none.jsonl"),
            offset: 0,
        });
        none.poll(&mut Vec::new());
        assert_eq!(
            none.fate(),
            TurnFate::Unknown,
            "a scanner with no prompt reads none"
        );
    }

    /// Scenario: Size the witness's window for the default hold and for a short one a test sets. It stays two thirds of the hold, and never past twenty seconds.
    #[test]
    fn the_window_stays_inside_the_hold() {
        assert_eq!(window(Duration::from_secs(60)), WITNESS_MAX);
        assert_eq!(
            window(Duration::from_millis(1500)),
            Duration::from_millis(1000)
        );
    }
}
