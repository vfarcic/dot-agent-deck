//! Bounded, provenance-checked ingestion for the hook socket (issues #319 and
//! #318).
//!
//! The hook socket is the daemon's one unauthenticated inbound surface. It is
//! owner-only on disk (`0600` on Unix, a current-user DACL on Windows), so the
//! threat model here is a **same-user process**, never a remote one. Two things
//! were missing, and they are fixed together because they are the same
//! sentence: *what the daemon will read* and *whose word it will take for it*.
//!
//! # What the daemon will read (#319)
//!
//! [`run_hook_loop`](crate::daemon) used to iterate `BufReader::lines()`, which
//! accumulates a newline-free payload without limit — a peer that writes bytes
//! and never a `\n` grows daemon memory until the process dies — and it spawned
//! one task per accepted connection with no cap on how many were outstanding.
//! [`BoundedLines`] closes the first; [`MAX_HOOK_CONNECTIONS`] closes the
//! second. Fixing only one leaves the other open: a thousand connections each
//! holding a bounded buffer is the same exhaustion by another route.
//!
//! # Whose word the daemon takes for it (#318)
//!
//! Before this, `dot-agent-deck agent-event` read `DOT_AGENT_DECK_PANE_ID` from
//! its own environment and the daemon forwarded that claim verbatim, so any
//! same-user process could run
//!
//! ```text
//! DOT_AGENT_DECK_PANE_ID=<someone-elses-pane> dot-agent-deck agent-event --type running
//! ```
//!
//! and drive another pane's card. Being able to *connect* was treated as
//! authority to speak for *any* pane.
//!
//! The fix is a per-agent capability token ([`AgentToken`]) that the daemon
//! mints from the OS CSPRNG at spawn, injects into the child's environment as
//! `DOT_AGENT_DECK_AGENT_TOKEN`, and **resolves back to its own
//! `(pane_id, agent_id)`** at ingest — see [`admit`]. The event's claimed
//! `pane_id`/`agent_id` are overwritten by what the token resolves to, never
//! compared against it: a valid token for pane A on an event naming pane B must
//! not drive pane B, and "compare and trust" gets that wrong the moment the
//! comparison is skipped, reordered, or made lenient.
//!
//! ## Fail closed for protected panes, open for foreign ones
//!
//! An event with no token, or with an unrecognized one, may not name a pane
//! this daemon **protects** — it is refused ([`Provenance::Refused`]). The same
//! event naming a pane the daemon does *not* protect still registers a foreign
//! card exactly as before ([`Provenance::Foreign`]). That asymmetry is
//! deliberate and is issue #601's named remainder: external agents posting into
//! a deck they were not spawned by keep working, and `managed_pane_ids` is
//! still not an ownership proof — which is precisely why nothing here is built
//! on it.
//!
//! "Protected" is deliberately a **wider** question than "is an agent running
//! on it" — see [`PaneAuthority::pane_is_protected`]. A pane whose agent has
//! exited but whose record still lingers is protected, because the daemon's
//! older ownership layer will still accept an event for it; making provenance
//! ask the narrower routing question is what left re-audit finding 1's
//! token-less spoofing window open after every natural exit.
//!
//! ## What this does not claim
//!
//! A same-user process can read another process's environment
//! (`/proc/<pid>/environ` on Linux). So this is **provenance binding plus a
//! raised bar, not authentication against an adversary who already has code
//! execution as this user** — the trade issue #401 records as its option 1.
//! Peer-PID (`crate::platform::peercred`) would resist a token-reading
//! attacker, but it does not cover wrapper or detached hook runners whose
//! process chain back to the pane's PTY leader is broken, and it puts a
//! live-process-table dependency on the ingest path; it is the candidate
//! strengthening tracked under #543, not a substitute for this.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

// ---------------------------------------------------------------------------
// Bounds (#319)
// ---------------------------------------------------------------------------

/// Hard cap on one newline-terminated hook line, in bytes.
///
/// **64 KiB.** The number is chosen from what a real event actually weighs
/// against what the daemon can absorb, and it is deliberately not the 16 MiB
/// [`crate::daemon_protocol::MAX_FRAME_LEN`] the attach socket uses — that
/// frame carries PTY scrollback snapshots, and a hook line carries a small
/// structured record.
///
/// Measured against the fields the producers actually emit
/// ([`crate::hook::build_claude_event`], [`crate::wrap`], the `agent-event`
/// CLI), an ordinary event is **300–900 bytes**: a 36-byte `session_id`, a pane
/// id bounded to [`crate::agent_pty::PANE_ID_ENV_MAX_LEN`], a `tool_detail`
/// already truncated to 120 chars by `hook::extract_tool_detail`, and a
/// `user_prompt` already truncated to
/// [`crate::prompt_delivery::USER_PROMPT_MAX_LEN`] (200 chars). The one field
/// with no producer-side cap is `metadata["bash_command"]`, the untruncated
/// command behind a `ToolStart` — a heredoc-bearing shell one-liner is the
/// realistic tail, and a few KiB is a large one.
///
/// 64 KiB is therefore roughly **70x a typical event** and still an order of
/// magnitude above the worst legitimate `bash_command` anyone has written,
/// while bounding the daemon's exposure to
/// `MAX_HOOK_LINE_BYTES * MAX_HOOK_CONNECTIONS` = **8 MiB** — less than a
/// single attach frame the daemon already accepts. A peer that exceeds it has
/// its **connection closed**; the bytes are never truncated and re-parsed,
/// because a truncated JSON line is not a valid event and truncate-then-parse
/// would let a peer smuggle a short valid event in behind megabytes of padding.
pub const MAX_HOOK_LINE_BYTES: usize = 64 * 1024;

/// Hard cap on how many accepted hook connections may be in flight at once.
///
/// **128.** Hook connections are short-lived by construction — the `hook` and
/// `agent-event` CLIs connect, write one line and exit — so the steady-state
/// count for a real fleet is a handful even when every agent fires at once. The
/// long-lived ones are the reply-bearing verbs on this same socket
/// (`Delegate`, `Dispatch`, `GetSeed`, `ListTargets`), and a `Dispatch` holds
/// its connection across a `git worktree` add plus a spawn — seconds, not
/// milliseconds. 128 leaves room for every role of several concurrent
/// orchestrations to be mid-dispatch while a whole fleet's hooks land, which is
/// well past anything observed.
///
/// The cap is enforced with a [`tokio::sync::Semaphore`] acquired *before* the
/// per-connection task is spawned, and an excess connection is **closed
/// promptly rather than queued**: the accept loop must stay responsive, and the
/// daemon must keep ingesting from connections already inside the cap. Queueing
/// would convert a connection flood into unbounded pending state, which is the
/// exhaustion this cap exists to prevent.
pub const MAX_HOOK_CONNECTIONS: usize = 128;

/// The budget the two constants above claim, held at COMPILE time rather than by
/// a test — the numbers are the security property, and a regression should not
/// be able to reach a test runner. Widening either constant past this fails the
/// build and forces the reasoning in their docs to be re-argued.
const _: () = {
    assert!(
        MAX_HOOK_LINE_BYTES >= 16 * 1024,
        "the line cap must stay comfortably above a large metadata[\"bash_command\"]",
    );
    assert!(
        MAX_HOOK_LINE_BYTES * MAX_HOOK_CONNECTIONS <= crate::daemon_protocol::MAX_FRAME_LEN,
        "the worst-case buffered total must stay within one attach frame the daemon already accepts",
    );
    assert!(
        MAX_HOOK_CONNECTIONS > 0,
        "the cap must admit at least one peer"
    );
};

/// How long the daemon will wait for ONE complete hook line before closing the
/// connection.
///
/// **30 seconds**, and this is the other half of [`MAX_HOOK_CONNECTIONS`]
/// rather than a separate idea. A cap without a deadline converts a
/// memory-exhaustion bug into a function-denial bug: the permit is held for the
/// whole connection, so 128 peers that connect and send *nothing* — or trickle
/// bytes forever under the line cap — deny every later hook, `GetSeed`,
/// `Delegate`, `Dispatch`, `WorkDone` and `ListTargets` connection, with no
/// memory pressure and no natural recovery. The cap alone does not close issue
/// #319; the pair does.
///
/// The deadline is on the WHOLE call, not on one `read`. A per-read idle
/// timeout resets on every byte moved, so it never fires against a peer that
/// dribbles one byte every 29 seconds — the exact shape the line cap already
/// refuses to be beaten by, and it would be odd for the two bounds to disagree
/// about what "slow" means. [`crate::hook::request_from_socket`] measures its
/// own client-side budget the same way, for the same reason.
///
/// **Why 30 s and not less.** Every legitimate producer writes its line in a
/// single `write_all` immediately after `connect` and then half-closes: the
/// `hook` and `agent-event` CLIs, `wrap`, and the reply-bearing verbs alike.
/// The genuinely long-lived case is a `Dispatch` that holds its connection
/// across a `git worktree` add plus a spawn — but that time is spent inside the
/// daemon's own handler, *not* waiting for a line, so it is not measured here
/// at all. 30 s is therefore some three orders of magnitude above the only
/// legitimate wait there is (a local `connect`-to-`write` latency), which
/// leaves ample room for a loaded machine while bounding a wedged permit to
/// half a minute instead of forever.
///
/// **Why not less still.** The deadline's job is to guarantee recovery, not to
/// be tight: an attacker who reconnects every 30 s can still hold the cap, and
/// no achievable value changes that (they can hold it by sending valid events
/// too). What the deadline buys is that the wedge cannot outlive the attacker,
/// and a legitimate peer is never cut off mid-line.
pub const HOOK_LINE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the daemon will spend writing ONE reply on a hook connection before
/// giving up and closing it.
///
/// **10 seconds.** The reply-bearing verbs (`Delegate`, `GetSeed`,
/// `ListTargets`) write a few hundred bytes to a local socket, which completes
/// in microseconds unless the peer has stopped reading — and a peer that has
/// stopped reading can otherwise pin a permit indefinitely once its receive
/// buffer fills, which is [`HOOK_LINE_TIMEOUT`]'s denial with the direction
/// reversed. Shorter than the read deadline because there is even less
/// legitimate reason to be slow here: the daemon already has the bytes.
pub const HOOK_REPLY_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Test-only knob that SHORTENS [`HOOK_LINE_TIMEOUT`] and
/// [`HOOK_REPLY_WRITE_TIMEOUT`], in milliseconds.
///
/// Same shape as [`crate::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`]:
/// read from the daemon's own environment, absent in production. It exists so
/// an L2 test can prove that a wedged connection cap genuinely RECOVERS at the
/// real socket, which a 30-second default makes untestable in the tier.
///
/// **It can only shorten.** [`hook_line_timeout`] clamps the override to the
/// production constant, so no environment can relax a bound — the knob makes
/// the daemon stricter or does nothing, and there is no value of it that
/// reintroduces the denial these deadlines close.
pub const DOT_AGENT_DECK_TEST_HOOK_TIMEOUT_MS: &str = "DOT_AGENT_DECK_TEST_HOOK_TIMEOUT_MS";

fn test_timeout_override() -> Option<Duration> {
    std::env::var(DOT_AGENT_DECK_TEST_HOOK_TIMEOUT_MS)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_millis)
}

/// The effective read deadline: [`HOOK_LINE_TIMEOUT`], or a shorter test
/// override. Never longer — see [`DOT_AGENT_DECK_TEST_HOOK_TIMEOUT_MS`].
pub fn hook_line_timeout() -> Duration {
    test_timeout_override()
        .map(|o| o.min(HOOK_LINE_TIMEOUT))
        .unwrap_or(HOOK_LINE_TIMEOUT)
}

/// The effective reply-write deadline. Same clamp as [`hook_line_timeout`].
pub fn hook_reply_write_timeout() -> Duration {
    test_timeout_override()
        .map(|o| o.min(HOOK_REPLY_WRITE_TIMEOUT))
        .unwrap_or(HOOK_REPLY_WRITE_TIMEOUT)
}

/// Hard cap on how much of a raw hook **payload prefix** the daemon will ever
/// put in its log.
///
/// **512 bytes**, plus the short `…[truncated, N bytes total]` suffix
/// [`truncate_for_log`] appends — so the logged *value* is a 512-byte prefix
/// plus that suffix, deliberately a little over 512, never the whole line.
///
/// Audit finding 9: every malformed line was logged verbatim, up to the full
/// [`MAX_HOOK_LINE_BYTES`], so a peer looping malformed lines grew
/// `~/.local/state/dot-agent-deck/deck.log` at 64 KiB a line. 512 bytes is over
/// half of even the largest ordinary event (300–900 bytes is the measured
/// range, and the head of one is where the diagnostic value is), and 128x
/// cheaper in the pathological case.
pub const MAX_LOGGED_HOOK_LINE_BYTES: usize = 512;

/// Prepare an **unparseable** raw hook line for the daemon log: blank any
/// capability token textually, then bound the length.
///
/// Audit finding 6, and re-audit finding 2 for the split between this and
/// [`redact_decoded_for_log`]. Two branches in [`crate::daemon`] log the
/// payload — the unknown-`event_type` diagnostic and the malformed-event
/// warning — and both ran *after* [`admit`] had stripped the typed token, so
/// they were the one place a live capability could still reach the disk. That
/// defeats [`AgentToken`]'s redacted [`fmt::Debug`] entirely, and a log file is
/// a longer-lived and easier source than `/proc/<pid>/environ`: it outlives the
/// agent process, it is plain text, and it is exactly what gets attached to a
/// bug report.
///
/// # This is for the branch that has nothing but bytes
///
/// The malformed branch is reached *because* the line did not parse, so there
/// is no decoded value to project and a textual scan is the only redaction
/// available. It degrades gracefully on a truncated or almost-JSON line, which
/// is exactly the input it gets.
///
/// **It is textual, so it matches only the literal `"agent_token"` spelling.**
/// JSON has infinitely many equivalent spellings of that member name — write
/// any letter as a `\u`-escape and `"agent_token"` decodes to the same
/// key — and a scanner that is not a JSON parser cannot see them. On this
/// branch that is sound: a line that does
/// not parse never reached [`admit`], so nothing here was ever *honoured* as a
/// capability — the redaction is best-effort hygiene over bytes the daemon
/// rejected. The branch where a spelling like that IS honoured is the decoded
/// one, and it uses [`redact_decoded_for_log`], which asks serde rather than
/// the byte stream.
pub fn redact_for_log(line: &str) -> String {
    let mut out = String::with_capacity(line.len().min(MAX_LOGGED_HOOK_LINE_BYTES) + 32);
    let mut rest = line;
    // `"agent_token"` as it appears on the wire; the field is `#[serde(rename)]`-free
    // so the Rust name IS the JSON name.
    const FIELD: &str = "\"agent_token\"";
    while let Some(at) = rest.find(FIELD) {
        let (before, from_field) = rest.split_at(at);
        out.push_str(before);
        out.push_str(FIELD);
        let after_field = &from_field[FIELD.len()..];
        match redact_json_string_value(after_field) {
            Some(consumed) => {
                out.push_str(":\"<redacted>\"");
                rest = &after_field[consumed..];
            }
            None => {
                // No `: "…"` follows — the field name appeared in some other
                // position (inside a string, or the line is truncated right
                // after it). Nothing to redact; keep scanning past it.
                rest = after_field;
            }
        }
    }
    out.push_str(rest);
    truncate_for_log(out)
}

/// From just past a `"agent_token"` field name, how many bytes make up
/// `: "<value>"`, or `None` when that is not what follows.
fn redact_json_string_value(after_field: &str) -> Option<usize> {
    let mut bytes = after_field.char_indices();
    let mut idx = loop {
        let (i, c) = bytes.next()?;
        match c {
            c if c.is_whitespace() => continue,
            ':' => break i + 1,
            _ => return None,
        }
    };
    // Skip whitespace between the colon and the opening quote.
    let opened = loop {
        let c = after_field[idx..].chars().next()?;
        if c.is_whitespace() {
            idx += c.len_utf8();
            continue;
        }
        if c != '"' {
            return None;
        }
        break idx + 1;
    };
    // Find the closing quote, honouring backslash escapes so a token containing
    // an escaped quote cannot end the span early. Tokens are hex today, but the
    // field is `Option<String>` from the wire and takes whatever a peer sends.
    let mut i = opened;
    let mut escaped = false;
    for c in after_field[opened..].chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(i + 1);
        }
        i += c.len_utf8();
    }
    // Unterminated string: the rest of the line IS the value, so consume it all
    // rather than leaving a live token in the tail.
    Some(after_field.len())
}

/// Every top-level member name [`crate::event::AgentEvent`] puts on the wire,
/// **except** `agent_token`.
///
/// The allowlist [`redact_decoded_for_log`] projects a decoded payload onto. It
/// is pinned against the real serialization by
/// `the_log_allowlist_is_exactly_the_wire_shape_minus_the_token`, so a field
/// added to `AgentEvent` cannot silently start being dropped from the log — and
/// a field *renamed to* `agent_token` cannot silently start being kept.
const LOGGABLE_EVENT_MEMBERS: &[&str] = &[
    "session_id",
    "agent_type",
    "event_type",
    "tool_name",
    "tool_detail",
    "cwd",
    "timestamp",
    "user_prompt",
    "metadata",
    "pane_id",
    "agent_id",
    "agent_version",
    "schema_version",
    "live_target",
];

/// Prepare a hook line that **did** decode into an [`crate::event::AgentEvent`]
/// for the daemon log: re-render it from the parsed JSON, keeping only the
/// members the event type actually has and dropping `agent_token` outright.
///
/// Re-audit finding 2. [`redact_for_log`] scans for the literal substring
/// `"agent_token"`, and JSON member names have equivalent escaped spellings:
/// `{"agent_\u0074oken":"…"}` decodes to the key `agent_token`, so `serde_json`
/// populates [`crate::event::AgentEvent::agent_token`], [`admit`] resolves and
/// strips it as a real capability — and the raw line the textual scanner sees
/// contains no matching substring. The token was then logged verbatim by the
/// unknown-`event_type` branch. Keeping the payload under
/// [`MAX_LOGGED_HOOK_LINE_BYTES`] made truncation irrelevant.
///
/// Asking `serde_json` closes that by construction: the parser resolves every
/// escape before this code sees a key, so every spelling of the member name
/// arrives here as the one string `agent_token` and is dropped with it. The
/// projection is an **allowlist**, not a denylist of that one name, so a token
/// smuggled under an unexpected member (`{"x":"…"}`) is dropped too — those
/// members are not part of the event and carry no diagnostic value.
///
/// # What this deliberately does not claim
///
/// A sender that puts a token in a *typed* string field — `session_id`,
/// `tool_detail`, a `metadata` value — still has it logged, and no redaction
/// short of guessing at token-shaped strings could tell that apart from real
/// data. That is a different property: those bytes are not honoured as a
/// capability by anything, so the daemon is not converting a live credential
/// into a log entry, it is echoing a string a same-user peer chose to send. The
/// property this protects is the one F6 named — **the daemon must not log the
/// field it accepts as a token** — and it now holds for every JSON spelling of
/// that field.
pub fn redact_decoded_for_log(line: &str) -> String {
    let Ok(serde_json::Value::Object(members)) = serde_json::from_str::<serde_json::Value>(line)
    else {
        // Unreachable from the daemon's decoded branch (a line that became an
        // `AgentEvent` is a JSON object), but this is a logging path: fall back
        // to the textual redaction rather than dropping the diagnostic.
        return redact_for_log(line);
    };
    let kept: serde_json::Map<String, serde_json::Value> = members
        .into_iter()
        .filter(|(name, _)| LOGGABLE_EVENT_MEMBERS.contains(&name.as_str()))
        .collect();
    match serde_json::to_string(&serde_json::Value::Object(kept)) {
        Ok(rendered) => truncate_for_log(rendered),
        // A `Value` that came out of the parser always serializes again; if it
        // somehow did not, say so rather than falling back to the raw line.
        Err(_) => "<event could not be re-rendered for the log>".to_string(),
    }
}

/// Bound a log fragment to [`MAX_LOGGED_HOOK_LINE_BYTES`] on a char boundary,
/// saying how much was dropped so a reader is never misled into thinking they
/// are looking at the whole line.
///
/// The returned string is a **512-byte prefix plus the suffix**, so it is
/// slightly longer than the cap by design — see [`MAX_LOGGED_HOOK_LINE_BYTES`].
fn truncate_for_log(mut s: String) -> String {
    if s.len() <= MAX_LOGGED_HOOK_LINE_BYTES {
        return s;
    }
    let total = s.len();
    let mut cut = MAX_LOGGED_HOOK_LINE_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(&format!("…[truncated, {total} bytes total]"));
    s
}

/// Why [`BoundedLines::next_line`] gave up on a connection.
#[derive(Debug)]
pub enum HookLineError {
    /// The peer sent more than [`MAX_HOOK_LINE_BYTES`] without a newline. The
    /// caller closes the connection; nothing is parsed.
    TooLong {
        /// The cap that was exceeded, for the log line.
        limit: usize,
    },
    /// The bytes before the newline were not valid UTF-8, or the socket read
    /// failed. Same outcome — the connection is closed.
    Io(std::io::Error),
    /// No complete line arrived within [`HOOK_LINE_TIMEOUT`] — the peer
    /// connected and sent nothing, or trickled under the cap without ever
    /// completing a line. Same outcome again, and it is what stops a permit
    /// being held forever.
    Idle {
        /// The deadline that expired, for the log line.
        after: Duration,
    },
}

impl fmt::Display for HookLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { limit } => {
                write!(f, "hook line exceeded {limit} bytes with no newline")
            }
            Self::Io(e) => write!(f, "hook read failed: {e}"),
            Self::Idle { after } => {
                write!(f, "no complete hook line within {after:?}")
            }
        }
    }
}

impl std::error::Error for HookLineError {}

/// A newline-delimited reader with a hard per-line byte cap.
///
/// Drop-in replacement for `tokio::io::BufReader::new(r).lines()` on the hook
/// socket, with one behavioural difference and one deliberate similarity:
///
/// - **Difference:** a line that reaches [`MAX_HOOK_LINE_BYTES`] without a
///   newline ends the stream with [`HookLineError::TooLong`] instead of growing
///   the buffer. The partial bytes are dropped, never parsed.
/// - **Similarity:** a final line with no trailing newline is still returned
///   (the EOF case), and a trailing `\r` is stripped, exactly as
///   `tokio::io::Lines` does — so a CRLF-writing peer is unaffected by the
///   swap.
pub struct BoundedLines<R> {
    inner: R,
    /// Bytes read from the socket that have not yet been consumed as a line.
    /// Never allowed to exceed `limit` — see [`Self::next_line`].
    buf: Vec<u8>,
    /// How much of `buf` has been scanned for a newline already, so a slow
    /// trickle of bytes does not re-scan the accumulated prefix on every read.
    scanned: usize,
    limit: usize,
    eof: bool,
}

impl<R: AsyncRead + Unpin> BoundedLines<R> {
    /// Wrap `inner` with the production [`MAX_HOOK_LINE_BYTES`] cap.
    pub fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_HOOK_LINE_BYTES)
    }

    /// Wrap `inner` with an explicit cap. Only the unit tests below use a
    /// non-production limit — the daemon always goes through [`Self::new`], so
    /// there is no way to widen the bound from outside this module.
    pub fn with_limit(inner: R, limit: usize) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            scanned: 0,
            limit,
            eof: false,
        }
    }

    /// [`Self::next_line`] with a deadline on the WHOLE call — the production
    /// entry point.
    ///
    /// Issue #319 / audit finding 2: the byte cap bounds what one line may
    /// weigh, and this bounds how long the connection may take to produce one.
    /// Without it a peer that never writes a newline (or never writes at all)
    /// holds its [`MAX_HOOK_CONNECTIONS`] permit until the daemon dies, so 128
    /// such peers deny the hook socket entirely — a function-denial with no
    /// memory pressure and no natural recovery. See [`HOOK_LINE_TIMEOUT`] for
    /// why the deadline covers the whole call rather than one `read`.
    ///
    /// Cancelling mid-read discards whatever partial line had accumulated,
    /// which is correct: the caller closes the connection, and a partial line
    /// was never going to be parsed.
    pub async fn next_line_within(
        &mut self,
        budget: Duration,
    ) -> Result<Option<String>, HookLineError> {
        match tokio::time::timeout(budget, self.next_line()).await {
            Ok(result) => result,
            Err(_) => Err(HookLineError::Idle { after: budget }),
        }
    }

    /// Read the next line, or `Ok(None)` at end of stream.
    ///
    /// The cap counts the line's own bytes, **not** its terminating `\n`, so a
    /// line of exactly `limit` bytes is legal and `limit + 1` is not. It is
    /// checked on the ACCUMULATED buffer rather than on a single read, so a peer
    /// cannot get past it by writing one byte at a time.
    pub async fn next_line(&mut self) -> Result<Option<String>, HookLineError> {
        loop {
            if let Some(nl) = find_newline(&self.buf, self.scanned) {
                // `nl` is the line's length in bytes, the newline excluded.
                if nl > self.limit {
                    return Err(HookLineError::TooLong { limit: self.limit });
                }
                let mut line = self.buf.drain(..=nl).collect::<Vec<u8>>();
                line.pop(); // the '\n'
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.scanned = 0;
                return decode(line).map(Some);
            }
            self.scanned = self.buf.len();

            if self.eof {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                if self.buf.len() > self.limit {
                    return Err(HookLineError::TooLong { limit: self.limit });
                }
                // EOF with no trailing newline: the bytes we have ARE the line.
                let mut line = std::mem::take(&mut self.buf);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.scanned = 0;
                return decode(line).map(Some);
            }

            // No newline yet and already past the cap: the line is over-long
            // whatever arrives next, so refuse now rather than reading more.
            if self.buf.len() > self.limit {
                return Err(HookLineError::TooLong { limit: self.limit });
            }
            // Never ask for more than would take the buffer to `limit + 1` —
            // one byte past the cap is all it takes to decide, and it keeps the
            // buffer from holding a peer's overshoot even transiently. The
            // 8 KiB ceiling keeps the ordinary case to a couple of reads.
            let want = (self.limit - self.buf.len() + 1).min(8 * 1024);
            let mut chunk = vec![0u8; want];
            let n = self
                .inner
                .read(&mut chunk)
                .await
                .map_err(HookLineError::Io)?;
            if n == 0 {
                self.eof = true;
                continue;
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn find_newline(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)
        .and_then(|tail| tail.iter().position(|b| *b == b'\n'))
        .map(|off| from + off)
}

fn decode(line: Vec<u8>) -> Result<String, HookLineError> {
    String::from_utf8(line).map_err(|e| {
        HookLineError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })
}

// ---------------------------------------------------------------------------
// Provenance (#318)
// ---------------------------------------------------------------------------

/// Environment variable carrying an agent's hook capability token.
///
/// Injected by the daemon into every agent it spawns, alongside
/// [`crate::agent_pty::DOT_AGENT_DECK_PANE_ID`] and
/// [`crate::agent_pty::DOT_AGENT_DECK_AGENT_ID`], and **scrubbed from the
/// inherited environment first** for the same reason those two are: a daemon
/// launched from inside another deck's pane must not hand that pane's token to
/// every agent it spawns. Defined once here so the injector, the scrub site and
/// the CLI readers cannot drift apart.
pub const DOT_AGENT_DECK_AGENT_TOKEN: &str = "DOT_AGENT_DECK_AGENT_TOKEN";

/// How many CSPRNG bytes back one token. 32 bytes = 256 bits, rendered as 64
/// lowercase hex characters. Far past any brute-force reach for a value that
/// lives only as long as one agent process, and small enough that the token
/// adds ~70 bytes to a hook line that [`MAX_HOOK_LINE_BYTES`] budgets 64 KiB
/// for.
const TOKEN_BYTES: usize = 32;

/// A per-agent hook capability token.
///
/// Opaque by construction: only equality and hashing are meaningful, and its
/// [`fmt::Debug`] is **redacted**. That is not decoration — `AgentEvent` derives
/// `Debug`, the daemon logs event fields, and a token that renders itself into
/// a log line is a token sitting in `~/.local/state/dot-agent-deck/deck.log`
/// for the life of the file. The redaction makes the safe thing the default at
/// every `{:?}` site, present and future.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentToken(String);

impl AgentToken {
    /// Mint a fresh token from the OS CSPRNG.
    ///
    /// Returns an error rather than falling back to a weaker source: an agent
    /// whose token is predictable is strictly worse than an agent with no token
    /// at all, because the first silently looks authorized while the second is
    /// visibly refused. The caller ([`crate::agent_pty::AgentPtyRegistry`])
    /// fails the spawn.
    pub fn mint() -> std::io::Result<Self> {
        let mut bytes = [0u8; TOKEN_BYTES];
        crate::platform::csprng::fill_random(&mut bytes)?;
        let mut s = String::with_capacity(TOKEN_BYTES * 2);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        Ok(Self(s))
    }

    /// Rebuild a token from a value that arrived over the wire or out of the
    /// environment. No validation: an unrecognized value simply resolves to
    /// nothing in [`PaneAuthority::resolve_agent_token`], which is the same
    /// outcome as a malformed one.
    pub fn from_wire(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value, for injecting into a child's environment and for the
    /// attach-socket reply. Deliberately the only way out of the newtype, so
    /// `grep as_str` over this type finds every place a token is exposed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AgentToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AgentToken(<redacted>)")
    }
}

/// What a token resolves to *in the daemon's own registry*.
///
/// This is the authority the ingest path substitutes for whatever the payload
/// claimed. `pane_id` is `Option` because an agent may legitimately have been
/// spawned with no `DOT_AGENT_DECK_PANE_ID` — such an agent's events name no
/// pane, which is the honest answer rather than letting it keep a claimed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBinding {
    pub pane_id: Option<String>,
    pub agent_id: String,
}

/// The daemon-side registry the ingest path asks about provenance.
///
/// A trait rather than a direct [`crate::agent_pty::AgentPtyRegistry`] call so
/// [`classify`] and [`admit`] are testable as pure data — the decision table
/// below is the security property, and it should not need a PTY to exercise.
pub trait PaneAuthority {
    /// Resolve a token to the `(pane, agent)` the daemon minted it for, or
    /// `None` if it is unknown or has been revoked.
    fn resolve_agent_token(&self, token: &AgentToken) -> Option<TokenBinding>;

    /// Is `pane_id` **protected** by this daemon — could an event naming it
    /// still legitimately be one of ours?
    ///
    /// # Why this is not "is there a live agent on it" (re-audit finding 1)
    ///
    /// It used to be `manages_pane`, the daemon's *routing* question: a live
    /// agent, a spawn reservation, an in-flight close or respawn. That is the
    /// wrong question for provenance, and the gap it left was the whole of
    /// re-audit finding 1. On a natural exit the registry keeps the record —
    /// nothing reaps it, and for a pane nobody closes that is forever — so
    /// routing said "not mine", [`classify`] returned [`Provenance::Foreign`],
    /// [`admit`] left the payload-claimed pane in place, and the OLDER
    /// ownership layer (`AgentPtyRegistry::generation_ownership`) then
    /// positively accepted the event, because a retired generation keeps its
    /// pane until another claims it. A token-less forged event could drive that
    /// card indefinitely, and no token was needed at all.
    ///
    /// So provenance asks the admission layer's question instead: **is any
    /// generation, live or retired-but-not-yet-succeeded, still holding this
    /// pane** (plus the in-flight reservation / close / respawn states). The
    /// answer is a superset of the routing one, and it is true in exactly the
    /// cases where the ownership layer would otherwise say `Owned` — so the two
    /// layers can no longer disagree in the direction that admits.
    fn pane_is_protected(&self, pane_id: &str) -> bool;
}

/// Why an event was refused, for the daemon's warning line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// No `agent_token` at all on an event naming a protected pane.
    MissingToken,
    /// A token that resolves to nothing — never minted, or revoked when its
    /// generation was succeeded — on an event naming a protected pane.
    UnknownToken,
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingToken => f.write_str("no capability token"),
            Self::UnknownToken => f.write_str("unrecognized or revoked capability token"),
        }
    }
}

/// The verdict for one inbound event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The token resolved. Use `binding` — **not** what the event claimed.
    Bound(TokenBinding),
    /// No usable token, but the event names no pane this daemon protects, so it
    /// is the pre-existing foreign-agent path and is left exactly as it is.
    Foreign,
    /// No usable token on an event naming a protected pane. Dropped.
    Refused(RefusalReason),
}

/// The whole decision, as pure data.
///
/// `claimed_pane_is_protected` is the caller's answer to
/// [`PaneAuthority::pane_is_protected`] for the pane the *payload* named — the
/// only thing the payload's claim is ever used for, and only ever to make the
/// daemon *stricter*.
pub fn classify(
    token: Option<&AgentToken>,
    resolved: Option<TokenBinding>,
    claimed_pane_is_protected: bool,
) -> Provenance {
    match (token, resolved) {
        // Derive, do not compare-and-trust: a valid token for pane A on an
        // event naming pane B yields A, so B is never driven.
        (Some(_), Some(binding)) => Provenance::Bound(binding),
        (Some(_), None) if claimed_pane_is_protected => {
            Provenance::Refused(RefusalReason::UnknownToken)
        }
        (None, _) if claimed_pane_is_protected => Provenance::Refused(RefusalReason::MissingToken),
        // Unknown or absent token naming a pane we do not protect: the
        // foreign-agent compatibility path (#601's named remainder).
        _ => Provenance::Foreign,
    }
}

/// Apply [`classify`] to a decoded event, in place.
///
/// Always **strips** `agent_token` from the event before returning, whatever the
/// verdict: past this point the event is broadcast to every subscribed attach
/// client and applied into `AppState`, and the token has no business travelling
/// any further than the socket it arrived on.
///
/// On [`Provenance::Bound`] the event's `pane_id` and `agent_id` are replaced by
/// the daemon's own binding.
pub fn admit<A: PaneAuthority + ?Sized>(
    authority: &A,
    event: &mut crate::event::AgentEvent,
) -> Provenance {
    let token = event.agent_token.take();
    let claimed_pane_is_protected = event
        .pane_id
        .as_deref()
        .is_some_and(|p| authority.pane_is_protected(p));
    let resolved = token
        .as_ref()
        .and_then(|t| authority.resolve_agent_token(t));
    let verdict = classify(token.as_ref(), resolved, claimed_pane_is_protected);
    if let Provenance::Bound(ref binding) = verdict {
        event.pane_id = binding.pane_id.clone();
        event.agent_id = Some(binding.agent_id.clone());
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // -----------------------------------------------------------------
    // Bounds (#319)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn a_line_under_the_limit_is_returned() {
        let mut lines = BoundedLines::with_limit(&b"hello\nworld\n"[..], 16);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("hello"));
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("world"));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    /// A line of exactly the limit still fits — the cap is "more than this is
    /// refused", so an off-by-one here would reject a legitimate maximal event.
    #[tokio::test]
    async fn a_line_of_exactly_the_limit_is_returned() {
        let payload = "x".repeat(8);
        let input = format!("{payload}\n");
        let mut lines = BoundedLines::with_limit(input.as_bytes(), 8);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some(&*payload));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_line_over_the_limit_is_refused() {
        let input = "x".repeat(9);
        let mut lines = BoundedLines::with_limit(input.as_bytes(), 8);
        assert!(matches!(
            lines.next_line().await,
            Err(HookLineError::TooLong { limit: 8 })
        ));
    }

    /// The refusal must not be reachable by padding: a valid short event hidden
    /// behind an over-limit newline-free prefix is refused whole, never
    /// truncated and re-parsed.
    #[tokio::test]
    async fn an_oversized_prefix_does_not_smuggle_a_valid_line_through() {
        let input = format!("{}{}", "x".repeat(9), "{\"ok\":true}\n");
        let mut lines = BoundedLines::with_limit(input.as_bytes(), 8);
        assert!(matches!(
            lines.next_line().await,
            Err(HookLineError::TooLong { .. })
        ));
    }

    #[tokio::test]
    async fn eof_without_a_trailing_newline_still_yields_the_line() {
        let mut lines = BoundedLines::with_limit(&b"tail"[..], 16);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("tail"));
        assert_eq!(lines.next_line().await.unwrap(), None);
        // Idempotent at EOF — the daemon loop calls until `None`.
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_empty_stream_is_immediately_eof() {
        let mut lines = BoundedLines::with_limit(&b""[..], 16);
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_trailing_carriage_return_is_stripped_like_tokio_lines() {
        let mut lines = BoundedLines::with_limit(&b"crlf\r\nbare\r"[..], 16);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("crlf"));
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("bare"));
    }

    /// The cap is on the accumulated buffer, not on one read, so writing a byte
    /// at a time must not get past it. `Cursor` hands out the whole slice at
    /// once, so this drives the reader through a source that yields one byte
    /// per `poll_read`.
    #[tokio::test]
    async fn a_byte_at_a_time_peer_still_hits_the_cap() {
        struct Trickle(Vec<u8>);
        impl AsyncRead for Trickle {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if self.0.is_empty() {
                    return std::task::Poll::Ready(Ok(()));
                }
                let b = self.0.remove(0);
                buf.put_slice(&[b]);
                std::task::Poll::Ready(Ok(()))
            }
        }
        let mut lines = BoundedLines::with_limit(Trickle(vec![b'x'; 40]), 8);
        assert!(matches!(
            lines.next_line().await,
            Err(HookLineError::TooLong { limit: 8 })
        ));
    }

    #[tokio::test]
    async fn invalid_utf8_closes_the_connection_rather_than_being_parsed() {
        let mut lines = BoundedLines::with_limit(&[0xff, 0xfe, b'\n'][..], 16);
        assert!(matches!(lines.next_line().await, Err(HookLineError::Io(_))));
    }

    // -----------------------------------------------------------------
    // Deadlines (#319, audit finding 2)
    // -----------------------------------------------------------------

    /// The denial the connection cap alone does NOT close: a peer that connects
    /// and simply says nothing. Its permit was held for the daemon's lifetime,
    /// so 128 of these wedged hook ingestion permanently — no memory pressure,
    /// no natural recovery.
    ///
    /// `start_paused` runs it on the virtual clock: the 30-second production
    /// deadline is asserted exactly, in microseconds, with no sleep and no
    /// flake.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_writes_is_cut_off_at_the_deadline() {
        // The peer half is held open for the whole test, so the reader never
        // sees EOF — a closed peer would end the stream cleanly and prove
        // nothing.
        let (_peer, daemon_side) = tokio::io::duplex(64);
        let mut lines = BoundedLines::new(daemon_side);
        let started = tokio::time::Instant::now();
        let outcome = lines.next_line_within(HOOK_LINE_TIMEOUT).await;
        assert!(
            matches!(outcome, Err(HookLineError::Idle { .. })),
            "an idle peer must be cut off, got {outcome:?}"
        );
        assert_eq!(
            started.elapsed(),
            HOOK_LINE_TIMEOUT,
            "the deadline must be the one the constant documents"
        );
    }

    /// The variant a per-READ idle timeout would miss, and the reason the
    /// deadline covers the whole call: a peer that keeps moving bytes but never
    /// completes a line resets a per-read timer forever, while staying under
    /// `MAX_HOOK_LINE_BYTES` so the byte cap never fires either.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_trickles_without_ever_completing_a_line_is_cut_off() {
        let (mut peer, daemon_side) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            loop {
                if peer.write_all(b"x").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        let mut lines = BoundedLines::new(daemon_side);
        assert!(
            matches!(
                lines.next_line_within(HOOK_LINE_TIMEOUT).await,
                Err(HookLineError::Idle { .. })
            ),
            "bytes without a newline must not keep the connection alive"
        );
    }

    /// The deadline must not cost an ordinary peer anything.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_peer_is_unaffected_by_the_deadline() {
        let mut lines = BoundedLines::new(&b"{\"ok\":true}\n"[..]);
        assert_eq!(
            lines.next_line_within(HOOK_LINE_TIMEOUT).await.unwrap(),
            Some("{\"ok\":true}".to_string())
        );
    }

    /// The test knob can only make the daemon STRICTER. A value above the
    /// production constant is clamped down to it, so no environment can relax a
    /// bound — see `DOT_AGENT_DECK_TEST_HOOK_TIMEOUT_MS`.
    ///
    /// Reads the process environment, so it asserts the clamp arithmetic
    /// directly rather than by setting a variable (`set_var` is unsound with
    /// concurrent tests, and nextest's per-test processes would not share it
    /// anyway).
    #[test]
    fn the_test_timeout_knob_can_only_shorten() {
        let longer = HOOK_LINE_TIMEOUT + Duration::from_secs(600);
        assert_eq!(longer.min(HOOK_LINE_TIMEOUT), HOOK_LINE_TIMEOUT);
        let shorter = Duration::from_millis(50);
        assert_eq!(shorter.min(HOOK_LINE_TIMEOUT), shorter);
        // With nothing set, the production values are what the daemon uses.
        assert_eq!(hook_line_timeout(), HOOK_LINE_TIMEOUT);
        assert_eq!(hook_reply_write_timeout(), HOOK_REPLY_WRITE_TIMEOUT);
    }

    // -----------------------------------------------------------------
    // Log hygiene (#318 audit finding 6, #319 audit finding 9)
    // -----------------------------------------------------------------

    /// The daemon branch that logs a RAW hook line runs after `admit` has
    /// stripped the typed token — so the raw line was the one path by which a
    /// live capability reached the disk, defeating `AgentToken`'s redacted
    /// `Debug` entirely. (The DECODED branch is redacted structurally instead;
    /// see `redact_decoded_for_log` and its tests below.)
    #[test]
    fn a_token_never_survives_into_a_log_line() {
        let token = AgentToken::mint().expect("mint");
        let line = format!(
            "{{\"session_id\":\"s\",\"agent_token\":\"{}\",\"event_type\":\"nonsense\"}}",
            token.as_str()
        );
        let redacted = redact_for_log(&line);
        assert!(
            !redacted.contains(token.as_str()),
            "the raw token must not reach the log: {redacted}"
        );
        assert!(
            redacted.contains("<redacted>"),
            "the redaction must be visible rather than silent: {redacted}"
        );
        assert!(
            redacted.contains("nonsense"),
            "the diagnostic the branch exists for must survive: {redacted}"
        );
    }

    /// The case a serde-shaped redaction would miss, and the reason this is
    /// textual: the malformed-event branch is reached BECAUSE the line did not
    /// parse.
    #[test]
    fn a_token_is_redacted_even_out_of_an_unparseable_line() {
        let token = AgentToken::mint().expect("mint");
        for shape in [
            // Truncated mid-object.
            format!("{{\"agent_token\": \"{}\", \"pane", token.as_str()),
            // Trailing comma, which serde refuses outright.
            format!(
                "{{\"pane_id\":\"p\",,\"agent_token\":\"{}\"}}",
                token.as_str()
            ),
            // Unterminated value: the token runs to the end of the line.
            format!("{{\"agent_token\":\"{}", token.as_str()),
        ] {
            let redacted = redact_for_log(&shape);
            assert!(
                !redacted.contains(token.as_str()),
                "a token must not survive a malformed line either: {redacted}"
            );
        }
    }

    /// A token containing an escaped quote must not end the redacted span
    /// early. The field is `Option<String>` off the wire, so its value is
    /// whatever a peer sends, not necessarily the hex the daemon mints.
    #[test]
    fn an_escaped_quote_inside_a_token_does_not_truncate_the_redaction() {
        let line = r#"{"agent_token":"aa\"bb-secret","event_type":"x"}"#;
        let redacted = redact_for_log(line);
        assert!(!redacted.contains("secret"), "{redacted}");
        assert!(redacted.contains(r#""event_type":"x""#), "{redacted}");
    }

    /// A line with no token is passed through unchanged (below the length cap),
    /// so ordinary diagnostics are not degraded by the redaction.
    #[test]
    fn a_line_without_a_token_is_untouched() {
        let line = r#"{"session_id":"s","event_type":"typo"}"#;
        assert_eq!(redact_for_log(line), line);
    }

    /// Audit finding 9: a peer looping malformed lines wrote up to
    /// `MAX_HOOK_LINE_BYTES` to the daemon log PER LINE. The cap bounds each
    /// one, and says how much it dropped so a reader is never misled into
    /// thinking they have the whole payload.
    #[test]
    fn an_oversized_line_is_bounded_and_says_so() {
        let line = "z".repeat(MAX_HOOK_LINE_BYTES);
        let redacted = redact_for_log(&line);
        assert!(
            redacted.len() < MAX_LOGGED_HOOK_LINE_BYTES + 64,
            "a log line must stay bounded, got {} bytes",
            redacted.len()
        );
        assert!(
            redacted.contains(&format!("{} bytes total", MAX_HOOK_LINE_BYTES)),
            "the reader must be told the payload was truncated: {redacted}"
        );
    }

    /// Truncation must not split a multi-byte character — a panic inside the
    /// daemon's per-connection task is a worse outcome than a long log line.
    #[test]
    fn truncation_respects_char_boundaries() {
        let line = "é".repeat(MAX_HOOK_LINE_BYTES);
        let redacted = redact_for_log(&line);
        assert!(redacted.contains("bytes total"));
    }

    // -----------------------------------------------------------------
    // Structural log redaction for the DECODED branch (re-audit finding 2)
    // -----------------------------------------------------------------

    /// The bypass the re-audit demonstrated with `jq`: JSON member names have
    /// escaped spellings, `serde_json` decodes them to the same key, and a
    /// textual scan for `"agent_token"` sees none of them.
    ///
    /// So the token IS honoured as a capability by `admit` and the raw line
    /// still contains no matching substring — and the unknown-`event_type`
    /// branch logged it verbatim. Keeping the payload short makes truncation
    /// irrelevant, which is why the length cap was never the answer here.
    #[test]
    fn an_escaped_member_name_is_still_the_token_field_and_is_still_dropped() {
        let token = AgentToken::mint().expect("mint");
        // `t` is `t`: this decodes to the member name `agent_token`.
        let line = format!(
            "{{\"session_id\":\"s\",\"agent_type\":\"claude-code\",\"event_type\":\"nonsense\",\
             \"timestamp\":\"2026-08-24T00:00:00Z\",\"agent_\\u0074oken\":\"{}\"}}",
            token.as_str()
        );

        // Precondition, and the whole finding: serde takes it as the token.
        let decoded: crate::event::AgentEvent =
            serde_json::from_str(&line).expect("the escaped spelling still decodes");
        assert_eq!(
            decoded.agent_token.as_ref().map(AgentToken::as_str),
            Some(token.as_str()),
            "precondition: this spelling really is the capability field"
        );
        assert!(
            !line.contains("\"agent_token\""),
            "precondition: the textual scanner has nothing to match on"
        );
        assert!(
            redact_for_log(&line).contains(token.as_str()),
            "precondition: this is exactly what the textual redaction misses"
        );

        let logged = redact_decoded_for_log(&line);
        assert!(
            !logged.contains(token.as_str()),
            "the decoded branch must not log a live capability: {logged}"
        );
        assert!(
            logged.contains("nonsense"),
            "the diagnostic the branch exists for must survive: {logged}"
        );
    }

    /// A token smuggled under a member the event does not have is dropped too:
    /// the projection is an ALLOWLIST, so it does not have to enumerate the
    /// spellings an attacker might choose.
    #[test]
    fn a_token_under_an_unexpected_member_is_dropped() {
        let token = AgentToken::mint().expect("mint");
        let line = format!(
            "{{\"session_id\":\"s\",\"agent_type\":\"claude-code\",\"event_type\":\"nonsense\",\
             \"timestamp\":\"2026-08-24T00:00:00Z\",\"stash\":\"{}\"}}",
            token.as_str()
        );
        let logged = redact_decoded_for_log(&line);
        assert!(!logged.contains(token.as_str()), "{logged}");
        assert!(!logged.contains("stash"), "{logged}");
        assert!(logged.contains("nonsense"), "{logged}");
    }

    /// The allowlist is pinned against the real wire shape, so a field added to
    /// `AgentEvent` cannot silently stop being logged — and a field renamed to
    /// `agent_token` cannot silently start being logged.
    #[test]
    fn the_log_allowlist_is_exactly_the_wire_shape_minus_the_token() {
        use crate::event::{AgentEvent, LiveTarget, TargetKind, Writable};
        let mut full = AgentEvent {
            session_id: "s".into(),
            agent_type: crate::event::AgentType::ClaudeCode,
            event_type: crate::event::EventType::ToolStart,
            tool_name: Some("Bash".into()),
            tool_detail: Some("ls".into()),
            cwd: Some("/tmp".into()),
            timestamp: chrono::Utc::now(),
            user_prompt: Some("hi".into()),
            metadata: HashMap::from([("bash_command".to_string(), "ls".to_string())]),
            pane_id: Some("p".into()),
            agent_id: Some("1".into()),
            // Every `skip_serializing_if` field must be `Some` or it would not
            // appear on the wire and the comparison would pass vacuously.
            agent_version: Some("1.2.3".into()),
            schema_version: Some(1),
            live_target: Some(LiveTarget {
                kind: TargetKind::Pty,
                writable: Writable::Live,
            }),
            agent_token: None,
        };

        let members = |e: &AgentEvent| -> std::collections::BTreeSet<String> {
            let serde_json::Value::Object(map) =
                serde_json::to_value(e).expect("an event serializes")
            else {
                panic!("an event serializes as a JSON object");
            };
            map.keys().cloned().collect()
        };

        let expected: std::collections::BTreeSet<String> = LOGGABLE_EVENT_MEMBERS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            members(&full),
            expected,
            "the log allowlist has drifted from AgentEvent's wire shape"
        );

        full.agent_token = Some(AgentToken::from_wire("t"));
        let with_token = members(&full);
        assert!(
            with_token.contains("agent_token"),
            "precondition: the token is a wire member when set"
        );
        assert_eq!(
            with_token
                .difference(&expected)
                .cloned()
                .collect::<Vec<_>>(),
            vec!["agent_token".to_string()],
            "the token must be the ONLY wire member the allowlist withholds"
        );
    }

    /// The projection keeps the payload readable: every member the event
    /// actually has survives, so the branch stays as diagnosable as it was.
    #[test]
    fn the_decoded_projection_keeps_the_events_own_members() {
        let line = r#"{"session_id":"s","agent_type":"claude-code","event_type":"typo","timestamp":"2026-08-24T00:00:00Z","pane_id":"p","tool_detail":"ls -la"}"#;
        let logged = redact_decoded_for_log(line);
        for kept in [
            "\"session_id\":\"s\"",
            "\"pane_id\":\"p\"",
            "ls -la",
            "typo",
        ] {
            assert!(logged.contains(kept), "{kept} must survive: {logged}");
        }
    }

    /// The projection is bounded by the same cap as the textual path — a huge
    /// `metadata["bash_command"]` cannot turn one log record into 64 KiB.
    #[test]
    fn the_decoded_projection_is_bounded_too() {
        let line = format!(
            "{{\"session_id\":\"s\",\"agent_type\":\"claude-code\",\"event_type\":\"typo\",\
             \"timestamp\":\"2026-08-24T00:00:00Z\",\"metadata\":{{\"bash_command\":\"{}\"}}}}",
            "z".repeat(MAX_HOOK_LINE_BYTES / 2)
        );
        let logged = redact_decoded_for_log(&line);
        assert!(
            logged.len() < MAX_LOGGED_HOOK_LINE_BYTES + 64,
            "got {} bytes",
            logged.len()
        );
        assert!(logged.contains("bytes total"), "{logged}");
    }

    /// A payload that is not a JSON object never reaches this from the daemon
    /// (the branch is only entered for a line that decoded into an `AgentEvent`),
    /// but a logging path must degrade rather than lose the diagnostic — so it
    /// falls back to the textual redaction instead of returning nothing.
    #[test]
    fn a_non_object_payload_falls_back_to_the_textual_redaction() {
        assert_eq!(redact_decoded_for_log("[1,2,3]"), "[1,2,3]");
        let token = AgentToken::mint().expect("mint");
        let broken = format!("not json at all \"agent_token\":\"{}\"", token.as_str());
        assert!(!redact_decoded_for_log(&broken).contains(token.as_str()));
    }

    // -----------------------------------------------------------------
    // Provenance (#318)
    // -----------------------------------------------------------------

    #[test]
    fn a_minted_token_is_64_hex_chars_and_never_repeats() {
        let a = AgentToken::mint().expect("mint");
        let b = AgentToken::mint().expect("mint");
        assert_eq!(a.as_str().len(), TOKEN_BYTES * 2);
        assert!(a.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// The token must not render itself into a log line. `AgentEvent` derives
    /// `Debug` and the daemon logs event fields, so this is the guard that keeps
    /// a future `{:?}` from leaking the capability.
    #[test]
    fn debug_is_redacted() {
        let t = AgentToken::from_wire("supersecretvalue");
        assert_eq!(format!("{t:?}"), "AgentToken(<redacted>)");
        assert!(!format!("{t:?}").contains("supersecret"));
    }

    #[test]
    fn a_token_serializes_transparently_as_a_bare_string() {
        let t = AgentToken::from_wire("abc");
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"abc\"");
        let back: AgentToken = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(back, t);
    }

    #[derive(Default)]
    struct FakeAuthority {
        tokens: HashMap<String, TokenBinding>,
        protected: Vec<String>,
    }

    impl PaneAuthority for FakeAuthority {
        fn resolve_agent_token(&self, token: &AgentToken) -> Option<TokenBinding> {
            self.tokens.get(token.as_str()).cloned()
        }
        fn pane_is_protected(&self, pane_id: &str) -> bool {
            self.protected.iter().any(|p| p == pane_id)
        }
    }

    fn authority() -> FakeAuthority {
        let mut a = FakeAuthority::default();
        a.tokens.insert(
            "tok-a".into(),
            TokenBinding {
                pane_id: Some("pane-a".into()),
                agent_id: "1".into(),
            },
        );
        a.protected = vec!["pane-a".into(), "pane-b".into()];
        a
    }

    fn event_for(pane: Option<&str>, token: Option<&str>) -> crate::event::AgentEvent {
        crate::event::AgentEvent {
            session_id: "s".into(),
            agent_type: crate::event::AgentType::ClaudeCode,
            event_type: crate::event::EventType::ToolStart,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: pane.map(str::to_string),
            agent_id: Some("claimed".into()),
            agent_version: None,
            schema_version: None,
            live_target: None,
            agent_token: token.map(AgentToken::from_wire),
        }
    }

    #[test]
    fn a_tokenless_event_naming_a_protected_pane_is_refused() {
        let mut e = event_for(Some("pane-a"), None);
        assert_eq!(
            admit(&authority(), &mut e),
            Provenance::Refused(RefusalReason::MissingToken)
        );
    }

    #[test]
    fn an_unknown_token_naming_a_protected_pane_is_refused() {
        let mut e = event_for(Some("pane-a"), Some("tok-nope"));
        assert_eq!(
            admit(&authority(), &mut e),
            Provenance::Refused(RefusalReason::UnknownToken)
        );
    }

    #[test]
    fn a_tokenless_event_naming_an_unprotected_pane_is_the_foreign_path() {
        let mut e = event_for(Some("someone-elses-pane"), None);
        assert_eq!(admit(&authority(), &mut e), Provenance::Foreign);
        // Left exactly as it arrived, minus the (absent) token.
        assert_eq!(e.pane_id.as_deref(), Some("someone-elses-pane"));
    }

    /// The core of #318: a *valid* token for pane A on an event naming pane B
    /// must resolve to A. Comparing the claim against the token and accepting on
    /// a match would still drive B here.
    #[test]
    fn a_valid_token_for_one_pane_cannot_drive_another() {
        let mut e = event_for(Some("pane-b"), Some("tok-a"));
        let verdict = admit(&authority(), &mut e);
        assert_eq!(
            verdict,
            Provenance::Bound(TokenBinding {
                pane_id: Some("pane-a".into()),
                agent_id: "1".into(),
            })
        );
        assert_eq!(e.pane_id.as_deref(), Some("pane-a"));
        assert_eq!(e.agent_id.as_deref(), Some("1"));
    }

    /// The same substitution applies to the claimed `agent_id`, which the
    /// post-respawn `SessionStart` filter keys on — a forged one must not be
    /// able to pass itself off as the new generation.
    #[test]
    fn the_claimed_agent_id_is_replaced_by_the_tokens_own() {
        let mut e = event_for(Some("pane-a"), Some("tok-a"));
        admit(&authority(), &mut e);
        assert_eq!(e.agent_id.as_deref(), Some("1"));
    }

    /// Whatever the verdict, the capability never travels past ingest — it is
    /// not broadcast to attach clients and not applied into `AppState`.
    #[test]
    fn the_token_is_stripped_from_the_event_on_every_path() {
        for (pane, token) in [
            (Some("pane-a"), Some("tok-a")),
            (Some("pane-a"), Some("tok-nope")),
            (Some("free-pane"), Some("tok-a")),
            (Some("free-pane"), None),
        ] {
            let mut e = event_for(pane, token);
            admit(&authority(), &mut e);
            assert!(e.agent_token.is_none(), "token survived ingest");
        }
    }

    /// An event naming no pane at all carries no claim to check, so it takes the
    /// foreign path when it has no token — and is still bound by its token when
    /// it has one.
    #[test]
    fn an_event_naming_no_pane_is_foreign_without_a_token_and_bound_with_one() {
        let mut e = event_for(None, None);
        assert_eq!(admit(&authority(), &mut e), Provenance::Foreign);

        let mut e = event_for(None, Some("tok-a"));
        assert!(matches!(admit(&authority(), &mut e), Provenance::Bound(_)));
        assert_eq!(e.pane_id.as_deref(), Some("pane-a"));
    }

    /// A token minted for an agent with no `DOT_AGENT_DECK_PANE_ID` resolves to
    /// no pane, and the event loses the pane it claimed rather than keeping it.
    #[test]
    fn a_paneless_binding_clears_a_claimed_pane() {
        let mut a = authority();
        a.tokens.insert(
            "tok-paneless".into(),
            TokenBinding {
                pane_id: None,
                agent_id: "7".into(),
            },
        );
        let mut e = event_for(Some("pane-a"), Some("tok-paneless"));
        admit(&a, &mut e);
        assert_eq!(e.pane_id, None);
        assert_eq!(e.agent_id.as_deref(), Some("7"));
    }
}
