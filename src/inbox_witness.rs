//! Issue #2341: Claude Code's prompt hook commits a deck message only once
//! the session transcript shows Claude took the prompt that carried it.
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

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::InboxAttemptRef;

/// The longest a witness watches a transcript.
pub const WITNESS_MAX: Duration = Duration::from_secs(20);

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
    /// The mark of a Claude Code hook payload's `transcript_path`: an
    /// absolute path to a `.jsonl` file, at its current length, or at 0 when
    /// it does not exist yet (a session's first prompt: Claude Code creates
    /// the transcript, and its folder, only once it records that prompt,
    /// blocked or taken). `None` for anything else, and the hook then commits
    /// what it writes itself (module docs).
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

/// What a witness is started with, on its command line. It holds attempt
/// names and a path, nothing secret: the caps stay in the spool.
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
/// Claude took them: a `user` entry's prompt (a string, or its `text`
/// parts), and the context a hook added (`attachment` of type
/// `hook_additional_context`). Nothing else counts: a `system` entry that
/// quotes a blocked prompt, a queued notification, a request log.
pub fn entry_texts(entry: &Value) -> Vec<&str> {
    match entry.get("type").and_then(Value::as_str) {
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

/// The exact attempts one transcript line shows Claude took, whole
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

/// Reads what a transcript gains past a mark, a whole line at a time.
pub struct Scanner {
    mark: TranscriptMark,
    /// The transcript existed at some read.
    present: bool,
    partial: Vec<u8>,
    read: u64,
    skipping: bool,
}

impl Scanner {
    pub fn new(mark: TranscriptMark) -> Scanner {
        Scanner {
            mark,
            present: false,
            partial: Vec::new(),
            read: 0,
            skipping: false,
        }
    }

    /// Read what was appended since the last call and add every attempt its
    /// whole lines show to `found`. A transcript that shrank below the mark,
    /// or went, shows nothing more; reading stops at [`MAX_READ`].
    pub fn poll(&mut self, found: &mut Vec<InboxAttemptRef>) {
        let Ok(mut file) = std::fs::File::open(&self.mark.path) else {
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
            self.partial.clear();
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
    /// The attempts the transcript showed Claude took, by the deadline.
    Found(Vec<InboxAttemptRef>),
    /// The transcript never existed while the witness watched: Claude
    /// Code keeps none for this session, so nothing can witness.
    NoTranscript,
}

/// Watch the transcript until every attempt in `expected` is found or
/// `deadline` passes.
pub fn watch(mark: TranscriptMark, expected: &[InboxAttemptRef], deadline: Instant) -> Watched {
    let mut scanner = Scanner::new(mark);
    let mut seen = Vec::new();
    loop {
        scanner.poll(&mut seen);
        let found: Vec<InboxAttemptRef> = expected
            .iter()
            .filter(|at| seen.contains(at))
            .cloned()
            .collect();
        let now = Instant::now();
        if found.len() == expected.len() {
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

/// `dot-agent-deck inbox witness --plan <json>`: watch, commit what Claude
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
    // A session that keeps no transcript can't witness anything: what the
    // hook wrote whole is committed then, as a hook with no transcript
    // commits it.
    let found = match watch(plan.transcript.clone(), &expected, deadline) {
        Watched::Found(found) => found,
        Watched::NoTranscript => plan
            .batch
            .iter()
            .flat_map(|batch| batch.written.iter())
            .chain(&plan.native)
            .cloned()
            .collect(),
    };
    crate::inbox_prompt::commit_exact(
        found,
        crate::event::InboxOneShotSender::ClaudeHooks,
        Instant::now() + crate::inbox_prompt::prompt_receipts_wait(),
    );
    if let (Some(batch), Some(mut stream)) = (plan.batch.as_ref(), connection) {
        let line = format!(
            "{}{}\n",
            if plan.cut { "\n" } else { "" },
            batch.control_line(plan.degraded.clone())
        );
        let _ = stream.set_write_timeout(Some(crate::inbox_oneshot::RECEIPTS_ANSWER_WAIT));
        let _ = stream.write_all(line.as_bytes());
        let _ = stream.shutdown(std::net::Shutdown::Write);
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
