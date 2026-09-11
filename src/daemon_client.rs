//! Client side of the M1.2 streaming attach protocol (PRD #76, M1.3).
//!
//! The TUI's stream-backed pane drives the daemon through this module — never
//! by reaching into [`crate::daemon_protocol`]'s frame helpers directly. The
//! protocol layer takes generic [`AsyncRead`]/[`AsyncWrite`] so the same code
//! paths run over a Unix socket today and will run over piped stdio in M2.1
//! (`daemon attach`). Only [`DaemonClient::connect`] and the resulting
//! [`AttachConnection`] are Unix-socket specific.
//!
//! Wire types ([`AttachRequest`], [`AttachResponse`], frame kinds) are
//! re-exported from [`crate::daemon_protocol`] — there is exactly one
//! definition of the wire format in the crate.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::platform::ipc::{
    EndpointAvailability, EndpointPresence, IpcStream, LOCAL_ENDPOINT_PRESENCE,
};
use crate::platform::transport::{AttachTransport, TransportReadHalf, TransportWriteHalf};

pub use crate::agent_pty::{
    AgentRecord, TabMembership, validate_orchestration_surface, validate_tab_membership,
};
use crate::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_DETACH, KIND_EVENT, KIND_REQ, KIND_RESP, KIND_SHUTDOWN,
    KIND_SHUTDOWN_ACK, KIND_STREAM_END, KIND_STREAM_OUT, read_frame, write_frame,
};
use crate::event::{AgentType, BroadcastMsg, SendResult};

/// Errors returned by the client. Server-side error responses are surfaced
/// as [`ClientError::Server`] with the daemon's message; transport problems
/// surface as [`ClientError::Io`].
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("I/O error talking to daemon: {0}")]
    Io(#[from] io::Error),
    #[error("daemon returned error: {0}")]
    Server(String),
    #[error("daemon attach socket {0} does not exist (is the daemon running?)")]
    SocketMissing(PathBuf),
    #[error("malformed daemon response: {0}")]
    Malformed(String),
}

/// Where a daemon lives, from a client's point of view (PRD #741 M2).
///
/// Until this type existed a daemon *was* a `PathBuf`, and every operation the
/// deck performs against one took that path. That is exactly right while the
/// only daemon reachable is the one on this machine, and exactly wrong the
/// moment a second kind exists: three of those operations act on a **local
/// process or a local inode**, and handed a remote deck's address they would
/// act on the wrong thing while reporting success.
///
/// So the kind is in the type, and the local-only operations take
/// [`LocalEndpoint`] rather than `&Path`. [`Self::as_local`] is the only way to
/// obtain one from an `Endpoint`, and it returns `None` for [`Self::Remote`] —
/// so those operations cannot be *reached* from a remote deck and the caller is
/// made to decide what to do instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A daemon process on this machine. Byte-identical in behaviour to
    /// everything that came before this type: same path, same trust check, same
    /// lazy-spawn, same stop.
    Local(LocalEndpoint),
    /// A daemon on another machine. PRD #741 M5 gives it a transport and M6 a
    /// stored shape; today it exists so the *type* distinction is real and the
    /// local-only operations have something to refuse.
    Remote(RemoteEndpoint),
}

impl Endpoint {
    /// This host's configured attach endpoint — what every caller used before
    /// this type existed, and still the default selection until M6 stores one.
    pub fn local() -> Self {
        Self::Local(LocalEndpoint::from_config())
    }

    /// The local daemon behind this endpoint, or `None` if there is not one.
    ///
    /// This is the whole mechanism. `None` for [`Self::Remote`] is what makes
    /// `peer_pid` termination, the stale-inode unlink and lazy-spawn
    /// unreachable for a remote deck.
    pub fn as_local(&self) -> Option<&LocalEndpoint> {
        match self {
            Self::Local(local) => Some(local),
            Self::Remote(_) => None,
        }
    }

    /// [`Self::as_local`] with a refusal that names the operation and the deck,
    /// for the call sites that must report *why* rather than silently skip —
    /// the desktop's Stop and Replace buttons (PRD #741 M7 turns this message
    /// into a disabled control with the same explanation).
    pub fn require_local(&self, operation: &'static str) -> Result<&LocalEndpoint, EndpointError> {
        self.as_local().ok_or_else(|| EndpointError::LocalOnly {
            operation,
            deck: self.describe(),
        })
    }

    /// The address a client connects to.
    ///
    /// **Deliberately a bare `&Path` and never a [`LocalEndpoint`].** When M5
    /// adds the `ssh -N -L` transport, the address a *remote* deck is reached at
    /// is a forwarded socket on this filesystem — and spelling that
    /// `LocalEndpoint` would hand it straight back to `run_daemon_stop`, whose
    /// `SO_PEERCRED` lookup would then name the local `ssh` client. The two
    /// values are different things and this type keeps them different.
    pub fn connect_address(&self) -> Result<&Path, EndpointError> {
        match self {
            Self::Local(local) => Ok(local.path()),
            Self::Remote(remote) => Err(EndpointError::RemoteTransportUnavailable {
                deck: remote.label().to_string(),
            }),
        }
    }

    /// What this endpoint's connect address can be asked about on **this**
    /// filesystem (PRD #741 M3).
    ///
    /// The third answer the four filesystem-presence predicates needed. It is a
    /// property of the *endpoint*, not of the platform and not of the path —
    /// under DECISION 1A a remote deck's connect address is a forwarded socket
    /// that exists right here, so nothing about the path itself distinguishes
    /// the cases and only the endpoint knows.
    pub fn presence(&self) -> EndpointPresence {
        match self {
            Self::Local(_) => LOCAL_ENDPOINT_PRESENCE,
            Self::Remote(_) => EndpointPresence::Elsewhere,
        }
    }

    /// How this endpoint is named in a message to the user. For a local deck
    /// that is its address, which is what the desktop's connection banner has
    /// always shown.
    pub fn describe(&self) -> String {
        match self {
            Self::Local(local) => local.path().to_string_lossy().into_owned(),
            Self::Remote(remote) => remote.label().to_string(),
        }
    }
}

/// A daemon running on **this machine**, addressed by the OS name a client
/// connects to: a Unix domain socket path, or a `\\.\pipe\…` name on Windows
/// (which has no filesystem presence — see
/// [`crate::platform::ipc::LOCAL_ENDPOINT_PRESENCE`]).
///
/// A newtype over the `PathBuf` these call sites used to pass, and the wrapper
/// is the point. Three operations are only meaningful — and only safe — when
/// the daemon is a process on this host:
///
/// 1. **Termination by peer credential.** [`crate::daemon_stop::run_daemon_stop`]
///    reads the daemon's pid off the connected socket
///    ([`crate::platform::peercred::peer_pid`]) and terminates it;
///    [`crate::build_version_handshake::ensure_compatible_daemon_or_die`] does
///    the same on a build-stamp mismatch. Over an `ssh -L` forwarded socket
///    that pid is the **local `ssh` client's**, so the call would tear the
///    tunnel down and report that it had stopped the daemon — and Replace would
///    then lazy-spawn a *local* daemon and report success.
/// 2. **The stale-inode unlink** in
///    [`crate::daemon_attach::ensure_daemon_running`], which is safe only
///    because the trust check immediately above it just proved the inode is
///    ours.
/// 3. **Lazy-spawn**, which starts a daemon process *here* — never the answer
///    when the daemon you asked for is somewhere else.
///
/// Each of those takes `&LocalEndpoint`, and [`Endpoint::as_local`] is the only
/// route from an [`Endpoint`] to one.
///
/// **What this does not claim.** [`Self::at`] accepts any path, so the type
/// guards against *reaching for the wrong value*, not against a caller that
/// deliberately asserts a wrong one. That is why [`Endpoint::connect_address`]
/// returns a bare `&Path`: the one value most likely to be wrapped by mistake —
/// M5's forwarded socket — never arrives here already wearing this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpoint {
    path: PathBuf,
}

impl LocalEndpoint {
    /// This host's configured attach endpoint,
    /// [`crate::config::attach_socket_path`] — exactly the value every caller
    /// passed before this type existed, `DOT_AGENT_DECK_ATTACH_SOCKET` override
    /// included.
    pub fn from_config() -> Self {
        Self::at(crate::config::attach_socket_path())
    }

    /// A local daemon at an explicitly chosen address.
    ///
    /// The escape hatch, and it is named as one: nothing here verifies that
    /// `path` names a daemon on this machine. It exists for callers that
    /// already hold the configured path and for tests that bind a scripted
    /// listener in a `tempfile::tempdir()`. **Never** hand it an address a
    /// remote transport produced — see the type's docs for what that would cost.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The address to connect to, bind against, or poll for.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for LocalEndpoint {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// A daemon on another machine (PRD #741 M5/M6).
///
/// **A placeholder, deliberately.** M2 adds no transport and no settings, so
/// this carries only enough to *name* a deck in a refusal message. M6 designs
/// what is actually stored — host, optional user, port, optional key *path*,
/// optional jump-host name, each a validating newtype, and never a secret — and
/// M5 gives it a transport. Do not grow it here: a schema invented now would be
/// the one M6 has to migrate away from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    label: String,
}

impl RemoteEndpoint {
    /// Name a remote deck. The label is display text only — it addresses
    /// nothing and reaches nothing until M5.
    pub fn named(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }

    /// The deck's display name.
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// Why an operation could not be performed against an [`Endpoint`].
///
/// Both variants are *errors returned*, not panics: a caller that reaches a
/// remote deck before M5 lands gets a message it can render, which is the
/// difference between an unfinished milestone and a crash.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EndpointError {
    /// The endpoint is remote and the remote transport has not landed yet
    /// (PRD #741 M5).
    #[error(
        "cannot connect to the remote deck {deck}: connecting to a daemon on another machine is \
         not supported yet"
    )]
    RemoteTransportUnavailable { deck: String },
    /// The operation acts on a local process or a local inode, so it has no
    /// meaning against a daemon on another machine. PRD #741 M7 renders this as
    /// a disabled control rather than a failed action.
    #[error(
        "{operation} is not available for the remote deck {deck}: it acts on a process on this \
         machine, which is not the machine that deck runs on"
    )]
    LocalOnly {
        operation: &'static str,
        deck: String,
    },
}

/// PRD #127 C5: the distinct outcomes of a `run-now`. Both mean the task is
/// registered (the request succeeded); they differ only in whether a fire
/// actually started or was skipped because a prior run is still active, so the
/// caller can report a non-confusing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunNowOutcome {
    Started,
    SkippedStillRunning,
}

/// Map the `agents` token the daemon's `RunNow` handler returns to a
/// [`RunNowOutcome`]. `"skipped"` → skipped; anything else (incl. a stale
/// daemon that omits the token) → started. Pure so it is unit-testable.
pub fn run_now_outcome_from_agents(agents: &Option<Vec<String>>) -> RunNowOutcome {
    match agents {
        Some(tokens) if tokens.iter().any(|t| t == "skipped") => RunNowOutcome::SkippedStillRunning,
        _ => RunNowOutcome::Started,
    }
}

/// Owned counterpart of [`AttachRequest::StartAgent`]. Owned (vs. borrowed)
/// because callers are typically blocking threads that need to hand the
/// options off to an async task running on the tokio runtime.
#[derive(Debug, Clone)]
pub struct StartAgentOptions {
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// Human-readable label captured into the daemon's per-agent registry
    /// (M2.11). Forwarded as `AttachRequest::StartAgent.display_name`; the
    /// daemon validates it via `is_valid_display_name` and stores `None`
    /// on failure. `None` here omits the field from the wire payload so
    /// older daemons keep accepting the request.
    pub display_name: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub env: Vec<(String, String)>,
    /// PRD #76 M2.12: which tab the TUI placed this agent pane in
    /// (mode / orchestration). Forwarded as
    /// `AttachRequest::StartAgent.tab_membership` so the daemon can
    /// echo it back via `list_agents` and the TUI can rebuild tab
    /// structure on reconnect. `None` here means "dashboard pane" and
    /// omits the field from the wire payload so older daemons keep
    /// accepting the request.
    pub tab_membership: Option<TabMembership>,
    /// PRD #76 M2.13: which AI agent this spawn command runs (inferred
    /// from the command via [`AgentType::from_command`] at the TUI spawn
    /// site). Forwarded as `AttachRequest::StartAgent.agent_type` so the
    /// daemon captures it from the outset and `list_agents` can echo it
    /// back on reconnect — the hydration path uses the value to seed
    /// placeholder sessions with the correct `agent_type` instead of
    /// `AgentType::None` (which the dashboard renders as "No agent").
    /// `None` here omits the field from the wire payload so older
    /// daemons keep accepting the request.
    pub agent_type: Option<AgentType>,
    /// PRD #201 native prompt delivery: a seed/prompt to stash daemon-side for
    /// this pane at spawn time (via `AgentPtyRegistry::set_pending_seed`), to be
    /// pulled NATIVELY by the pane's extension via `dot-agent-deck get-seed`
    /// (→ `pi.sendUserMessage`) instead of typed into the PTY. Set only for a
    /// Pi start-role (orchestrator) pane. `None` omits the field from the wire
    /// payload so older daemons keep accepting the request (and, receiving no
    /// seed, drive the unchanged PTY-injection path).
    pub seed: Option<String>,
}

impl Default for StartAgentOptions {
    fn default() -> Self {
        Self {
            command: None,
            cwd: None,
            display_name: None,
            rows: 24,
            cols: 80,
            env: Vec::new(),
            tab_membership: None,
            agent_type: None,
            seed: None,
        }
    }
}

// ---------------------------------------------------------------------------
// I/O-generic protocol helpers (transport-independent — work over UnixStream
// today and over piped stdio in M2.1).
// ---------------------------------------------------------------------------

/// Send a single REQ frame carrying a JSON-encoded [`AttachRequest`].
pub async fn send_request<W: AsyncWrite + Unpin>(
    wr: &mut W,
    req: &AttachRequest,
) -> io::Result<()> {
    let payload = serde_json::to_vec(req)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    write_frame(wr, KIND_REQ, &payload).await
}

/// Read a single RESP frame and decode it. Errors out on EOF, wrong frame
/// kind, or malformed JSON.
pub async fn read_response<R: AsyncRead + Unpin>(
    rd: &mut R,
) -> Result<AttachResponse, ClientError> {
    match read_frame(rd).await? {
        None => Err(ClientError::Malformed(
            "daemon closed connection before sending RESP".into(),
        )),
        Some((KIND_RESP, payload)) => serde_json::from_slice(&payload)
            .map_err(|e| ClientError::Malformed(format!("RESP JSON: {e}"))),
        Some((kind, _)) => Err(ClientError::Malformed(format!(
            "expected RESP, got frame kind 0x{kind:02x}"
        ))),
    }
}

/// PRD #20 R20-011: translate a `WriteAndSubmit` [`AttachResponse`] into the
/// honest [`SendResult`] a caller acts on, enforcing that `ok` AGREES with the
/// delivered-vs-non-delivered outcome. Three cases:
///
/// * A typed `send_result` present → return it, EXCEPT when it claims delivery
///   (`applied`/`queued`) while `ok = false`. That contradiction (a
///   forward-compat/hostile daemon, or a bug) must NEVER be reported as success:
///   `ok = false` wins and we surface a server error. An unknown future variant
///   decodes to [`SendResult::Unknown`] and is returned verbatim (a non-delivery
///   the UI matches handle conservatively).
/// * No `send_result` and `ok = false` → a genuine transport/server failure.
/// * No `send_result` and `ok = true` → a pre-PRD-20 daemon; legacy
///   fire-and-forget "assume applied".
fn interpret_send_response(resp: AttachResponse) -> Result<SendResult, ClientError> {
    if let Some(result) = resp.send_result {
        let claims_delivered = matches!(result, SendResult::Applied | SendResult::Queued);
        if claims_delivered && !resp.ok {
            return Err(ClientError::Server(resp.error.unwrap_or_else(|| {
                "daemon reported ok=false with a delivered send_result".into()
            })));
        }
        return Ok(result);
    }
    if !resp.ok {
        return Err(ClientError::Server(
            resp.error
                .unwrap_or_else(|| "write-and-submit failed".into()),
        ));
    }
    Ok(SendResult::Applied)
}

/// One-shot request/response: send `req`, read one RESP, return it. Used for
/// non-streaming operations (`list-agents`, `start-agent`, `stop-agent`).
pub async fn issue_command<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    rd: &mut R,
    wr: &mut W,
    req: &AttachRequest,
) -> Result<AttachResponse, ClientError> {
    send_request(wr, req).await?;
    read_response(rd).await
}

/// Per-entry byte ceiling the live-snapshot clamp enforces on each surviving
/// `first_prompts` entry (PRD #162 finding #2). A hostile/malformed daemon
/// could advertise a megabyte-long prompt that would bloat the rebuilt card;
/// 64 KiB is far above any real first prompt yet bounds the worst case.
const MAX_FIRST_PROMPT_BYTES: usize = 65536;

/// Drop ASCII/Unicode control characters from a daemon-supplied string so no
/// raw control byte (ANSI escape, NUL, DEL, C1) survives into a rendered cell.
/// Mirrors the `char::is_control` policy `login_shell` / the build-handshake
/// render seam apply elsewhere on untrusted wire input.
fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Truncate `s` to at most `max_bytes`, snapping back to the nearest char
/// boundary so a multi-byte UTF-8 sequence is never split.
fn clamp_bytes(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

/// Sanitize a single `AgentRecord` echoed by the daemon before it reaches the
/// TUI. Defense in depth at the wire boundary (M2.12 fixup auditor #1, PRD
/// #162 findings #1/#2): the daemon validates on `StartAgent`, but a malformed
/// or older daemon could still echo an untrusted record. Two scrubs:
///
/// - `tab_membership`: clamped to `None` if the embedded `name` fails
///   [`validate_tab_membership`] (logged via `tracing::warn!` — the agent is
///   real, we just don't trust the bucketing hint).
/// - `display_name` (issue #833): scrubbed of control characters AND bidi
///   overrides and clamped to [`crate::agent_pty::DISPLAY_NAME_MAX_LEN`] via
///   [`crate::untrusted_text::sanitize_display_name`]; a name with nothing
///   printable left becomes `None`, which hydration renders as the agent id.
///   This field is the card TITLE the TUI actually uses — hydration copies it
///   into `ui.pane_display_names`, the dashboard loop fills `ui.display_names`
///   from that, and `ui::render_card_grid` prefers that map over the session's
///   own `display_name` — so it is the string on this record whose scrub the
///   render most depends on. The daemon does gate it
///   (`agent_pty::is_valid_display_name`), but that gate lives at the other end
///   of the wire: a daemon too old to carry the gate's bidi half, or one not
///   running this code at all, echoes whatever it stored. Same
///   defense-in-depth argument as `tab_membership` above.
///
/// NOT scrubbed here, and this list is the whole of what is: `id` and
/// `pane_id_env` (daemon-minted — `id` is a monotonic counter stringified) and
/// **`cwd`**. That last one is a real residual rather than a safe omission: the
/// dashboard renders its basename, and the only thing standing between a
/// hostile value and a cell is the daemon-side `agent_pty::is_valid_cwd`, whose
/// byte test admits a bidi override for exactly the reason
/// `is_valid_display_name`'s did before issue #833 — `U+202E` is three bytes
/// each above `0x20`. Deliberately out of scope for #833, which names two
/// seams, and stated here rather than left for the next reader to rediscover.
/// - `live` snapshot (PRD #162): control bytes are stripped from
///   `last_user_prompt`, every `first_prompts` entry, and `active_tool.name` /
///   `.detail`, and each of those strings is length-bounded to
///   [`MAX_FIRST_PROMPT_BYTES`]; `first_prompts` is additionally clamped to at
///   most [`crate::state::MAX_FIRST_PROMPTS`] entries. The snapshot is KEPT as
///   `Some(..)` — the agent is real; only its strings are scrubbed.
fn sanitize_record_tab_membership(rec: &mut AgentRecord) {
    if let Some(raw) = rec.display_name.take() {
        rec.display_name = crate::untrusted_text::sanitize_display_name(&raw);
        if rec.display_name.is_none() {
            tracing::warn!(
                agent_id = %rec.id,
                name_len = raw.len(),
                "list_agents: dropping daemon-supplied display_name with no printable \
                 content — the card falls back to the agent id"
            );
        }
    }

    if let Some(tm) = rec.tab_membership.take() {
        let name_len = tm.name().len();
        match validate_tab_membership(tm) {
            Some(v) => rec.tab_membership = Some(v),
            None => {
                tracing::warn!(
                    agent_id = %rec.id,
                    name_len,
                    "list_agents: clamping invalid tab_membership.name from daemon record to None — pane lands on dashboard"
                );
            }
        }
    }

    if let Some(live) = rec.live.as_mut() {
        if let Some(prompt) = live.last_user_prompt.as_mut() {
            *prompt = clamp_bytes(strip_control_chars(prompt), MAX_FIRST_PROMPT_BYTES);
        }
        if let Some(tool) = live.active_tool.as_mut() {
            tool.name = clamp_bytes(strip_control_chars(&tool.name), MAX_FIRST_PROMPT_BYTES);
            if let Some(detail) = tool.detail.as_mut() {
                *detail = clamp_bytes(strip_control_chars(detail), MAX_FIRST_PROMPT_BYTES);
            }
        }
        // Clamp the count first, then scrub + length-bound each survivor so we
        // never waste work scrubbing entries we're about to drop.
        live.first_prompts.truncate(crate::state::MAX_FIRST_PROMPTS);
        for prompt in live.first_prompts.iter_mut() {
            *prompt = clamp_bytes(strip_control_chars(prompt), MAX_FIRST_PROMPT_BYTES);
        }
    }
}

// ---------------------------------------------------------------------------
// PRD #819 M5: the client's reading of the daemon's advertised capability set.
// ---------------------------------------------------------------------------

/// PRD #819 M5: the capability set observed on ONE handshake reply, plus the
/// client's fail-safe reading of it.
///
/// This is the general form of the [`AttachResponse::guarded_send`] check
/// (`daemon_advertises_guarded_send` below): it consults the daemon's
/// **explicit capability advertisement, never the protocol version number**,
/// and it withholds rather than proceeds when the advertisement is missing.
///
/// Three properties, each of which is a decision rather than an accident:
///
/// * **Absence withholds.** `capabilities: None` on the wire — an older daemon
///   that predates the field, or a newer one that chose not to answer — reads
///   as [`Self::absent`], for which [`Self::supports`] is `false` for every
///   string. Absence is never "probably fine, it is local".
/// * **Unknown strings are ignored, not rejected.** The set is just strings;
///   one this build has no constant for is carried and matched like any other,
///   so a newer daemon can grow the set without breaking an older client.
/// * **Nothing here branches on serde's `unknown variant …` text.** That text
///   is what an older daemon's clean `ok:false` refusal of an unknown verb is
///   otherwise discriminated by, and it is not a stability contract. This type
///   exists to replace that string match.
///
/// It is **compatibility metadata, not authentication** — a daemon controls
/// its own replies and can claim anything. What it buys is a stable answer to
/// "will this op parse over there".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonCapabilities {
    /// `None` is the wire's own absence, preserved rather than flattened to an
    /// empty set: [`Self::is_advertised`] can then tell "this daemon said
    /// nothing" from "this daemon said it supports nothing", which are the same
    /// for [`Self::supports`] but not for a diagnostic.
    advertised: Option<BTreeSet<String>>,
}

impl DaemonCapabilities {
    /// Capture the set from a `Hello` reply. The ONLY constructor that reads
    /// the wire, so there is one place where absence is turned into "withhold".
    pub fn from_hello(resp: &AttachResponse) -> Self {
        Self {
            advertised: resp
                .capabilities
                .as_ref()
                .map(|list| list.iter().cloned().collect()),
        }
    }

    /// The older-daemon reading: nothing was advertised, so everything is
    /// withheld. Equivalent to [`Self::from_hello`] on a reply whose
    /// `capabilities` field is absent.
    pub fn absent() -> Self {
        Self { advertised: None }
    }

    /// Whether the daemon advertised a set at all. `false` means this client
    /// knows nothing about the daemon's verbs and must withhold every one of
    /// them — it does NOT mean the daemon supports none.
    pub fn is_advertised(&self) -> bool {
        self.advertised.is_some()
    }

    /// Whether `capability` was advertised. `false` for an unadvertised daemon,
    /// for a daemon advertising a different set, and for a daemon advertising
    /// an empty set — the three cases a caller must treat identically.
    pub fn supports(&self, capability: &str) -> bool {
        self.advertised
            .as_ref()
            .is_some_and(|set| set.contains(capability))
    }

    /// The withhold decision, as an error a caller can propagate. `Ok(())` only
    /// when `capability` was explicitly advertised.
    ///
    /// This is the single spelling of the decline, so the message stays uniform
    /// across call sites when the TUI adopts the project verbs (PRD #819 M6 /
    /// the split-out `ui.rs` work). It deliberately names the capability and not
    /// the daemon's version: the version is not what was checked.
    pub fn require(&self, capability: &str) -> Result<(), ClientError> {
        if self.supports(capability) {
            return Ok(());
        }
        Err(ClientError::Server(format!(
            "daemon does not advertise the `{capability}` capability; withholding the request \
             rather than assuming support (an older daemon answers an unknown op with a bare \
             ok:false, which is not something to branch on)"
        )))
    }
}

/// PRD #819 M5: the daemon identity a captured [`DaemonCapabilities`] describes.
///
/// Both fields come off the same `Hello` reply the set did. `build_version` is
/// the finer-grained one (PRD #103's `DAD_BUILD_ID`, commit hash and dirty
/// marker included), so a rebuilt daemon at the same endpoint is a different
/// generation even at an unchanged `PROTOCOL_VERSION`.
///
/// It is recorded rather than only compared because the reply carries no
/// per-process nonce: two runs of the same binary at the same socket are
/// indistinguishable here, and that is fine — the capability set is a property
/// of the BUILD, so a same-build restart advertises the same set. What the
/// generation catches is the case that is not fine: a *different* build
/// answering at an endpoint whose set was already captured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonGeneration {
    pub server_version: Option<u32>,
    pub build_version: Option<String>,
}

impl DaemonGeneration {
    fn from_hello(resp: &AttachResponse) -> Self {
        Self {
            server_version: resp.server_version,
            build_version: resp.build_version.clone(),
        }
    }
}

/// One captured handshake: the set, and the endpoint + generation it describes.
///
/// The pair is the cache key. A set that outlives the connection it describes is
/// worse than no cache, so nothing is returned from here without the endpoint
/// matching — see [`cached_capabilities_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilitySnapshot {
    endpoint: PathBuf,
    generation: DaemonGeneration,
    capabilities: DaemonCapabilities,
}

/// The cache read, as a free function so the key rule is unit-testable without a
/// daemon, a socket, or a `DaemonClient` whose endpoint cannot be mutated.
///
/// Returns the captured set ONLY when the snapshot describes `endpoint`. A
/// snapshot taken against a different endpoint is not "close enough": the two
/// daemons are unrelated processes that may be different builds entirely.
fn cached_capabilities_for(
    snapshot: Option<&CapabilitySnapshot>,
    endpoint: &Path,
) -> Option<DaemonCapabilities> {
    snapshot
        .filter(|snap| snap.endpoint == endpoint)
        .map(|snap| snap.capabilities.clone())
}

// ---------------------------------------------------------------------------
// Unix-socket transport
// ---------------------------------------------------------------------------

/// Thin handle around the daemon's attach socket path. Cheap to clone — every
/// operation opens its own short-lived [`IpcStream`] (matching the daemon's
/// per-connection state machine in [`crate::daemon_protocol`]).
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
    /// PRD #741 M3: what a `stat` of `socket_path` is allowed to mean.
    ///
    /// Carried rather than derived, because the address alone cannot say: under
    /// DECISION 1A a remote deck's connect address is a forwarded socket on this
    /// filesystem and is indistinguishable from a local daemon's by inspection.
    /// [`Self::new`] sets the local answer — every caller that existed before
    /// this field held a local daemon's path — and
    /// [`Self::for_endpoint`] is how a caller that knows better says so.
    presence: EndpointPresence,
    /// PRD #819 M5: the capability set captured at the handshake for
    /// `socket_path`, shared across clones of this handle so the set is fetched
    /// ONCE per daemon rather than once per project-aware call.
    ///
    /// Shared deliberately: `DaemonClient` is cloned freely and each clone talks
    /// to the same daemon over the same path, so a per-clone cache would be a
    /// per-call handshake with extra steps.
    ///
    /// A `std::sync::Mutex` rather than a `tokio` one because the lock is never
    /// held across an `.await` — [`Self::capabilities`] reads it, drops it,
    /// handshakes, then re-takes it to store.
    capabilities: Arc<Mutex<Option<CapabilitySnapshot>>>,
}

impl DaemonClient {
    /// A client for a daemon on **this** machine, at `socket_path`.
    ///
    /// Deliberately unchanged in signature (PRD #741 M3): every TUI and CLI
    /// caller holds this host's attach path and nothing about them moves for
    /// this PRD. The presence it records is
    /// [`LOCAL_ENDPOINT_PRESENCE`], which is exactly what the code did before
    /// the field existed.
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            presence: LOCAL_ENDPOINT_PRESENCE,
            capabilities: Arc::new(Mutex::new(None)),
        }
    }

    /// A client for whichever deck `endpoint` names (PRD #741 M3).
    ///
    /// The difference from [`Self::new`] is the presence, not the address: a
    /// remote deck's connect address is still a path, and still one this process
    /// connects to. What changes is that nothing may read a `stat` of it as the
    /// daemon's health — see [`EndpointPresence::Elsewhere`].
    ///
    /// Errors while [`Endpoint::connect_address`] does, i.e. for a remote deck
    /// until M5 lands.
    pub fn for_endpoint(endpoint: &Endpoint) -> Result<Self, EndpointError> {
        Ok(Self {
            socket_path: endpoint.connect_address()?.to_path_buf(),
            presence: endpoint.presence(),
            capabilities: Arc::new(Mutex::new(None)),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Surface a clear "daemon not running" error before any I/O is
    /// attempted. The remote-deck-local TUI calls this at startup so the
    /// user doesn't see a generic ECONNREFUSED.
    ///
    /// PRD #741 M3: only [`EndpointAvailability::Absent`] is a refusal.
    /// `Unanswerable` is the third answer this used to spell as absence — a
    /// Windows pipe name has nothing to `stat` (so this reported *every* live
    /// Windows daemon missing), and a remote deck has something to `stat` that
    /// is not the daemon. In both cases the connect that follows is the only
    /// honest test, so this steps aside rather than inventing a verdict.
    pub fn ensure_socket_exists(&self) -> Result<(), ClientError> {
        match self.presence.availability(&self.socket_path) {
            EndpointAvailability::Absent => {
                Err(ClientError::SocketMissing(self.socket_path.clone()))
            }
            EndpointAvailability::Present | EndpointAvailability::Unanswerable => Ok(()),
        }
    }

    /// Open a connection to the daemon and split it into owned halves.
    ///
    /// **PRD #741 M3: this returns the halves, not the stream, and that is the
    /// design rather than a convenience.** Every one of the seventeen callers
    /// split immediately, so handing back an unsplit transport bought nothing —
    /// and what it cost is that a caller could split it itself with
    /// [`tokio::io::split`], which is the exact regression
    /// [`crate::platform::ipc`]'s module docs record an earlier draft shipping:
    /// a write half that does not half-close on drop, so the daemon never
    /// notices a client has gone. Splitting here, once, makes that unreachable
    /// from the client.
    ///
    /// The concrete transport is still an [`IpcStream`] — M3 changes the type
    /// that flows, not what is underneath, and not the lifetime either (holding
    /// the connection open is M4).
    async fn connect(&self) -> io::Result<(TransportReadHalf, TransportWriteHalf)> {
        Ok(IpcStream::connect(&self.socket_path)
            .await?
            .split_transport())
    }

    /// List daemon-side agents. Returns one [`AgentRecord`] per agent,
    /// preferring the daemon's new `agent_records` field (which carries
    /// each agent's spawn-time `DOT_AGENT_DECK_PANE_ID`). Falls back to
    /// the legacy `agents`-only field with `pane_id_env: None` so a
    /// newer TUI keeps working against an older daemon — at the cost of
    /// not being able to preserve pane ids on rehydration there.
    ///
    /// M2.12 fixup auditor #1: re-validates each record's
    /// `tab_membership` at this wire boundary. The daemon validates
    /// `StartAgent.tab_membership` before storing it, but a malformed
    /// or older daemon could still echo back an invalid `name` here. An
    /// invalid membership is cleared to `None` (the agent is real, it
    /// just lands on the dashboard) and a `tracing::warn!` surfaces the
    /// drift — we never propagate a control-byte name into
    /// bucketing/logging/tab lookup.
    pub async fn list_agents(&self) -> Result<Vec<AgentRecord>, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(&mut rd, &mut wr, &AttachRequest::ListAgents).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "list-agents failed".into()),
            ));
        }
        if let Some(mut records) = resp.agent_records {
            for rec in &mut records {
                sanitize_record_tab_membership(rec);
            }
            return Ok(records);
        }
        Ok(resp
            .agents
            .unwrap_or_default()
            .into_iter()
            .map(|id| AgentRecord {
                id,
                pane_id_env: None,
                display_name: None,
                cwd: None,
                tab_membership: None,
                agent_type: None,
                rows: 0,
                cols: 0,
                // Legacy `agents`-only daemon shape carries no live session
                // state; the TUI falls back to a bare placeholder.
                live: None,
                // PRD #745 M11: and no spawn instant either — this daemon
                // predates the field, so it reported no spawn time and none may
                // be invented for it. Absence renders as nothing.
                spawned_at_ms: None,
            })
            .collect())
    }

    /// PRD #127 M1.3: ask a running daemon to re-read the global
    /// `schedules.toml` and diff/replace its registered task set without a
    /// restart. Returns the now-registered ENABLED task names. The CLI's
    /// mutating subcommands call this after an atomic write so the daemon picks
    /// the change up live.
    pub async fn reload_schedules(&self) -> Result<Vec<String>, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(&mut rd, &mut wr, &AttachRequest::ReloadSchedules).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "reload-schedules failed".into()),
            ));
        }
        Ok(resp.agents.unwrap_or_default())
    }

    /// PRD #127 M1.5: fire a registered scheduled task now (the
    /// `schedule run-now` door). Errors if no such task is registered.
    pub async fn run_now(&self, name: &str) -> Result<RunNowOutcome, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::RunNow {
                name: name.to_string(),
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "run-now failed".into()),
            ));
        }
        // PRD #127 C5: surface started vs skipped-still-running to the caller.
        Ok(run_now_outcome_from_agents(&resp.agents))
    }

    pub async fn start_agent(&self, opts: StartAgentOptions) -> Result<String, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let req = AttachRequest::StartAgent {
            command: opts.command,
            cwd: opts.cwd,
            display_name: opts.display_name,
            rows: opts.rows,
            cols: opts.cols,
            env: opts.env,
            tab_membership: opts.tab_membership,
            agent_type: opts.agent_type,
            seed: opts.seed,
        };
        let resp = issue_command(&mut rd, &mut wr, &req).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "start-agent failed".into()),
            ));
        }
        resp.id
            .ok_or_else(|| ClientError::Malformed("start-agent ok but no id in response".into()))
    }

    /// PRD #100: route a pane write through the daemon's atomic
    /// `write_and_submit_guarded` primitive instead of the
    /// two-`STREAM_IN`-frames-with-gap pattern. Same one-shot connection
    /// shape as `resize_agent` / `stop_agent`. The daemon holds the
    /// per-agent writer mutex across `payload → SUBMIT_DELAY → CR`, so a
    /// concurrent daemon-initiated write (work-done feedback, respawn
    /// notice) cannot interleave between the payload and the submit CR.
    ///
    /// PRD #20 M3: returns the daemon's honest [`SendResult`] rather than a bare
    /// `()`. A newer daemon reports `applied` for a live target and
    /// `history-only` / `no-live-target` when the session can't accept live
    /// input; a pre-PRD-20 daemon omits the field, which we read as
    /// [`SendResult::Applied`] (the legacy fire-and-forget assumption). A
    /// transport/`ok=false` failure still surfaces as `Err`.
    ///
    /// Issue #608: a write through this identity-less door is refused by a
    /// current daemon (`no-live-target`, nothing written). On the PANED arm that
    /// is the change — an absent `expected_agent_id` no longer degrades to
    /// pane-only authorization. On the PANE-LESS arm nothing changed, because it
    /// never accepted this shape: `<no-pane>` has always been routed by agent
    /// identity, so a request naming no agent has always resolved to
    /// `Writable::None` there.
    ///
    /// Issue #608 audit, finding 5: this doc used to say the method was kept for
    /// PANE-LESS callers, which was never true — that arm declines this shape as
    /// firmly as the paned one now does. What it is actually kept for is a
    /// pre-PRD-20 daemon: naming no identity is what keeps
    /// [`Self::write_and_submit_with_identity`]'s guarded-send capability probe
    /// out of the way, so this remains the legacy-compatible fire-and-forget
    /// door. Against a current daemon every caller wants the identity-bearing
    /// sibling, paned or not; in-tree the only remaining caller of this one is a
    /// unit test.
    pub async fn write_and_submit(
        &self,
        pane_id: &str,
        text: &str,
    ) -> Result<SendResult, ClientError> {
        self.write_and_submit_with_identity(pane_id, text, None, None, None)
            .await
    }

    /// PRD #20 R20-003/R20-004: identity-bearing, idempotent counterpart of
    /// [`Self::write_and_submit`]. Carries the agent identity + session the
    /// prompt was queued for (`expected_agent_id` / `expected_session_id`) and a
    /// stable `delivery_id`. The daemon compares the identity against the exact
    /// live registry target BEFORE writing and returns `stale` / `wrong-session`
    /// (without writing) on a rebind, and dedups on `delivery_id` so a retry
    /// after a lost response replays the first result instead of double-submitting.
    ///
    /// The additive fields ride ALONGSIDE the base `WriteAndSubmit` shape as JSON
    /// rather than widening the [`AttachRequest`] enum — its 2-field
    /// `WriteAndSubmit { pane_id, text }` literal is depended on by existing call
    /// sites, and a pre-PRD-20 daemon simply ignores the extra keys (degrading to
    /// pane-only authorization), so the wire stays forward + backward compatible
    /// and needs no `PROTOCOL_VERSION` bump.
    pub async fn write_and_submit_with_identity(
        &self,
        pane_id: &str,
        text: &str,
        expected_agent_id: Option<&str>,
        expected_session_id: Option<&str>,
        delivery_id: Option<&str>,
    ) -> Result<SendResult, ClientError> {
        // PRD #20 R20-006 (finding #6): an identity-bearing send DEPENDS on the
        // daemon's guarded-send guarantees (exact agent+session match, atomic
        // delivery-id dedup). If the daemon doesn't advertise that capability —
        // an older build that silently IGNORES the identity/idempotency fields
        // and just returns `ok=true` — FAIL SAFE and do NOT submit. Trusting that
        // unguarded `ok=true` would (a) let a lost-response retry double-submit
        // (no dedup), and (b) let a rebind receive the old prompt (no identity
        // check). Refusing preserves pre-PRD-20 fire-once semantics against an
        // old daemon. The plain (non-identity) `write_and_submit` path skips this
        // and stays legacy-compatible.
        let identity_bearing =
            expected_agent_id.is_some() || expected_session_id.is_some() || delivery_id.is_some();
        if identity_bearing && !self.daemon_advertises_guarded_send().await? {
            return Err(ClientError::Server(
                "daemon does not advertise guarded-send support; refusing to submit an \
                 identity-bearing prompt unguarded (would risk double-submit / mis-deliver)"
                    .into(),
            ));
        }
        let mut request = serde_json::json!({
            "op": "write-and-submit",
            "pane_id": pane_id,
            "text": text,
        });
        if let Some(v) = expected_agent_id {
            request["expected_agent_id"] = serde_json::Value::String(v.to_string());
        }
        if let Some(v) = expected_session_id {
            request["expected_session_id"] = serde_json::Value::String(v.to_string());
        }
        if let Some(v) = delivery_id {
            request["delivery_id"] = serde_json::Value::String(v.to_string());
        }
        let resp = self.issue_json_command(&request).await?;
        interpret_send_response(resp)
    }

    /// One-shot request/response for a hand-built JSON request. Used by
    /// [`Self::write_and_submit_with_identity`] to carry additive fields the
    /// [`AttachRequest`] enum doesn't declare, without widening the enum.
    async fn issue_json_command(
        &self,
        request: &serde_json::Value,
    ) -> Result<AttachResponse, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let payload = serde_json::to_vec(request)
            .map_err(|e| ClientError::Malformed(format!("request JSON: {e}")))?;
        write_frame(&mut wr, KIND_REQ, &payload).await?;
        read_response(&mut rd).await
    }

    /// PRD #20 R20-006 (finding #6): probe whether the daemon advertises the
    /// guarded-send capability on its `Hello` reply. `Ok(true)` only when the
    /// reply carries `guarded_send = Some(true)` — i.e. a daemon that enforces
    /// the identity/idempotency guards. `Ok(false)` for any older daemon that
    /// omits the field (so the caller fails a guarded send safe). A transport
    /// error surfaces as `Err` (also fail-safe: the caller does not submit).
    async fn daemon_advertises_guarded_send(&self) -> Result<bool, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::Hello {
                client_version: crate::daemon_protocol::PROTOCOL_VERSION,
                client_build_version: None,
            },
        )
        .await?;
        Ok(resp.guarded_send == Some(true))
    }

    /// PRD #819 M5: the daemon's advertised capability set for THIS endpoint,
    /// captured once at a handshake and cached for the life of the connection
    /// it describes.
    ///
    /// The first call performs one `Hello` exchange; later calls return the
    /// captured set without touching the socket. That is the whole point of the
    /// method: the [`Self::daemon_advertises_guarded_send`] probe below opens a
    /// **fresh connection and sends a second `Hello` every time it is
    /// consulted**, which is affordable for a check made once per prompt and is
    /// not affordable for a check made before every project-aware action.
    ///
    /// **What invalidates it**, and nothing else does:
    ///
    /// * a different endpoint — the cache is keyed by socket path, and a
    ///   snapshot taken against another daemon is never returned
    ///   ([`cached_capabilities_for`]);
    /// * an explicit [`Self::invalidate_capabilities`], which a caller issues on
    ///   reconnect — the daemon may have been replaced by a different build
    ///   while this handle was idle, and a set that outlives the connection it
    ///   describes is worse than no cache.
    ///
    /// A transport failure is NOT cached: it surfaces as `Err` and the next call
    /// re-handshakes. `Err` is itself fail-safe — a caller that cannot learn the
    /// capability withholds, exactly as it does for an absent one.
    ///
    /// Two concurrent first-callers may each handshake; the exchange is
    /// idempotent and the second store simply overwrites an identical snapshot.
    /// Holding the lock across the `.await` to prevent that would be a worse
    /// trade than one redundant `Hello`.
    pub async fn capabilities(&self) -> Result<DaemonCapabilities, ClientError> {
        if let Some(hit) = self.cached_capabilities() {
            return Ok(hit);
        }
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::Hello {
                client_version: crate::daemon_protocol::PROTOCOL_VERSION,
                client_build_version: None,
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(resp.error.unwrap_or_else(|| {
                "handshake failed while reading capabilities".into()
            })));
        }
        Ok(self.store_capabilities_from_hello(&resp))
    }

    /// Seed the cache from a `Hello` reply the caller already has.
    ///
    /// The handshake this PRD wants the set captured at is the one that already
    /// happens at startup ([`crate::build_version_handshake`]) or at the
    /// desktop's `trusted_daemon()` — both of which hold an
    /// [`AttachResponse`] and would otherwise force [`Self::capabilities`] to
    /// spend a second `Hello` re-learning what that reply already said. This is
    /// the seam for those sites; it is not wired to one yet, because with the
    /// TUI's project-resolution sites out of scope for PRD #819 there is no
    /// project-aware caller to seed it for (see the PRD's *Capability
    /// negotiation* section).
    ///
    /// Returns the captured set so a caller can act on it without a second read.
    pub fn store_capabilities_from_hello(&self, resp: &AttachResponse) -> DaemonCapabilities {
        let capabilities = DaemonCapabilities::from_hello(resp);
        let snapshot = CapabilitySnapshot {
            endpoint: self.socket_path.clone(),
            generation: DaemonGeneration::from_hello(resp),
            capabilities: capabilities.clone(),
        };
        *self.capabilities.lock().unwrap_or_else(|p| p.into_inner()) = Some(snapshot);
        capabilities
    }

    /// Drop the captured set, so the next [`Self::capabilities`] re-handshakes.
    /// Call this on reconnect: the daemon behind an unchanged socket path may be
    /// a different process, and a different build.
    pub fn invalidate_capabilities(&self) {
        *self.capabilities.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// The capture this handle currently holds for its own endpoint, if any.
    /// `None` means "not captured yet" — never "the daemon advertised nothing",
    /// which is [`DaemonCapabilities::absent`] and is a captured value.
    pub fn cached_capabilities(&self) -> Option<DaemonCapabilities> {
        let guard = self.capabilities.lock().unwrap_or_else(|p| p.into_inner());
        cached_capabilities_for(guard.as_ref(), &self.socket_path)
    }

    /// PRD #819 M5: the fail-safe check, following the
    /// [`Self::daemon_advertises_guarded_send`] precedent — it consults the
    /// explicit capability, NOT the protocol version, and it answers `false`
    /// rather than "probably" when the daemon said nothing.
    ///
    /// `Ok(false)` for an unadvertised daemon; `Err` for a transport failure,
    /// which a caller treats the same way (withhold) rather than as a licence to
    /// fall back to resolving the request itself.
    pub async fn daemon_advertises(&self, capability: &str) -> Result<bool, ClientError> {
        Ok(self.capabilities().await?.supports(capability))
    }

    /// [`Self::daemon_advertises`] with the decline already spelled: `Ok(())`
    /// only when `capability` was explicitly advertised, and otherwise the
    /// uniform withhold error from [`DaemonCapabilities::require`].
    ///
    /// This is what a project-aware call site puts in front of an
    /// [`AttachRequest::ListProjects`] / `ResolveProject` / `PrepareWorkflow`,
    /// so that an older daemon's clean `ok:false` — whose only discriminator is
    /// serde's `unknown variant …` text — is never reached, let alone matched.
    pub async fn require_capability(&self, capability: &str) -> Result<(), ClientError> {
        self.capabilities().await?.require(capability)
    }

    /// PRD #819 M6: the projects this daemon knows about. **Read-only.**
    ///
    /// Gated on the advertised capability rather than on the protocol version,
    /// so an older daemon is declined by [`Self::require_capability`] before a
    /// request it cannot parse is ever sent — its clean `ok:false` for an
    /// unknown op is discriminated only by serde's `unknown variant …` text,
    /// which is not a stability contract and is never matched here.
    ///
    /// An **empty** listing is a successful answer, not a failure: "this daemon
    /// has nothing live and its startup cwd is not a project" is the state the
    /// desktop renders its paste-a-path surface for.
    pub async fn list_projects(&self) -> Result<crate::event::ProjectListing, ClientError> {
        self.require_capability(crate::daemon_protocol::CAP_LIST_PROJECTS)
            .await?;
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(&mut rd, &mut wr, &AttachRequest::ListProjects {}).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "list-projects failed".into()),
            ));
        }
        resp.projects
            .ok_or_else(|| ClientError::Malformed("list-projects ok but no listing".into()))
    }

    /// PRD #819 M6: resolve ONE path. **Read-only**, and never a walk — see
    /// [`AttachRequest::ResolveProject`].
    ///
    /// `path` must be a path this daemon returned or one the **user** typed. A
    /// caller that derives it from its own environment reintroduces the exact
    /// defect this PRD removes: against a remote daemon the client's filesystem
    /// is not the daemon's, and the launch is silently wrong rather than failing.
    ///
    /// The reply's [`crate::event::ResolvedProject::path`] is the daemon's
    /// **canonical** spelling and may differ from the one sent. That is the
    /// string every later `PrepareWorkflow` and `StartAgent.cwd` must carry, not
    /// the one the caller had: canonicalising a symlinked path changes its
    /// basename, and an empty orchestration name is derived from the basename
    /// (PRD #220's bug, `crate::dispatch`).
    pub async fn resolve_project(
        &self,
        path: &str,
    ) -> Result<crate::event::ResolvedProject, ClientError> {
        self.require_capability(crate::daemon_protocol::CAP_RESOLVE_PROJECT)
            .await?;
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::ResolveProject {
                path: path.to_string(),
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "resolve-project failed".into()),
            ));
        }
        resp.project
            .ok_or_else(|| ClientError::Malformed("resolve-project ok but no project".into()))
    }

    /// PRD #819 M6: prepare a launch — the only project verb that writes.
    ///
    /// `path` is the daemon-canonical spelling from [`Self::list_projects`] or
    /// [`Self::resolve_project`]. `config_revision` is the one that resolve
    /// handed back; passing it is what closes the window between the picker and
    /// the write, and `None` means "no expectation" rather than "any revision"
    /// (see [`AttachRequest::PrepareWorkflow::config_revision`]).
    ///
    /// A failed preparation starts no roles, because it starts nothing at all:
    /// spawning is the caller's later `StartAgent` sequence, which presents
    /// [`crate::event::PreparedWorkflow::token`] through
    /// [`Self::start_agent_with_prep_token`].
    pub async fn prepare_workflow(
        &self,
        path: &str,
        orchestration: &str,
        task: &str,
        config_revision: Option<&str>,
    ) -> Result<crate::event::PreparedWorkflow, ClientError> {
        self.require_capability(crate::daemon_protocol::CAP_PREPARE_WORKFLOW)
            .await?;
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::PrepareWorkflow {
                path: path.to_string(),
                orchestration: orchestration.to_string(),
                task: task.to_string(),
                config_revision: config_revision.map(str::to_string),
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "prepare-workflow failed".into()),
            ));
        }
        resp.workflow_prepared
            .ok_or_else(|| ClientError::Malformed("prepare-workflow ok but no preparation".into()))
    }

    /// PRD #819 M4/M6: start one role of a workflow this daemon prepared.
    ///
    /// `prep_token: None` is byte-for-byte [`Self::start_agent`] and sends the
    /// ordinary `start-agent` op; a token sends
    /// [`AttachRequest::StartPreparedAgent`], on which the token is a **required
    /// field of the variant** rather than an additive key.
    ///
    /// # Why the distinct verb, and what it fixes on THIS side of the wire
    ///
    /// The token used to ride alongside `start-agent` as an extra JSON key, the
    /// way [`Self::write_and_submit_with_identity`]'s identity fields still do.
    /// That was fail-open: `start-agent` is an op every daemon back to PRD #76
    /// accepts, so an older one decoded the base variant, ignored the unknown
    /// key and started the role unenforced — and this client could not see it
    /// happen. A role start is its own short-lived connection: [`Self::connect`]
    /// is a bare `IpcStream::connect`, [`Self::issue_json_command`] writes one
    /// request frame and reads one response, and the daemon decodes exactly one
    /// request per connection. There is no `Hello` on it and nowhere to put one,
    /// so no amount of checking beforehand could cover the spawn itself.
    ///
    /// A distinct op needs no timing argument: a daemon that lacks the variant
    /// fails the decode and answers `ok: false` **on the spawn's own
    /// connection**, so the launch fails closed and starts nothing.
    ///
    /// # The capability gate, and why it is the lesser half
    ///
    /// [`crate::daemon_protocol::CAP_START_PREPARED_AGENT`] is required first,
    /// the way the three project verbs require theirs, so an unadvertising
    /// daemon is declined here rather than sent a request it cannot parse — its
    /// refusal text is serde's `unknown variant …`, which is not a stability
    /// contract and is never matched. But the gate reads a set captured at some
    /// earlier handshake, so it is an affordance, not the guarantee: the
    /// guarantee is the daemon's own refusal of an op it does not have, which
    /// costs no round trip and cannot go stale.
    ///
    /// The token is still not an authorization token — `crate::prep_token`'s
    /// module doc. Any peer here can spawn arbitrary commands through
    /// `start-agent` and is not slowed down by this; what the verb protects is a
    /// coordinator from launching against a preparation something else replaced.
    pub async fn start_agent_with_prep_token(
        &self,
        opts: StartAgentOptions,
        prep_token: Option<&str>,
    ) -> Result<String, ClientError> {
        let Some(token) = prep_token else {
            return self.start_agent(opts).await;
        };
        self.require_capability(crate::daemon_protocol::CAP_START_PREPARED_AGENT)
            .await?;
        let req = AttachRequest::StartPreparedAgent {
            prep_token: token.to_string(),
            command: opts.command,
            cwd: opts.cwd,
            display_name: opts.display_name,
            rows: opts.rows,
            cols: opts.cols,
            env: opts.env,
            tab_membership: opts.tab_membership,
            agent_type: opts.agent_type,
            seed: opts.seed,
        };
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(&mut rd, &mut wr, &req).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "start-prepared-agent failed".into()),
            ));
        }
        resp.id.ok_or_else(|| {
            ClientError::Malformed("start-prepared-agent ok but no id in response".into())
        })
    }

    /// Push a TUI pane resize through to the daemon's PTY. Idempotent on the
    /// wire: each call opens a fresh short-lived connection (matching the
    /// pattern used for `stop_agent` / `list_agents`). Callers that fire
    /// resize on every layout pass should treat transient errors as
    /// best-effort — the next resize will reconcile.
    pub async fn resize_agent(&self, id: &str, rows: u16, cols: u16) -> Result<(), ClientError> {
        self.resize_agent_as_viewer(id, rows, cols, None)
            .await
            .map(|_| ())
    }

    /// PRD #882 — ask the daemon to size an agent for one VIEWER, and learn what
    /// it actually applied.
    ///
    /// `viewer` is the token this client received when it attached. With it, the
    /// request updates that viewer's constraint and the daemon applies the
    /// smallest viewport among everyone attached; without it the request is
    /// applied directly and joins no minimum (the pre-#882 behaviour, kept for
    /// callers that are not rendering viewers).
    ///
    /// **The returned pair is the geometry in force, which is not necessarily
    /// the one asked for.** Size the local parser from it. `None` means the
    /// daemon predates #882 and echoed nothing, in which case the caller's own
    /// request is the right assumption — that daemon has no policy to disagree
    /// with it.
    pub async fn resize_agent_as_viewer(
        &self,
        id: &str,
        rows: u16,
        cols: u16,
        viewer: Option<&str>,
    ) -> Result<Option<(u16, u16)>, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::Resize {
                id: id.to_string(),
                rows,
                cols,
                viewer: viewer.map(|v| v.to_string()),
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "resize failed".into()),
            ));
        }
        Ok(match (resp.applied_rows, resp.applied_cols) {
            (Some(r), Some(c)) => Some((r, c)),
            _ => None,
        })
    }

    /// Update the daemon-side display_name and/or cwd for an agent (M2.11).
    /// Passing `None` for either field clears it. The daemon validates both
    /// values independently and silently drops anything that fails — see
    /// `AgentPtyRegistry::set_agent_label` for the rules. Best-effort: the
    /// TUI calls this from the rename flow on every keystroke commit, so a
    /// transient daemon error here is logged at the call site, not
    /// propagated.
    pub async fn set_agent_label(
        &self,
        id: &str,
        display_name: Option<String>,
        cwd: Option<String>,
    ) -> Result<(), ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::SetAgentLabel {
                id: id.to_string(),
                display_name,
                cwd,
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "set-agent-label failed".into()),
            ));
        }
        Ok(())
    }

    pub async fn stop_agent(&self, id: &str) -> Result<(), ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::StopAgent { id: id.to_string() },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "stop-agent failed".into()),
            ));
        }
        Ok(())
    }

    /// PRD #76 M2.17: open a long-lived `SubscribeEvents` connection.
    /// Returns once the daemon has confirmed the subscription with a
    /// successful RESP — subsequent frames on the wire are `KIND_EVENT`
    /// (one per hook event broadcast by the daemon) until the daemon
    /// closes the stream (`KIND_STREAM_END` carrying the reason) or
    /// either side drops the socket.
    pub async fn subscribe_events(&self) -> Result<EventSubscription, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(&mut rd, &mut wr, &AttachRequest::SubscribeEvents).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "subscribe-events failed".into()),
            ));
        }
        // Keep the write half alive for the lifetime of the subscription.
        // The daemon races a one-byte read on its side against rx.recv() to
        // detect client disconnect — dropping wr here would shut down our
        // side via SHUT_WR and trip that detector immediately, tearing the
        // subscription down before any events flow. Letting wr live until
        // EventSubscription drops means the daemon sees EOF exactly when
        // the client actually goes away.
        Ok(EventSubscription { rd, _wr: wr })
    }

    /// PRD #92 F1: send a `KIND_SHUTDOWN` header-only frame and wait
    /// for the daemon's explicit `KIND_SHUTDOWN_ACK` reply. Used by
    /// the **Stop** option in the Ctrl+C dialog.
    ///
    /// PRD #92 F1 followup (reviewer-blocker fix): the original wire
    /// used "socket close == ack" semantics, which a daemon running
    /// the previous binary (predating `PROTOCOL_VERSION = 2`) would
    /// also satisfy by closing the connection on an unknown frame
    /// kind. The TUI then thought shutdown had succeeded and exited
    /// while the daemon was still running — a silent-failure during
    /// the inevitable upgrade-mismatch window. The explicit
    /// `KIND_SHUTDOWN_ACK` lets the client distinguish the two cases
    /// and surface a real error.
    ///
    /// Three failure modes — all surface as `Err`:
    ///   - Timeout (1s elapsed without any frame on the wire).
    ///   - EOF (daemon closed the socket without sending an ack —
    ///     typically the upgrade-mismatch case).
    ///   - Any frame received whose kind is not `KIND_SHUTDOWN_ACK`.
    ///
    /// Success: a single `KIND_SHUTDOWN_ACK` frame arrives. We return
    /// `Ok(())` and let the caller exit; the daemon's actual teardown
    /// is asynchronous from that point (SIGTERM grace + SIGKILL) but
    /// the user's commit has been acknowledged.
    pub async fn send_shutdown(&self) -> Result<(), ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        write_frame(&mut wr, KIND_SHUTDOWN, &[]).await?;
        // Bound the wait at 1s — the daemon writes the ack BEFORE
        // beginning teardown (so the wire ordering is honest even when
        // the registry drain takes the full 3-second SIGTERM grace),
        // so a daemon that recognised the frame should respond in
        // sub-millisecond. 1s is comfortable headroom for unusual
        // scheduler stalls.
        let read_result =
            tokio::time::timeout(std::time::Duration::from_secs(1), read_frame(&mut rd)).await;
        match read_result {
            Ok(Ok(Some((kind, _payload)))) if kind == KIND_SHUTDOWN_ACK => Ok(()),
            Ok(Ok(Some((kind, _)))) => Err(ClientError::Server(format!(
                "expected KIND_SHUTDOWN_ACK (0x{:02x}), got kind 0x{:02x} — daemon may predate PROTOCOL_VERSION 2",
                KIND_SHUTDOWN_ACK, kind
            ))),
            Ok(Ok(None)) => Err(ClientError::Server(
                "daemon closed connection without acknowledging KIND_SHUTDOWN — possibly a binary predating PROTOCOL_VERSION 2"
                    .to_string(),
            )),
            Ok(Err(e)) => Err(ClientError::Io(e)),
            Err(_) => Err(ClientError::Server(
                "timed out waiting for KIND_SHUTDOWN_ACK after 1 second — daemon is unresponsive"
                    .to_string(),
            )),
        }
    }

    /// Open an attach-stream connection. Returns once the daemon has
    /// confirmed the attach with a successful RESP — i.e. the next frame on
    /// the wire is the consistent scrollback snapshot, followed by live
    /// STREAM_OUT frames (see [`crate::daemon_protocol`]'s state-machine
    /// docs).
    pub async fn attach(&self, id: &str) -> Result<AttachConnection, ClientError> {
        self.attach_as_viewer(id, None).await
    }

    /// PRD #882 — attach as a policy participant that has not measured itself
    /// yet.
    ///
    /// Registers as a viewer and receives geometry pushes, but contributes no
    /// constraint until its first resize. This is what a client whose attach
    /// runs before its layout does needs: attaching as a non-participant and
    /// resizing afterwards would send that resize with no token, and the daemon
    /// would apply it as an unattributed override across every other client.
    pub async fn attach_pending_viewport(&self, id: &str) -> Result<AttachConnection, ClientError> {
        self.attach_inner(id, None, true).await
    }

    /// PRD #882 — attach, optionally declaring the geometry this client can draw
    /// the agent at.
    ///
    /// Passing a viewport opts into the size policy in both directions: the
    /// agent is sized to the smallest viewport among attached viewers, and this
    /// connection is told (via `KIND_GEOMETRY`) whenever that changes. Passing
    /// `None` is the pre-#882 behaviour — no constraint contributed, no
    /// geometry frames delivered — and is what a non-rendering observer wants.
    ///
    /// The returned connection carries the viewer token and the geometry in
    /// force at attach time. **Size the parser from `applied`, not from the
    /// viewport asked for**: the replayed scrollback was written at the applied
    /// geometry, and the two differ whenever a smaller viewer is already
    /// attached.
    pub async fn attach_as_viewer(
        &self,
        id: &str,
        viewport: Option<(u16, u16)>,
    ) -> Result<AttachConnection, ClientError> {
        // A caller that names a viewport always participates; one that does not
        // is the legacy shape and stays out of the policy entirely.
        let participates = viewport.is_some();
        self.attach_inner(id, viewport, participates).await
    }

    async fn attach_inner(
        &self,
        id: &str,
        viewport: Option<(u16, u16)>,
        geometry_updates: bool,
    ) -> Result<AttachConnection, ClientError> {
        let (mut rd, mut wr) = self.connect().await?;
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::AttachStream {
                id: id.to_string(),
                rows: viewport.map(|(r, _)| r),
                cols: viewport.map(|(_, c)| c),
                geometry_updates,
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "attach-stream failed".into()),
            ));
        }
        let applied = match (resp.applied_rows, resp.applied_cols) {
            (Some(r), Some(c)) => Some((r, c)),
            _ => None,
        };
        Ok(AttachConnection {
            rd,
            wr,
            viewer: resp.viewer,
            applied,
        })
    }
}

/// Long-lived `SubscribeEvents` connection (PRD #76 M2.17, extended in
/// M2.19 to also carry delegate signals). Yields one [`BroadcastMsg`]
/// per `next_event` call until the daemon ends the stream
/// (`KIND_STREAM_END` — typically `"lagged"` when the broadcast
/// receiver fell behind) or the socket drops. Callers reconnect via
/// [`DaemonClient::subscribe_events`].
pub struct EventSubscription {
    rd: TransportReadHalf,
    /// Held purely as a lifetime signal: dropping the subscription drops
    /// `_wr`, which half-closes the transport — on the Unix backend a
    /// `shutdown(SHUT_WR)` — tripping the daemon's read-side disconnect
    /// detector and tearing the per-connection receiver down promptly. Never
    /// written to after the request.
    ///
    /// PRD #741 M3: this is now a boxed
    /// [`TransportWriteHalf`], and the drop behaviour survives the box because
    /// the box holds the backend's own half, whose `Drop` runs unchanged. The
    /// thing that would lose it is [`tokio::io::split`], which is why
    /// [`crate::platform::transport::TransportWriteHalf::new`] takes a
    /// [`HalfCloseOnDrop`](crate::platform::transport::HalfCloseOnDrop) rather
    /// than an `AsyncWrite`. `transport::tests::
    /// dropping_the_write_half_alone_gives_the_server_eof` is the proof.
    _wr: TransportWriteHalf,
}

impl EventSubscription {
    /// Read the next [`BroadcastMsg`] from the subscription. Returns
    /// `Ok(None)` on `KIND_STREAM_END`, peer EOF, or an unexpected
    /// frame kind (logged via `tracing::warn!`) — the caller should
    /// drop and reconnect. A malformed JSON payload is returned as
    /// `Err(io::Error)` so the caller can decide whether to reconnect
    /// or surface the bug.
    pub async fn next_event(&mut self) -> io::Result<Option<BroadcastMsg>> {
        loop {
            match read_frame(&mut self.rd).await? {
                None => return Ok(None),
                Some((KIND_EVENT, payload)) => {
                    match serde_json::from_slice::<BroadcastMsg>(&payload) {
                        // PRD #120 H1/M1/L2: validate the daemon-supplied
                        // orchestration surface at the wire boundary — BEFORE the
                        // render loop synthesizes a role vec sized to
                        // `max(role_index) + 1` (which a hostile/buggy index would
                        // OOM). Mirrors `sanitize_record_tab_membership` for the
                        // reconnect path. A REJECTED surface is dropped and we read
                        // the next frame, rather than ending the stream (which
                        // would trigger a needless reconnect).
                        Ok(BroadcastMsg::OrchestrationSurface(surface)) => {
                            match validate_orchestration_surface(surface) {
                                Some(v) => {
                                    return Ok(Some(BroadcastMsg::OrchestrationSurface(v)));
                                }
                                None => {
                                    tracing::warn!(
                                        "subscribe_events: dropping invalid OrchestrationSurface \
                                         (failed wire-boundary validation)"
                                    );
                                    continue;
                                }
                            }
                        }
                        Ok(msg) => return Ok(Some(msg)),
                        Err(e) => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("malformed KIND_EVENT payload: {e}"),
                            ));
                        }
                    }
                }
                Some((KIND_STREAM_END, reason)) => {
                    if !reason.is_empty() {
                        tracing::warn!(
                            reason = %String::from_utf8_lossy(&reason),
                            "subscribe_events: daemon ended stream"
                        );
                    }
                    return Ok(None);
                }
                Some((kind, _)) => {
                    tracing::warn!(
                        "unexpected frame kind 0x{kind:02x} on subscribe-events stream — ending"
                    );
                    return Ok(None);
                }
            }
        }
    }
}

/// Live attach-stream connection. After a successful [`DaemonClient::attach`]
/// the next read returns the daemon-supplied scrollback snapshot, then live
/// STREAM_OUT frames until the agent exits or the client detaches.
pub struct AttachConnection {
    /// PRD #741 M3: boxed transport halves rather than the IPC backend's own,
    /// so a PTY stream can run over something that is not a Unix socket or a
    /// named pipe. `wr`'s drop still half-closes — see
    /// [`EventSubscription`]'s `_wr` for why that is not automatic.
    rd: TransportReadHalf,
    wr: TransportWriteHalf,
    /// PRD #882 — the viewer token for this attach, or `None` when no viewport
    /// was declared. Pass it to [`DaemonClient::resize_agent_as_viewer`] so a
    /// resize updates this view's constraint instead of overriding everyone.
    viewer: Option<String>,
    /// PRD #882 — the geometry the daemon has applied, as of the last frame
    /// read. Seeded from the attach response and updated by every
    /// `KIND_GEOMETRY` frame, so a caller polling this after each read always
    /// has the grid the bytes it just received were written for.
    applied: Option<(u16, u16)>,
}

impl AttachConnection {
    /// Read the next chunk of agent output. Returns `Ok(None)` on
    /// `STREAM_END` or peer EOF — the stream is over and the caller should
    /// drop the connection. Unexpected frame kinds are logged via `tracing`
    /// and treated as EOF (the daemon closes the connection on protocol
    /// violations rather than sending `STREAM_END`).
    pub async fn next_output(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            match read_frame(&mut self.rd).await? {
                None => return Ok(None),
                Some((KIND_STREAM_OUT, bytes)) => return Ok(Some(bytes)),
                Some((KIND_STREAM_END, _)) => return Ok(None),
                // PRD #882: absorb the geometry push rather than ending on it.
                // Only an attach that declared a viewport is ever sent one, and
                // such a caller reads the value back through
                // [`AttachConnection::applied`] after each read; treating it as
                // end-of-stream — which the `_` arm below would — would tear the
                // pane down the first time a second client attached.
                Some((crate::daemon_protocol::KIND_GEOMETRY, bytes)) => {
                    if let Some((rows, cols)) = crate::daemon_protocol::parse_geometry_frame(&bytes)
                    {
                        self.applied = Some((rows, cols));
                    }
                }
                Some((kind, _)) => {
                    tracing::warn!("unexpected frame kind 0x{kind:02x} on attach stream — ending");
                    return Ok(None);
                }
            }
        }
    }

    /// PRD #882 — the viewer token minted for this attach, if it declared a
    /// viewport.
    pub fn viewer(&self) -> Option<&str> {
        self.viewer.as_deref()
    }

    /// PRD #882 — the geometry the daemon has applied for this agent, as of the
    /// last frame read through [`AttachConnection::next_output`].
    pub fn applied(&self) -> Option<(u16, u16)> {
        self.applied
    }

    /// Forward a chunk of keystrokes to the daemon's PTY writer.
    pub async fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_frame(&mut self.wr, crate::daemon_protocol::KIND_STREAM_IN, bytes).await
    }

    /// Send an explicit DETACH frame. Best-effort — if the write fails the
    /// daemon will still observe the close as detach when the socket is
    /// dropped.
    pub async fn detach(mut self) -> io::Result<()> {
        write_frame(&mut self.wr, KIND_DETACH, &[]).await
    }

    /// Split into owned halves for callers that drive read and write tasks
    /// concurrently (the typical pane wiring).
    pub fn into_split(self) -> (TransportReadHalf, TransportWriteHalf) {
        (self.rd, self.wr)
    }

    /// Test-only: an `AttachConnection` over an already-connected in-process
    /// socket pair, so a unit test can drive code that *takes* one without a
    /// daemon, a listener, or a filesystem path. The returned peer half is the
    /// other end — hold it to keep the connection open, drop it to give the
    /// reader EOF.
    ///
    /// `#[cfg(test)]`, so it is absent from the release library: PRD #341's
    /// finding was that release-exposed test seams are a real attack surface,
    /// and this deliberately is not one. Unix-only because
    /// [`tokio::net::UnixStream::pair`] is; the Windows named-pipe backend has
    /// no in-process equivalent, and the only consumer is likewise Unix-gated.
    #[cfg(all(test, unix))]
    pub(crate) fn connected_pair_for_test() -> (Self, tokio::net::UnixStream) {
        let (ours, peer) = tokio::net::UnixStream::pair().expect("socket pair for test");
        // PRD #741 M3: the native split, then boxed — so the test seam carries
        // the same `SHUT_WR`-on-drop write half production does. Going through
        // `tokio::io::split` here would give a test double whose teardown does
        // not match the thing under test.
        let (rd, wr) = ours.into_split();
        let rd = TransportReadHalf::new(rd);
        let wr = TransportWriteHalf::new(wr);
        (
            Self {
                rd,
                wr,
                viewer: None,
                applied: None,
            },
            peer,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `spec::spec` is needed cross-platform by `pane_input_011` (a pure serde
    // decode test that stays cross-platform below).
    use spec::spec;
    // PRD #42 M8/review B1: the attach-server harness below binds a real
    // listener at a filesystem tempdir path and spawns `/bin/sh`, neither of
    // which exists on Windows (`IpcListener::bind` on a non-`\\.\pipe\` path
    // → `ERROR_INVALID_NAME`; no `/bin/sh`). Gate the harness + the tests that
    // use it to Unix so the Windows `cargo nextest run` step compiles and does
    // not panic. The pure tests below (`ensure_socket_exists_reports_missing`,
    // `sanitize_record_tab_membership_*`, `run_now_outcome_*`,
    // `pane_input_011`) stay cross-platform. No Unix coverage is lost — all of
    // these still run on Unix. (PRD #20's `pane_input_012`/`015` drive the
    // socket harness, so they join the Unix-gated set.)
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};
    #[cfg(unix)]
    use tempfile::TempDir;

    #[cfg(unix)]
    use crate::agent_pty::AgentPtyRegistry;
    use crate::daemon_protocol::{
        CAP_LIST_PROJECTS, CAP_PREPARE_WORKFLOW, CAP_RESOLVE_PROJECT, DAEMON_CAPABILITIES,
        PROTOCOL_VERSION,
    };
    #[cfg(unix)]
    use crate::daemon_protocol::{bind_attach_listener, serve_attach};
    #[cfg(unix)]
    use tokio::sync::broadcast;

    /// Mirror the harness lock from `tests/daemon_protocol.rs`: `bind_socket`
    /// flips the process-global umask while binding, and a tempdir created
    /// inside that window inherits 0o600, breaking later binds. Hold this
    /// across tempdir+bind for any in-process attach server.
    #[cfg(unix)]
    static BIND_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    async fn spawn_test_server() -> (TempDir, PathBuf, Arc<AgentPtyRegistry>) {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (dir, path, listener) = {
            let _g = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("attach.sock");
            let listener = bind_attach_listener(&path).expect("bind");
            (dir, path, listener)
        };
        let reg = registry.clone();
        let (event_tx, _) = broadcast::channel(16);
        tokio::spawn(async move {
            let _ = serve_attach(listener, reg, event_tx).await;
        });
        (dir, path, registry)
    }

    /// PRD #741 M2, the headline property: the local-only operations take a
    /// [`LocalEndpoint`], and the only route from an [`Endpoint`] to one refuses
    /// a remote deck. This is the runtime half of a property the compiler
    /// already enforces — `run_daemon_stop(&Endpoint::Remote(..))` does not
    /// build, so there is nothing here to assert about it — and what it pins is
    /// that the *accessor* keeps returning `None` rather than being "helpfully"
    /// widened later to hand back some substitute local endpoint.
    #[test]
    fn a_remote_endpoint_yields_no_local_endpoint() {
        let local = Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock"));
        assert_eq!(
            local.as_local().map(LocalEndpoint::path),
            Some(Path::new("/tmp/attach.sock")),
            "a local deck must hand back the endpoint the local-only operations take"
        );

        let remote = Endpoint::Remote(RemoteEndpoint::named("build-box"));
        assert!(
            remote.as_local().is_none(),
            "a remote deck must yield NO local endpoint — this is what makes \
             peer_pid termination, the stale-inode unlink and lazy-spawn \
             unreachable for it"
        );
    }

    /// The refusal a user reads when they press Stop or Replace against a remote
    /// deck: it names the operation and the deck, and it says *why* rather than
    /// reporting a bare failure. M7 turns this text into a disabled control.
    #[test]
    fn require_local_refuses_a_remote_deck_by_name() {
        let remote = Endpoint::Remote(RemoteEndpoint::named("build-box"));
        let err = remote
            .require_local("Stop daemon")
            .expect_err("a remote deck must refuse a local-only operation");
        assert_eq!(
            err,
            EndpointError::LocalOnly {
                operation: "Stop daemon",
                deck: "build-box".to_string(),
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("Stop daemon"), "name the operation: {msg}");
        assert!(msg.contains("build-box"), "name the deck: {msg}");
        assert!(
            msg.contains("this machine"),
            "say why it cannot apply: {msg}"
        );

        let local = Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock"));
        assert!(
            local.require_local("Stop daemon").is_ok(),
            "a local deck must still be stoppable — the guard must not refuse everything"
        );
    }

    /// The connect path for a remote deck returns a message, not a panic: M5 has
    /// not landed, and a `todo!()` here would crash the app on the first
    /// selection rather than explain itself.
    #[test]
    fn connecting_to_a_remote_deck_errors_rather_than_panicking() {
        let remote = Endpoint::Remote(RemoteEndpoint::named("build-box"));
        let err = remote
            .connect_address()
            .expect_err("M5 has not landed; connecting must refuse");
        assert_eq!(
            err,
            EndpointError::RemoteTransportUnavailable {
                deck: "build-box".to_string(),
            }
        );
        assert!(
            err.to_string().contains("not supported yet"),
            "the refusal must read as unfinished work, not as a broken deck: {err}"
        );
    }

    /// `Local` is byte-identical, which is the milestone's other half. Two
    /// things pin it: the connect address is exactly the path handed in (not a
    /// normalised or re-derived one), and `from_config()` is exactly
    /// `config::attach_socket_path()` — the value every call site passed before
    /// this type existed, `DOT_AGENT_DECK_ATTACH_SOCKET` override included.
    #[test]
    fn a_local_endpoint_still_resolves_to_todays_attach_path() {
        let explicit = Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock"));
        assert_eq!(
            explicit.connect_address().expect("a local deck connects"),
            Path::new("/tmp/attach.sock")
        );

        assert_eq!(
            LocalEndpoint::from_config().path(),
            crate::config::attach_socket_path(),
            "the configured local endpoint must be exactly the path this crate \
             has always used, or the local case is not byte-identical"
        );
    }

    /// The desktop's connection banner renders `describe()`, and for a local
    /// deck that has to stay the socket path it showed before — otherwise a
    /// milestone that adds no user-visible behaviour has changed a user-visible
    /// string.
    #[test]
    fn describing_a_local_deck_still_prints_its_socket_path() {
        assert_eq!(
            Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock")).describe(),
            "/tmp/attach.sock"
        );
        assert_eq!(
            Endpoint::Remote(RemoteEndpoint::named("build-box")).describe(),
            "build-box"
        );
    }

    /// Unix only since PRD #741 M3, and the gate is the fix rather than a
    /// retreat. `ensure_socket_exists` used to `stat` unconditionally, so on
    /// Windows — where the address is a `\\.\pipe\…` name that never
    /// exists — it reported *every* live daemon missing. It now answers only
    /// where a `stat` means something, and this test asserts the arm where it
    /// does; the other two arms are
    /// `ensure_socket_exists_stays_silent_when_a_stat_cannot_answer`.
    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_socket_exists_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.sock");
        let client = DaemonClient::new(missing.clone());
        let err = client.ensure_socket_exists().unwrap_err();
        assert!(matches!(err, ClientError::SocketMissing(p) if p == missing));
    }

    /// PRD #741 M3, the third answer at the fourth predicate: an address a
    /// `stat` cannot speak for must not be reported as a missing daemon.
    ///
    /// Two ways to arrive there, and the test drives both on every platform by
    /// naming the presence rather than relying on which one is native. A remote
    /// deck is the one that matters for M5 — its address really does exist
    /// (DECISION 1A forwards a socket onto this filesystem), so a predicate that
    /// trusted `exists()` would have reported a *healthy* deck for a dead one.
    #[test]
    fn ensure_socket_exists_stays_silent_when_a_stat_cannot_answer() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("does-not-exist.sock");

        // A named pipe: nothing to stat, so nothing may be concluded.
        let pipe_client = DaemonClient {
            socket_path: absent.clone(),
            presence: EndpointPresence::NoFilesystemName,
            capabilities: Arc::new(Mutex::new(None)),
        };
        assert!(
            pipe_client.ensure_socket_exists().is_ok(),
            "a pipe name that cannot exist must not read as a missing daemon"
        );

        // A remote deck, with nothing at the address: still unanswerable here,
        // because the thing that would be there is the tunnel and not the deck.
        let remote_client = DaemonClient {
            socket_path: absent,
            presence: EndpointPresence::Elsewhere,
            capabilities: Arc::new(Mutex::new(None)),
        };
        assert!(
            remote_client.ensure_socket_exists().is_ok(),
            "a deck on another machine cannot be declared gone by a local stat"
        );
    }

    /// PRD #741 M3: [`DaemonClient::new`] is byte-identical to what it was, and
    /// [`DaemonClient::for_endpoint`] agrees with it for a local deck. The two
    /// differ only in what a `stat` is allowed to mean, which is the whole
    /// point of the field.
    #[test]
    fn for_endpoint_matches_new_for_a_local_deck_and_refuses_a_remote_one() {
        let local = Endpoint::Local(LocalEndpoint::at("/tmp/attach.sock"));
        let from_endpoint = DaemonClient::for_endpoint(&local).expect("a local deck connects");
        let from_path = DaemonClient::new("/tmp/attach.sock".into());
        assert_eq!(from_endpoint.socket_path(), from_path.socket_path());
        assert_eq!(from_endpoint.presence, from_path.presence);
        assert_eq!(from_path.presence, LOCAL_ENDPOINT_PRESENCE);

        let remote = Endpoint::Remote(RemoteEndpoint::named("build-box"));
        assert_eq!(
            remote.presence(),
            EndpointPresence::Elsewhere,
            "a remote deck's address is never the daemon's inode"
        );
        assert!(
            DaemonClient::for_endpoint(&remote).is_err(),
            "M5 has not landed; building a client for a remote deck must refuse"
        );
    }

    /// PRD #741 M3 test-plan item 4, at the client seam rather than at the
    /// transport's own: [`DaemonClient::connect`] now hands back boxed halves,
    /// and the half-close has to survive the box.
    ///
    /// Drives a scripted socket — accept, read the request frame, answer — and
    /// then asserts the server observes EOF once the client drops **only** its
    /// write half. That ordering is the whole test: dropping both halves closes
    /// the socket on any implementation, so a version that kept them together
    /// would pass while the property was gone. It is the property
    /// [`EventSubscription`]'s `_wr` exists for, and the one
    /// `platform/ipc/mod.rs`'s module docs record an earlier draft silently
    /// regressing by splitting with [`tokio::io::split`].
    ///
    /// Failure mode is a hang, not a wrong value, so the server's second read is
    /// bounded — an unbounded `read_frame` on a regressed build would wedge the
    /// run instead of reporting.
    ///
    /// Binds a plain `tokio::net::UnixListener` rather than going through
    /// `spawn_test_server`: the subject is the transport teardown, not the
    /// protocol, and `bind_attach_listener` flips the process-global umask
    /// (which is what `BIND_LOCK` exists to contain).
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_clients_write_half_lets_the_daemon_see_the_disconnect() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");

        let server = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept one client");
            let (mut reader, mut writer) = stream.into_split();
            let (kind, _payload) = read_frame(&mut reader)
                .await
                .expect("read the request")
                .expect("the client sent a frame");
            assert_eq!(kind, KIND_REQ);
            let ok = serde_json::to_vec(&AttachResponse::ok()).expect("serialize");
            write_frame(&mut writer, KIND_RESP, &ok)
                .await
                .expect("answer the client");
            // The daemon's disconnect detector is a read that must complete.
            tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut reader)).await
        });

        let client = DaemonClient::new(socket);
        let (mut rd, mut wr) = client.connect().await.expect("connect");
        let resp = issue_command(&mut rd, &mut wr, &AttachRequest::SubscribeEvents)
            .await
            .expect("the scripted daemon answers");
        assert!(resp.ok);
        wr.flush().await.expect("flush");

        drop(wr);
        // `rd` stays alive on purpose — see the doc comment.
        let observed = server
            .await
            .expect("the scripted daemon must not panic")
            .expect(
                "the daemon must observe EOF once the client's write half drops — without the \
                 half-close it stays blocked here and never tears the subscription down",
            )
            .expect("EOF is not an I/O error");
        assert!(
            observed.is_none(),
            "EOF must arrive as a clean end-of-stream, got frame {observed:?}"
        );
        drop(rd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_list_stop_round_trip() {
        let (_dir, path, registry) = spawn_test_server().await;
        let client = DaemonClient::new(path);

        let id = client
            .start_agent(StartAgentOptions {
                command: Some("/bin/sh".into()),
                ..Default::default()
            })
            .await
            .expect("start should succeed");

        let agents = client.list_agents().await.unwrap();
        let ids: Vec<String> = agents.iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids, vec![id.clone()]);

        client.stop_agent(&id).await.expect("stop should succeed");
        assert!(registry.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_agent_blank_command_returns_server_error() {
        let (_dir, path, _registry) = spawn_test_server().await;
        let client = DaemonClient::new(path);
        let err = client
            .start_agent(StartAgentOptions {
                command: Some("   ".into()),
                ..Default::default()
            })
            .await
            .expect_err("blank command should fail");
        assert!(matches!(err, ClientError::Server(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_streams_output_and_input() {
        let (_dir, path, registry) = spawn_test_server().await;
        let client = DaemonClient::new(path);

        let id = client
            .start_agent(StartAgentOptions {
                command: Some("/bin/sh".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        let mut conn = client.attach(&id).await.expect("attach");

        // Drive output via STREAM_IN; observe it via STREAM_OUT.
        conn.write_input(b"echo CLIENT-MARKER\n").await.unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut acc = Vec::new();
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, conn.next_output()).await {
                Ok(Ok(Some(bytes))) => {
                    acc.extend_from_slice(&bytes);
                    if acc
                        .windows(b"CLIENT-MARKER".len())
                        .any(|w| w == b"CLIENT-MARKER")
                    {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            acc.windows(b"CLIENT-MARKER".len())
                .any(|w| w == b"CLIENT-MARKER"),
            "expected marker in stream; got {:?}",
            String::from_utf8_lossy(&acc)
        );

        registry.close_agent(&id).unwrap();
    }

    /// Scenario: Decode a future daemon response carrying a send-result value
    /// this client does not know. The response must remain decodable and the
    /// unknown value must not be interpreted as delivered.
    #[spec("prompt/pane-input/011")]
    #[test]
    fn pane_input_011_unknown_send_result_decodes_as_safe_non_delivery() {
        let decoded = serde_json::from_value::<AttachResponse>(serde_json::json!({
            "ok": false,
            "send_result": "future-delivery-outcome"
        }));

        assert!(
            decoded.is_ok(),
            "an unknown send_result must not reject the whole AttachResponse: {decoded:?}"
        );
        let response = decoded.unwrap();
        assert!(
            !matches!(
                response.send_result,
                Some(SendResult::Applied | SendResult::Queued)
            ),
            "an unknown send_result must degrade to safe non-delivery: {:?}",
            response.send_result
        );
    }

    /// Scenario: Have a synthetic daemon return the inconsistent combination
    /// `ok=false` with `send_result=applied`. The client must let the failure
    /// bit win and must not report successful delivery.
    // PRD #42 M8: drives the Unix-domain-socket attach harness (`BIND_LOCK`,
    // `bind_attach_listener`), so it is Unix-gated like the other harness tests.
    #[cfg(unix)]
    #[spec("prompt/pane-input/012")]
    #[test]
    fn pane_input_012_ok_false_overrides_applied_send_result() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build send-result consistency runtime");
        runtime.block_on(pane_input_012_ok_false_overrides_applied_send_result_inner());
    }

    #[cfg(unix)]
    async fn pane_input_012_ok_false_overrides_applied_send_result_inner() {
        let (dir, path, listener) = {
            let _g = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("inconsistent-response.sock");
            let listener = bind_attach_listener(&path).expect("bind synthetic daemon");
            (dir, path, listener)
        };
        let server = tokio::spawn(async move {
            // PRD #42 M2: `IpcListener::accept()` yields the stream directly (no
            // `(stream, addr)` tuple like the raw `UnixListener`).
            let mut stream = listener.accept().await.expect("accept client");
            let _request = read_frame(&mut stream).await.expect("read request frame");
            let response = AttachResponse {
                ok: false,
                error: Some("delivery was not accepted".into()),
                send_result: Some(SendResult::Applied),
                ..Default::default()
            };
            crate::daemon_protocol::write_resp(&mut stream, &response)
                .await
                .expect("write inconsistent response");
        });
        let client = DaemonClient::new(path);

        let result = client
            .write_and_submit("pane-inconsistent", "must not report success")
            .await;
        server.await.unwrap();
        drop(dir);

        assert!(
            !matches!(result, Ok(SendResult::Applied | SendResult::Queued)),
            "ok=false must win over a contradictory delivered result; got {result:?}"
        );
    }

    /// Scenario: Point a new identity-bearing send client at a synthetic older
    /// daemon whose handshake does not advertise guarded-send support. The client
    /// must fail before submitting rather than trust an unsafe legacy `ok=true`.
    // PRD #42 M8: drives the Unix-domain-socket attach harness (`BIND_LOCK`,
    // `bind_attach_listener`, `Arc`/`AtomicUsize`), so it is Unix-gated like the
    // other harness tests.
    #[cfg(unix)]
    #[spec("prompt/pane-input/015")]
    #[test]
    fn pane_input_015_guarded_send_fails_safe_without_daemon_capability() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build guarded-send runtime");
        runtime.block_on(pane_input_015_guarded_send_fails_safe_without_daemon_capability_inner());
    }

    #[cfg(unix)]
    async fn pane_input_015_guarded_send_fails_safe_without_daemon_capability_inner() {
        let (dir, path, listener) = {
            let _g = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("legacy-unguarded-send.sock");
            let listener = bind_attach_listener(&path).expect("bind legacy daemon");
            (dir, path, listener)
        };
        let submissions = Arc::new(AtomicUsize::new(0));
        let server_submissions = submissions.clone();
        let server = tokio::spawn(async move {
            // PRD #42 M2: `IpcListener::accept()` yields the stream directly.
            while let Ok(Ok(mut stream)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
            {
                let Some((KIND_REQ, payload)) = read_frame(&mut stream)
                    .await
                    .expect("read legacy request frame")
                else {
                    continue;
                };
                let request: serde_json::Value =
                    serde_json::from_slice(&payload).expect("decode legacy request");
                let response = if request.get("op").and_then(|op| op.as_str()) == Some("hello") {
                    // Previous daemon shape: protocol version only, no guarded-send capability.
                    AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION)
                } else {
                    server_submissions.fetch_add(1, Ordering::SeqCst);
                    AttachResponse::with_send_result(SendResult::Applied)
                };
                crate::daemon_protocol::write_resp(&mut stream, &response)
                    .await
                    .expect("write legacy response");
            }
        });
        let client = DaemonClient::new(path);

        let result = client
            .write_and_submit_with_identity(
                "guarded-pane",
                "must not reach an unguarded daemon",
                Some("expected-agent"),
                Some("expected-session"),
                Some("guarded-delivery-015"),
            )
            .await;
        server.await.unwrap();
        drop(dir);

        assert!(
            result.is_err() && submissions.load(Ordering::SeqCst) == 0,
            "guarded send must fail safe before submission when capability is absent; result={result:?}, submissions={}",
            submissions.load(Ordering::SeqCst)
        );
    }

    // -----------------------------------------------------------------------
    // PRD #819 M5: the client-side capability helper.
    //
    // The property under test throughout is that ABSENCE WITHHOLDS. Every
    // "daemon does not advertise" shape below — the field missing, a different
    // set, an empty set — has to reach the same answer, because on the wire they
    // are indistinguishable from each other and from a daemon that will refuse
    // the verb.
    // -----------------------------------------------------------------------

    fn hello_advertising(list: &[&str]) -> AttachResponse {
        AttachResponse {
            capabilities: Some(list.iter().map(|c| c.to_string()).collect()),
            ..AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION)
        }
    }

    #[test]
    fn capabilities_from_an_advertising_daemon_permit_the_advertised_verbs() {
        let caps = DaemonCapabilities::from_hello(
            &AttachResponse::hello(PROTOCOL_VERSION).with_capabilities(),
        );
        assert!(caps.is_advertised());
        for cap in DAEMON_CAPABILITIES {
            assert!(caps.supports(cap), "`{cap}` must be permitted");
            assert!(caps.require(cap).is_ok(), "`{cap}` must not be declined");
        }
    }

    #[test]
    fn capabilities_absent_withhold_every_verb() {
        // The older-daemon case: a `Hello` reply from a build that predates the
        // field. It is NOT distinguishable from a newer daemon that chose to
        // omit it, which is exactly why it must withhold rather than proceed.
        let older = AttachResponse::hello(PROTOCOL_VERSION);
        assert!(older.capabilities.is_none(), "fixture must omit the field");

        for caps in [
            DaemonCapabilities::from_hello(&older),
            DaemonCapabilities::absent(),
            DaemonCapabilities::default(),
        ] {
            assert!(!caps.is_advertised());
            for cap in DAEMON_CAPABILITIES {
                assert!(!caps.supports(cap), "absence must withhold `{cap}`");
                let err = caps
                    .require(cap)
                    .expect_err("absence must decline, never enable");
                let text = err.to_string();
                assert!(
                    text.contains(cap),
                    "the decline must name the capability it withheld: {text}"
                );
            }
        }
    }

    #[test]
    fn capabilities_advertising_a_different_set_withhold_the_rest() {
        // A daemon that knows one verb and not the others. Only the one it named
        // is permitted — the others are as withheld as if it had said nothing.
        let caps = DaemonCapabilities::from_hello(&hello_advertising(&[CAP_LIST_PROJECTS]));
        assert!(caps.is_advertised());
        assert!(caps.supports(CAP_LIST_PROJECTS));
        assert!(!caps.supports(CAP_RESOLVE_PROJECT));
        assert!(!caps.supports(CAP_PREPARE_WORKFLOW));

        // And the degenerate one: advertising an EMPTY set is a real answer
        // ("I support none of these"), and reaches the same withhold as silence.
        let empty = DaemonCapabilities::from_hello(&hello_advertising(&[]));
        assert!(empty.is_advertised(), "an empty set is still an answer");
        for cap in DAEMON_CAPABILITIES {
            assert!(!empty.supports(cap));
        }
    }

    #[test]
    fn unknown_capability_strings_are_ignored_rather_than_rejected() {
        // A NEWER daemon advertising verbs this build has no constant for. The
        // reply must still decode, and the strings this build does know must
        // still be honoured — that is what lets the set grow without a bump.
        let decoded = serde_json::from_value::<AttachResponse>(serde_json::json!({
            "ok": true,
            "server_version": PROTOCOL_VERSION,
            "capabilities": [
                "list-projects",
                "a-verb-from-a-later-build",
                "another-one",
            ],
        }))
        .expect("unknown capability strings must not reject the whole response");

        let caps = DaemonCapabilities::from_hello(&decoded);
        assert!(caps.supports(CAP_LIST_PROJECTS));
        assert!(!caps.supports(CAP_RESOLVE_PROJECT));
        // Carried verbatim, not filtered against this build's own list: the
        // client is not the authority on what verbs exist.
        assert!(caps.supports("a-verb-from-a-later-build"));
    }

    #[test]
    fn cached_capabilities_are_not_reused_across_endpoints() {
        // The cache key rule, exercised directly: a set captured against one
        // socket is never handed out for another. Tested on the free function
        // because a `DaemonClient`'s endpoint cannot be mutated after
        // construction — which is the primary defence, and this is the second.
        let snapshot = CapabilitySnapshot {
            endpoint: PathBuf::from("/tmp/deck-a.sock"),
            generation: DaemonGeneration::default(),
            capabilities: DaemonCapabilities::from_hello(
                &AttachResponse::hello(PROTOCOL_VERSION).with_capabilities(),
            ),
        };

        let same = cached_capabilities_for(Some(&snapshot), Path::new("/tmp/deck-a.sock"))
            .expect("the endpoint it was captured for is a hit");
        assert!(same.supports(CAP_LIST_PROJECTS));

        assert_eq!(
            cached_capabilities_for(Some(&snapshot), Path::new("/tmp/deck-b.sock")),
            None,
            "a snapshot from another daemon must never satisfy this endpoint"
        );
        assert_eq!(
            cached_capabilities_for(None, Path::new("/tmp/deck-a.sock")),
            None
        );
    }

    #[test]
    fn seeding_from_a_hello_reply_records_the_daemon_generation() {
        let client = DaemonClient::new(PathBuf::from("/tmp/deck-generation.sock"));
        assert_eq!(client.cached_capabilities(), None, "starts uncaptured");

        let mut reply = AttachResponse::hello(PROTOCOL_VERSION).with_capabilities();
        reply.build_version = Some("0.39.2-gdeadbee".into());
        let captured = client.store_capabilities_from_hello(&reply);
        assert!(captured.supports(CAP_RESOLVE_PROJECT));
        assert_eq!(client.cached_capabilities(), Some(captured));

        let snapshot = client
            .capabilities
            .lock()
            .unwrap()
            .clone()
            .expect("seeded snapshot");
        assert_eq!(
            snapshot.generation,
            DaemonGeneration {
                server_version: Some(PROTOCOL_VERSION),
                build_version: Some("0.39.2-gdeadbee".into()),
            }
        );

        client.invalidate_capabilities();
        assert_eq!(
            client.cached_capabilities(),
            None,
            "reconnect must drop a set that no longer describes a live connection"
        );
    }

    /// The set is captured ONCE per daemon: N project-aware calls cost one
    /// `Hello`, not N. Drives a synthetic daemon that counts handshakes.
    #[cfg(unix)]
    #[test]
    fn capabilities_are_captured_once_per_handshake_not_once_per_call() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build capability-cache runtime");
        runtime.block_on(capabilities_are_captured_once_per_handshake_not_once_per_call_inner());
    }

    #[cfg(unix)]
    async fn capabilities_are_captured_once_per_handshake_not_once_per_call_inner() {
        let (dir, path, listener) = {
            let _g = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("capability-cache.sock");
            let listener = bind_attach_listener(&path).expect("bind capability daemon");
            (dir, path, listener)
        };
        let handshakes = Arc::new(AtomicUsize::new(0));
        let server_handshakes = handshakes.clone();
        let server = tokio::spawn(async move {
            while let Ok(Ok(mut stream)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
            {
                let Some((KIND_REQ, payload)) = read_frame(&mut stream)
                    .await
                    .expect("read capability request frame")
                else {
                    continue;
                };
                let request: serde_json::Value =
                    serde_json::from_slice(&payload).expect("decode capability request");
                assert_eq!(
                    request.get("op").and_then(|op| op.as_str()),
                    Some("hello"),
                    "the helper must not send anything but a handshake"
                );
                server_handshakes.fetch_add(1, Ordering::SeqCst);
                let response = AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION)
                    .with_capabilities();
                crate::daemon_protocol::write_resp(&mut stream, &response)
                    .await
                    .expect("write capability response");
            }
        });
        let client = DaemonClient::new(path);

        assert!(client.daemon_advertises(CAP_LIST_PROJECTS).await.unwrap());
        client
            .require_capability(CAP_RESOLVE_PROJECT)
            .await
            .expect("advertised verb is permitted");
        assert!(
            client
                .daemon_advertises(CAP_PREPARE_WORKFLOW)
                .await
                .unwrap()
        );
        assert!(
            !client
                .daemon_advertises("a-verb-this-daemon-never-named")
                .await
                .unwrap()
        );
        assert_eq!(
            handshakes.load(Ordering::SeqCst),
            1,
            "four capability questions must cost ONE handshake"
        );

        // A clone shares the capture — it is the same daemon at the same path.
        assert!(
            client
                .clone()
                .daemon_advertises(CAP_LIST_PROJECTS)
                .await
                .unwrap()
        );
        assert_eq!(handshakes.load(Ordering::SeqCst), 1);

        // Reconnect: the daemon behind this path may now be a different build.
        client.invalidate_capabilities();
        assert!(client.daemon_advertises(CAP_LIST_PROJECTS).await.unwrap());
        assert_eq!(
            handshakes.load(Ordering::SeqCst),
            2,
            "invalidation must force a fresh capture"
        );

        drop(client);
        server.await.unwrap();
        drop(dir);
    }

    /// The older-daemon case end to end: a daemon whose `Hello` omits the field
    /// entirely. Every project verb is withheld, and — the part that matters —
    /// the client never sends one, so it never sees the `ok:false` whose only
    /// discriminator is serde's `unknown variant …` text.
    #[cfg(unix)]
    #[test]
    fn an_older_daemon_withholds_every_project_verb() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build older-daemon runtime");
        runtime.block_on(an_older_daemon_withholds_every_project_verb_inner());
    }

    #[cfg(unix)]
    async fn an_older_daemon_withholds_every_project_verb_inner() {
        let (dir, path, listener) = {
            let _g = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("older-daemon.sock");
            let listener = bind_attach_listener(&path).expect("bind older daemon");
            (dir, path, listener)
        };
        let project_verbs = Arc::new(AtomicUsize::new(0));
        let server_project_verbs = project_verbs.clone();
        let server = tokio::spawn(async move {
            while let Ok(Ok(mut stream)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
            {
                let Some((KIND_REQ, payload)) = read_frame(&mut stream)
                    .await
                    .expect("read older-daemon request frame")
                else {
                    continue;
                };
                let request: serde_json::Value =
                    serde_json::from_slice(&payload).expect("decode older-daemon request");
                let response = if request.get("op").and_then(|op| op.as_str()) == Some("hello") {
                    // Pre-PRD-#819 shape: no `capabilities` on the reply.
                    AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION)
                } else {
                    server_project_verbs.fetch_add(1, Ordering::SeqCst);
                    AttachResponse::err("unknown variant `list-projects`")
                };
                crate::daemon_protocol::write_resp(&mut stream, &response)
                    .await
                    .expect("write older-daemon response");
            }
        });
        let client = DaemonClient::new(path);

        let caps = client.capabilities().await.expect("handshake succeeds");
        assert!(!caps.is_advertised());
        for cap in DAEMON_CAPABILITIES {
            assert!(
                client.require_capability(cap).await.is_err(),
                "`{cap}` must be withheld against a daemon that never advertised it"
            );
        }
        assert_eq!(
            project_verbs.load(Ordering::SeqCst),
            0,
            "withholding means the verb is never sent, so its refusal text is never read"
        );

        drop(client);
        server.await.unwrap();
        drop(dir);
    }

    #[test]
    fn sanitize_record_tab_membership_scrubs_display_name() {
        // Issue #833. `display_name` is the card TITLE the TUI actually uses —
        // hydration copies it into `ui.pane_display_names`, from which the
        // dashboard loop fills `ui.display_names`, and `render_card_grid`
        // prefers that map over the session's own name — and it was the one
        // string on this record with no scrub here at all.
        let mut rec = AgentRecord {
            id: "9".into(),
            pane_id_env: None,
            display_name: Some(format!("de\x1bplo\u{202e}yer\0 {}", "\u{3bc}".repeat(400))),
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
            live: None,
            spawned_at_ms: None,
        };
        sanitize_record_tab_membership(&mut rec);
        let name = rec
            .display_name
            .as_deref()
            .expect("a name with printable content is repaired, not dropped");
        assert!(
            !name.chars().any(char::is_control),
            "no control character may survive: {name:?}"
        );
        assert!(
            !name.chars().any(crate::untrusted_text::is_bidi_format_char),
            "no bidi override may survive: {name:?}"
        );
        let body = name.strip_suffix('…').expect("a clamped name is marked");
        assert!(
            body.len() <= crate::agent_pty::DISPLAY_NAME_MAX_LEN,
            "the name must be clamped to the daemon's own ceiling, got {} bytes",
            body.len()
        );
        assert!(
            body.ends_with('\u{3bc}'),
            "the clamp must snap back to a character boundary: {body:?}"
        );

        // Nothing printable left → `None`, so hydration falls back to the
        // agent id rather than titling the card with an empty string.
        let mut blank = rec.clone();
        blank.display_name = Some("\x1b\u{202e}\0\x7f  ".into());
        sanitize_record_tab_membership(&mut blank);
        assert_eq!(blank.display_name, None);

        // And an ordinary name round-trips untouched — the control that keeps
        // the two assertions above from passing on a scrub that eats everything.
        let mut ok = rec.clone();
        ok.display_name = Some("deployer".into());
        sanitize_record_tab_membership(&mut ok);
        assert_eq!(ok.display_name.as_deref(), Some("deployer"));
    }

    #[test]
    fn sanitize_record_tab_membership_strips_invalid_name() {
        // M2.12 fixup auditor #1: the daemon validates `tab_membership`
        // on `StartAgent`, but a malformed or older daemon could echo
        // back a record carrying an invalid `name`. The client-side
        // boundary sanitizer must clamp the membership to `None` so the
        // TUI's bucketing / tracing never sees control bytes — the
        // agent is still real and lands on the dashboard.
        let mut rec = AgentRecord {
            id: "7".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: Some(TabMembership::Mode {
                name: "\x1b[31mevil".into(),
            }),
            agent_type: None,
            rows: 0,
            cols: 0,
            live: None,
            spawned_at_ms: None,
        };
        sanitize_record_tab_membership(&mut rec);
        assert!(rec.tab_membership.is_none(), "invalid name must be cleared");

        // And a valid record round-trips untouched.
        let mut ok = AgentRecord {
            id: "8".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 2,
                role_name: "coder".into(),
                is_start_role: false,
                orchestration_cwd: None,
                display_title: None,
                orchestration_id: None,
            }),
            agent_type: None,
            rows: 0,
            cols: 0,
            live: None,
            spawned_at_ms: None,
        };
        sanitize_record_tab_membership(&mut ok);
        assert_eq!(
            ok.tab_membership,
            Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 2,
                role_name: "coder".into(),
                is_start_role: false,
                orchestration_cwd: None,
                display_title: None,
                orchestration_id: None,
            }),
        );
    }

    // PRD #127 C5 — run-now outcome parsing: the `agents` token distinguishes a
    // started fire from a skipped-still-running one; a stale daemon that omits
    // the token is treated as started.
    #[test]
    fn run_now_outcome_parses_started_vs_skipped() {
        assert_eq!(
            run_now_outcome_from_agents(&Some(vec!["started".to_string()])),
            RunNowOutcome::Started
        );
        assert_eq!(
            run_now_outcome_from_agents(&Some(vec!["skipped".to_string()])),
            RunNowOutcome::SkippedStillRunning
        );
        // Missing token (older daemon) → started.
        assert_eq!(run_now_outcome_from_agents(&None), RunNowOutcome::Started);
        assert_eq!(
            run_now_outcome_from_agents(&Some(vec![])),
            RunNowOutcome::Started
        );
    }
}
