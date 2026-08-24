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
//! ## Fail closed for managed panes, open for foreign ones
//!
//! An event with no token, or with an unrecognized one, may not name a pane
//! this daemon manages — it is refused ([`Provenance::Refused`]). The same
//! event naming a pane the daemon does *not* manage still registers a foreign
//! card exactly as before ([`Provenance::Foreign`]). That asymmetry is
//! deliberate and is issue #601's named remainder: external agents posting into
//! a deck they were not spawned by keep working, and `managed_pane_ids` is
//! still not an ownership proof — which is precisely why nothing here is built
//! on it.
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
}

impl fmt::Display for HookLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { limit } => {
                write!(f, "hook line exceeded {limit} bytes with no newline")
            }
            Self::Io(e) => write!(f, "hook read failed: {e}"),
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

    /// Does this daemon manage `pane_id` — i.e. is there a live agent, or a
    /// spawn reservation, or an in-flight close, holding it?
    fn manages_pane(&self, pane_id: &str) -> bool;
}

/// Why an event was refused, for the daemon's warning line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// No `agent_token` at all on an event naming a managed pane.
    MissingToken,
    /// A token that resolves to nothing — never minted, or revoked when its
    /// agent stopped — on an event naming a managed pane.
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
    /// No usable token, but the event names no pane this daemon manages, so it
    /// is the pre-existing foreign-agent path and is left exactly as it is.
    Foreign,
    /// No usable token on an event naming a managed pane. Dropped.
    Refused(RefusalReason),
}

/// The whole decision, as pure data.
///
/// `claimed_pane_is_managed` is the caller's answer to
/// [`PaneAuthority::manages_pane`] for the pane the *payload* named — the only
/// thing the payload's claim is ever used for, and only ever to make the
/// daemon *stricter*.
pub fn classify(
    token: Option<&AgentToken>,
    resolved: Option<TokenBinding>,
    claimed_pane_is_managed: bool,
) -> Provenance {
    match (token, resolved) {
        // Derive, do not compare-and-trust: a valid token for pane A on an
        // event naming pane B yields A, so B is never driven.
        (Some(_), Some(binding)) => Provenance::Bound(binding),
        (Some(_), None) if claimed_pane_is_managed => {
            Provenance::Refused(RefusalReason::UnknownToken)
        }
        (None, _) if claimed_pane_is_managed => Provenance::Refused(RefusalReason::MissingToken),
        // Unknown or absent token naming a pane we do not manage: the
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
    let claimed_pane_is_managed = event
        .pane_id
        .as_deref()
        .is_some_and(|p| authority.manages_pane(p));
    let resolved = token
        .as_ref()
        .and_then(|t| authority.resolve_agent_token(t));
    let verdict = classify(token.as_ref(), resolved, claimed_pane_is_managed);
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
        managed: Vec<String>,
    }

    impl PaneAuthority for FakeAuthority {
        fn resolve_agent_token(&self, token: &AgentToken) -> Option<TokenBinding> {
            self.tokens.get(token.as_str()).cloned()
        }
        fn manages_pane(&self, pane_id: &str) -> bool {
            self.managed.iter().any(|p| p == pane_id)
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
        a.managed = vec!["pane-a".into(), "pane-b".into()];
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
    fn a_tokenless_event_naming_a_managed_pane_is_refused() {
        let mut e = event_for(Some("pane-a"), None);
        assert_eq!(
            admit(&authority(), &mut e),
            Provenance::Refused(RefusalReason::MissingToken)
        );
    }

    #[test]
    fn an_unknown_token_naming_a_managed_pane_is_refused() {
        let mut e = event_for(Some("pane-a"), Some("tok-nope"));
        assert_eq!(
            admit(&authority(), &mut e),
            Provenance::Refused(RefusalReason::UnknownToken)
        );
    }

    #[test]
    fn a_tokenless_event_naming_an_unmanaged_pane_is_the_foreign_path() {
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
