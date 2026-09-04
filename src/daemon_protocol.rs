//! Streaming attach protocol for the daemon (PRD #76, M1.2).
//!
//! # Protocol versioning
//!
//! [`PROTOCOL_VERSION`] is the on-the-wire shape of this module. Bump it when
//! a change would cause an older or newer peer to mis-parse a frame:
//!
//! - **Bump:** new `KIND_*` codes, payload-schema changes that aren't
//!   forward-compatible (renames, type changes, removed fields without a
//!   `#[serde(default)]` shim), new [`AttachRequest`] variants.
//! - **Do NOT bump:** additive optional fields tagged
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]` — those are
//!   forward-compatible by design (older peer ignores the field, newer peer
//!   tolerates its absence).
//!
//! The handshake itself ([`AttachRequest::Hello`]) is enforced by the
//! **desktop** client, which refuses to connect unless the daemon reports
//! exactly this [`PROTOCOL_VERSION`] (`desktop/src-tauri/src/daemon_bridge.rs`,
//! `classify_handshake`) — that check runs before the build-stamp comparison
//! and its session-scoped bypass cannot reach it. No other client refuses on a
//! version *difference*: `72527b9` removed the laptop-side `connect`
//! comparison this note used to name (issue #491 — it compared two constants
//! that never shared a wire), leaving only a presence floor there, and the
//! local TUI attach path never had one (issue #405). Single-binary in-process
//! call sites match versions by construction.
//!
//! # Wire format
//!
//! Length-prefixed binary frames:
//!
//! ```text
//! +-------+--------------------+----------------------+
//! | 1 B   | 4 B (big-endian)   | N bytes              |
//! | kind  | payload length     | payload              |
//! +-------+--------------------+----------------------+
//! ```
//!
//! Justification: PRD line 294 explicitly rules out gRPC / JSON-RPC and
//! "extra build deps". We have `tokio` and `serde_json` already, so control
//! frames carry JSON and stream frames carry raw PTY bytes — no new deps,
//! and the framing is portable to stdio (M2.1). No socket-only assumptions
//! (no fd passing, no `SCM_RIGHTS`).
//!
//! # Frame kinds
//!
//! | Kind            | Direction         | Payload                       |
//! |-----------------|-------------------|-------------------------------|
//! | `KIND_REQ`      | client → server   | JSON [`AttachRequest`]        |
//! | `KIND_RESP`     | server → client   | JSON [`AttachResponse`]       |
//! | `KIND_STREAM_OUT` | server → client | raw PTY bytes                 |
//! | `KIND_STREAM_IN`  | client → server | raw bytes for PTY stdin       |
//! | `KIND_DETACH`     | client → server | empty — detach, leave agent   |
//! | `KIND_STREAM_END` | server → client | optional reason (e.g. lagged) |
//! | `KIND_EVENT`      | server → client | JSON [`crate::event::BroadcastMsg`] (M2.17/M2.19, after a `SubscribeEvents` request) |
//! | `KIND_SHUTDOWN`   | client → server | empty — shut the daemon down (PRD #92 F1) |
//! | `KIND_SHUTDOWN_ACK` | server → client | empty — acknowledges `KIND_SHUTDOWN` before teardown begins (PRD #92 F1 followup) |
//!
//! # Per-connection state machine
//!
//! 1. Client sends a single `KIND_REQ` with one of the [`AttachRequest`]
//!    variants.
//! 2. Server replies with `KIND_RESP` carrying [`AttachResponse`].
//! 3. For non-streaming ops (`list-agents`, `start-agent`, `stop-agent`,
//!    `snapshot`) the server then closes the connection. `snapshot` may
//!    emit one `KIND_STREAM_OUT` frame with the scrollback bytes, followed
//!    by `KIND_STREAM_END` and close.
//! 4. For `attach-stream`, the server immediately follows the OK response
//!    with a single `KIND_STREAM_OUT` carrying the consistent scrollback
//!    snapshot, then enters streaming mode: live PTY bytes flow as
//!    `KIND_STREAM_OUT`, client keystrokes flow as `KIND_STREAM_IN`, and
//!    either side may end via `KIND_DETACH` (client) or `KIND_STREAM_END`
//!    (server, e.g. agent died or subscriber lagged).
//!
//! # Concurrent attach
//!
//! Multiple clients may attach to the same agent. They share a single
//! [`crate::agent_pty::AgentBus`]: each subscriber gets its own broadcast
//! receiver, so PTY output fans out to every attached client. Each client's
//! `KIND_STREAM_IN` is forwarded through a shared writer (under
//! `tokio::sync::Mutex`), so concurrent keystrokes interleave at byte
//! granularity — last writer wins per byte, which matches PRD line 199's
//! "daemon is the single source of truth" model.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::platform::ipc::{IpcListener, IpcStream};

pub use crate::agent_pty::TabMembership;
use crate::agent_pty::{AgentPtyRegistry, AgentRecord, SpawnOptions};
use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, is_valid_pane_id_env};
use crate::event::{AgentType, BroadcastMsg};
use crate::pane_input::escape_bytes_for_log;
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Frame kinds
// ---------------------------------------------------------------------------

pub const KIND_REQ: u8 = 0x01;
pub const KIND_RESP: u8 = 0x02;
pub const KIND_STREAM_OUT: u8 = 0x10;
pub const KIND_STREAM_IN: u8 = 0x11;
pub const KIND_STREAM_END: u8 = 0x12;
pub const KIND_DETACH: u8 = 0x13;
/// PRD #76 M2.17: server → client JSON-encoded `AgentEvent` forwarded over
/// a long-lived `SubscribeEvents` connection. The TUI's remote-mode
/// `AppState` is otherwise disconnected from the daemon's hook ingestion
/// loop; this frame is the bridge.
pub const KIND_EVENT: u8 = 0x14;
/// PRD #92 F1: client → server header-only frame meaning "shut the daemon
/// down now." Triggered by the **Stop** option in the Ctrl+C dialog. The
/// daemon validates the frame is header-only (rejects any non-empty
/// payload — see followup hardening at the handler), sends back a
/// [`KIND_SHUTDOWN_ACK`] **before** beginning teardown, then iterates
/// its agent registry, SIGTERMs each child with a short grace before
/// SIGKILL, then exits. Idempotent on the daemon side
/// (`AgentPtyRegistry::shutdown_all_graceful` guards via a latch).
pub const KIND_SHUTDOWN: u8 = 0x15;
/// PRD #92 F1 followup: server → client header-only frame acknowledging
/// receipt of a well-formed [`KIND_SHUTDOWN`]. Sent **before** the daemon
/// begins teardown so the TUI can distinguish "daemon acknowledged"
/// from "old daemon closed the connection on an unknown frame" — the
/// original F1 wire used socket-close as the implicit ack, which was
/// indistinguishable from the upgrade-mismatch case (a daemon predating
/// `PROTOCOL_VERSION = 2` would close the connection on an unknown
/// frame kind). With the explicit ack the client treats
/// EOF-without-ack, an unrecognised frame, and the 1-second timeout
/// alike as errors, surfaces them via `ui.status_message`, and does
/// not exit the TUI — the user can retry, Detach, or `pkill` from a
/// shell.
pub const KIND_SHUTDOWN_ACK: u8 = 0x16;
/// PRD #20 R20-007 (finding #10): server → client frame emitted on the attach
/// stream when a `KIND_STREAM_IN` key/paste frame is REFUSED because the target
/// became non-live (history-only / view-only), exited, or rebound while the
/// stream stayed open. Carries a short UTF-8 reason (e.g. `history-only`) as its
/// payload. UNLIKE [`KIND_STREAM_END`] it does NOT end the stream — the client
/// stays attached (still sees output) and can surface honest feedback + leave
/// its input mode, and subsequent frames to a still-non-live target are each
/// rejected the same way. Adding this server→client frame changes the attach
/// stream wire shape, so it rides the `PROTOCOL_VERSION` 5 → 6 bump (see below).
pub const KIND_STREAM_REJECT: u8 = 0x17;

/// PRD #76 M2.21: wire-format version for the attach socket. Bump every time
/// the on-the-wire shape changes in a way an older client/daemon would
/// mis-parse — new `KIND_*` codes, payload schema changes, new request
/// variants. PRD #76 has accumulated several silent bumps (M2.17 added
/// `KIND_EVENT`, M2.19 changed its payload to `BroadcastMsg`, earlier
/// milestones added `Resize` / `SetAgentLabel` / `SubscribeEvents`); this
/// constant starts at the first post-M2.19 version so older daemons fail the
/// handshake instead of silently dropping live updates.
///
/// Additive `#[serde(default, skip_serializing_if = "Option::is_none")]`
/// fields do NOT require a bump — they're forward-compatible by design. See
/// the module-level "Protocol versioning" section for the full bump policy.
///
/// PRD #120 bumped 3 → 4: the `KIND_EVENT` payload
/// ([`crate::event::BroadcastMsg`]) gained a new
/// [`crate::event::BroadcastMsg::OrchestrationSurface`] variant (a new `kind`
/// tag) so the daemon can surface a freshly-spawned orchestration tab to
/// already-attached TUIs. An older client receiving the new tag would fail to
/// deserialize the frame, so this is a non-forward-compatible payload-schema
/// change.
///
/// PRD #201 bumped 4 → 5: [`crate::event::AgentType`] gained a wire-serialized
/// `Pi` variant that rides `AgentRecord.agent_type` (the `ListAgents`
/// `KIND_RESP`) and `AgentEvent.agent_type` (the `KIND_EVENT` broadcast). A
/// pre-Pi reader has neither the `Pi` variant NOR (before this PRD) a
/// `#[serde(other)]` catch-all, so `agent_type = "pi"` fails its whole-response
/// / whole-frame decode — a non-forward-compatible payload-schema change, the
/// same class as #120's new enum variant. The bump marks the break so the
/// old-reader/new-daemon pairing is detectable at handshake time rather than
/// arriving as a mid-session deserialize crash; when this was written
/// `crate::connect::probe_remote_protocol` also *refused* on it, which it no
/// longer does — see the enforcement note below. `AgentType`
/// now also carries `#[serde(other)]` so THIS build and every future one
/// degrade an unknown agent type to the neutral `None` placeholder rather than
/// erroring — future agent-type additions therefore need no further bump — but
/// already-released pre-Pi binaries predate that fallback, which is exactly
/// what this version bump guards.
///
/// PRD #20 bumped 5 → 6 (findings #6 + #10 — one coherent Rule 12 decision):
/// finding #10 adds a new server→client attach-stream frame kind
/// ([`KIND_STREAM_REJECT`]) so the daemon can report a typed input rejection
/// instead of silently dropping bytes. A new frame kind changes the stream wire
/// shape (an older client would see an unrecognised kind), so per the task's
/// Rule 12 rule this is a hard bump, not an additive-field change. Finding #6's
/// guarded-send capability rides ALONGSIDE this as an additive, optional field
/// on the `Hello` reply ([`AttachResponse::guarded_send`]) — a NEW client checks
/// that explicit capability (NOT the version number) before an identity-bearing
/// send, and fails safe when it is absent, so the two concerns are decoupled: an
/// old daemon that happens to share a version can never be mistaken for one that
/// enforces the identity/idempotency guards. Documented as a semantic
/// cross-version consideration in `changelog.d/20.breaking.md`; a
/// previous-release-daemon manual test is required at release.
///
/// PRD #370 bumped 6 → 7: [`crate::event::EventType`] gained `ShellBusy` /
/// `ShellIdle` variants (the daemon-synthesized "a foreground shell command
/// is running" signal), following the exact precedent PRD #201 set for
/// `AgentType::Pi` above — a pre-#370 reader has neither variant nor a
/// `#[serde(other)]` catch-all, so a `KIND_EVENT` frame carrying one fails
/// its whole-frame decode. The bump marks the old-reader/new-daemon pairing as
/// skewed at handshake time instead of letting it land as a mid-session crash;
/// the enforcement note below records where that is acted on today.
/// `EventType` now also carries `#[serde(other)]`
/// (mirroring `AgentType`'s retrofit), so future event-type additions need
/// no further bump.
///
/// Issue #717 bumped 7 → 8, for TWO changes that the module's own bump policy
/// above names explicitly and that ship as one decision.
///
/// A new REQUEST variant ([`AttachRequest::DispatchWorktreeClosePreview`]) — the
/// close dialog asking what a confirmed close would leave behind. That break is
/// one-directional and mild: a NEW client asking an OLD daemon gets a decode
/// failure, which the call site already treats as "render no warning".
///
/// And a new `KIND_EVENT` payload variant
/// ([`crate::event::BroadcastMsg::WorktreeKept`]) — the daemon reporting what
/// the close ACTUALLY left behind. This is the harder half, and the same class
/// as PRD #120's `OrchestrationSurface` above: an older peer has no such `kind`
/// tag and fails the whole-frame decode, taking its event subscription down with
/// it rather than skipping one message.
///
/// The second is what makes the bump load-bearing rather than bookkeeping. The
/// constant's job here is to make a skewed pairing *identifiable* at handshake
/// time instead of letting it surface later as a dead event stream. The reply's
/// [`AttachResponse::kept_worktree`] field is additive and would have needed no
/// bump on its own.
///
/// PRD #819 bumped 8 → 9, and **one bump covers every request variant that PRD
/// added**: [`AttachRequest::ListProjects`], [`AttachRequest::ResolveProject`],
/// [`AttachRequest::PrepareWorkflow`] and
/// [`AttachRequest::StartPreparedAgent`]. They are on the bump list for the
/// ordinary reason — an older daemon fails the frame decode on a variant it does
/// not have — and the last of them arrived after the first three, during the
/// same unreleased cycle, which is exactly when a variant is free. The
/// [`AttachResponse::capabilities`] field that goes with them is additive and
/// optional and would have needed no bump of its own; it is what lets a client
/// tell "speaks 9" from "answers this verb" once there is more than one build at
/// 9.
///
/// **Do not read that as licence to keep adding variants at 9.** It holds only
/// while 9 is unreleased: the moment a build carrying it ships, another variant
/// is another break for every user, and this repo's bump policy makes that
/// another minor release (`docs/develop/versioning.md`). The deadline is the
/// whole reason `StartPreparedAgent` was taken now rather than left as the
/// recorded next step it started as.
///
/// # Where this constant is enforced
///
/// **Exactly one call site refuses on it: the desktop.**
/// `classify_handshake` in `desktop/src-tauri/src/daemon_bridge.rs` requires
/// `server_version == Some(PROTOCOL_VERSION)`, runs that comparison *before*
/// the build-stamp one, and is not reachable by the stamp check's
/// session-scoped bypass — so a desktop and a daemon that disagree here never
/// exchange a second frame. Inside this crate nothing refuses on a version
/// *difference* — `probe_remote_protocol`'s surviving check is a presence
/// floor, described two paragraphs down — and that is issue #405. The bump
/// rationales above were written while
/// [`crate::connect::probe_remote_protocol`] compared the remote's
/// `server_version` against the laptop's and hard-failed on a difference, and
/// each one named that refusal as the payoff.
///
/// Issue #491 removed the comparison, because it could not fail for a real
/// reason: `connect` is an `ssh -t` wrapper that runs the *remote* binary's TUI
/// against the *remote* daemon, so the laptop's constant was never a party to
/// that attach conversation and the check could only refuse remotes whose two
/// ends already agreed by construction. The probe still refuses a remote that
/// cannot answer `daemon hello` at all — an install floor, not a version
/// verdict.
///
/// The local same-machine TUI↔daemon pairing is the place a wire-shape skew
/// most easily happens (the binary upgraded on disk under a still-running
/// daemon), and it is guarded by [`crate::build_version_handshake`]'s
/// `DAD_BUILD_ID` comparison rather than by this constant. Build-id equality is
/// strictly stronger than protocol equality when it *matches* — same build
/// implies same protocol — but declining its restart prompt (the right choice
/// when live agents would die with the daemon) attaches anyway with no version
/// check of any kind. Issue #405 tracks closing that. The desktop↔daemon
/// pairing skews the same way and is the one that *does* read this constant,
/// per the paragraph above.
///
/// Keep bumping this on every wire-shape break regardless. The bump is what
/// makes a skew *nameable* — it is the number the handshake reports, what
/// `daemon hello` prints, and the input any future compatibility gate will
/// read; #405 is what will make it *refused*.
pub const PROTOCOL_VERSION: u32 = 9;

/// Hard cap on a single frame's payload length. Defends against a malicious
/// or buggy peer trying to allocate gigabytes off a forged length prefix.
/// 16 MiB is well above any reasonable PTY chunk or scrollback snapshot.
///
/// `pub(crate)` rather than private (issue #478) because the bound belongs to
/// the *protocol*, not to [`read_frame`]: the TUI's synchronous one-shot client
/// (`ui::send_daemon_request_blocking_with_timeout`) decodes the same 5-byte
/// header without the async reader, and used to allocate straight off the u32.
/// Both sides now read the one constant — do not re-spell the 16 MiB literal.
pub(crate) const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// PRD #819: capability strings, and the project verbs' bounded error codes.
// ---------------------------------------------------------------------------

/// Capability string for [`AttachRequest::ListProjects`].
///
/// The strings are the kebab-case `op` names the request enum already
/// serializes to, so there is one spelling of each verb rather than two that
/// can drift.
pub const CAP_LIST_PROJECTS: &str = "list-projects";

/// Capability string for [`AttachRequest::ResolveProject`].
pub const CAP_RESOLVE_PROJECT: &str = "resolve-project";

/// Capability string for [`AttachRequest::PrepareWorkflow`].
pub const CAP_PREPARE_WORKFLOW: &str = "prepare-workflow";

/// Capability string for [`AttachRequest::StartPreparedAgent`].
///
/// Withheld wherever [`CAP_PREPARE_WORKFLOW`] is withheld, and the two are only
/// useful together: a build that cannot prepare a workflow can never have issued
/// a token, so a prepared start on it has nothing to present.
pub const CAP_START_PREPARED_AGENT: &str = "start-prepared-agent";

/// The capability set this build advertises on the [`AttachRequest::Hello`]
/// reply, via [`AttachResponse::with_capabilities`].
///
/// **What an entry claims, precisely:** this build's dispatch knows the op and
/// answers it with a defined [`AttachResponse`], rather than failing the frame
/// decode with serde's `unknown variant …`. It does **not** claim that a given
/// call will succeed — a bounded refusal (a rejected path, an over-long task)
/// is a normal answer to a known verb. Strike a verb from this list if this
/// build stops accepting it; do not leave a name here that the dispatch no
/// longer has an arm for.
///
/// **[`CAP_PREPARE_WORKFLOW`] and [`CAP_START_PREPARED_AGENT`] are Unix-only**,
/// which is the one place that distinction bites. A non-Unix build refuses both
/// verbs *unconditionally* with [`PROJECT_ERR_UNSUPPORTED_PLATFORM`] — the
/// publish cannot deliver the owner-only guarantee there, so no preparation can
/// exist and nothing can be started against one — and that is not a bounded
/// refusal of a particular request but the verb being unavailable, which is
/// exactly the "this build stops accepting it" case above. Advertising either
/// anyway would reduce the list to "the frame decodes", and the whole point of
/// PRD #819's capability helper is that a client withholds rather than enables
/// on absence. The two travel together deliberately: they are one feature split
/// across two round trips, and a build offering the second without the first
/// would be advertising a verb that can only ever answer
/// [`PROJECT_ERR_STALE_TOKEN`].
#[cfg(unix)]
pub const DAEMON_CAPABILITIES: &[&str] = &[
    CAP_LIST_PROJECTS,
    CAP_RESOLVE_PROJECT,
    CAP_PREPARE_WORKFLOW,
    CAP_START_PREPARED_AGENT,
];
#[cfg(not(unix))]
pub const DAEMON_CAPABILITIES: &[&str] = &[CAP_LIST_PROJECTS, CAP_RESOLVE_PROJECT];

/// PRD #819 M2: the project verbs' refusal carries a stable machine-readable
/// code as the first token of [`AttachResponse::error`], followed by `": "` and
/// a generic human sentence.
///
/// The codes exist because the alternative is matching prose, and because the
/// text after them is deliberately uninformative: an arbitrary caller-selected
/// path gets a bounded refusal that names no path, no parser source line and no
/// raw OS error. [`crate::project_config::ProjectConfigError`]'s `Display`
/// renders the offending TOML line verbatim, so returning it for a pasted path
/// would disclose file *content*, not merely existence. Detail is reserved for
/// a path already in the daemon's known set — see the PRD's disclosure split —
/// and that half arrives with M3.
pub const PROJECT_ERR_INVALID_PATH: &str = "invalid-path";

/// The `task` on [`AttachRequest::PrepareWorkflow`] exceeded
/// [`crate::bounded_read::MAX_TASK_BYTES`], or carried a NUL. See
/// [`PROJECT_ERR_INVALID_PATH`] for the code convention.
pub const PROJECT_ERR_TASK_REJECTED: &str = "task-rejected";

/// The verb parsed and its arguments were accepted, but this build has no
/// implementation behind it yet (PRD #819 M2 landed the wire contract; M3 and
/// M4 land the behaviour).
///
/// It is a distinct code rather than a generic failure because the two are not
/// the same thing to a caller, and it is an error rather than `ok: true` with
/// an empty payload for the same reason: a client — or a test — cannot tell an
/// empty answer apart from a real one. A panic was the other option and is
/// worse; `handle_connection` serves other clients.
///
/// **No arm of this build's dispatch returns it any more.** M3 landed
/// `list-projects` and `resolve-project`, M4 landed `prepare-workflow`, and the
/// helper that produced this refusal went with the last of them. The constant
/// stays because it is the thing the behaviour tests assert an answer is *not*:
/// a refusal that comes from resolving a project and a refusal that comes from
/// there being no implementation are indistinguishable to a caller that cannot
/// name the difference, and that is exactly how an implementation gets quietly
/// reverted. Keep it, and give it back to any verb this file grows a stub for.
pub const PROJECT_ERR_UNIMPLEMENTED: &str = "unimplemented";

/// PRD #819 M4: the caller's [`AttachRequest::PrepareWorkflow::config_revision`]
/// does not match the config the daemon just read.
///
/// The client resolved against one snapshot and is asking to launch against
/// another. Refusing is what closes the TOCTOU window between the picker, the
/// write and the spawn; the remedy is to resolve again, which is what the
/// sentence says.
pub const PROJECT_ERR_STALE_REVISION: &str = "stale-revision";

/// PRD #819 M4: the project resolved, but defines no role-bearing orchestration
/// under the requested name.
///
/// A separate code from [`PROJECT_ERR_UNRESOLVED`] because it is a different
/// fact and a different remedy — the project is fine and the *name* is wrong —
/// and because it is only reachable once the path has already resolved, so it
/// discloses nothing that refusal was protecting. The sentence still names no
/// available orchestration: that is config content for a path the caller may
/// merely have pasted.
pub const PROJECT_ERR_NO_ORCHESTRATION: &str = "no-such-orchestration";

/// PRD #819 M4: the project and the orchestration resolved, but the coordinator
/// context could not be published.
///
/// See [`crate::orchestrator_context::ContextPublishError::client_sentence`] for
/// what the text after it is allowed to say and why it is allowed to be more
/// specific than [`crate::project_resolve::generic_refusal`].
pub const PROJECT_ERR_PUBLISH_FAILED: &str = "publish-failed";

/// PRD #819 M4: an [`AttachRequest::StartPreparedAgent`] presented a `prep_token`
/// this daemon did not issue, or issued longer ago than
/// [`crate::prep_token::PREP_TOKEN_TTL`].
///
/// One code for both, because they are one answer: the token does not identify
/// a live preparation. Read [`crate::prep_token`]'s module doc before treating
/// this as an authorization failure — it is not one. There is no "absent token"
/// case to except any more: the token is a required field of the verb, and a
/// request without one does not decode.
pub const PROJECT_ERR_STALE_TOKEN: &str = "stale-token";

/// PRD #819 audit fix: an [`AttachRequest::StartPreparedAgent`] presented a
/// `prep_token` this daemon **did** issue and which has **not** expired, but the
/// state that preparation approved has moved — see
/// [`crate::project_resolve::revalidate_preparation`] for the five checks.
///
/// A distinct code from [`PROJECT_ERR_STALE_TOKEN`] because it is a distinct
/// fact: the token is live and the *world* changed, rather than the token being
/// unknown or aged out. The remedy is the same (prepare again), which is why the
/// sentence after each code reads alike, but a client or a test that cannot tell
/// "I presented garbage" from "another launch replaced my artifact" cannot
/// diagnose either.
///
/// One code for all five checks, and the sentence names none of them. Read
/// [`crate::prep_token`]'s module doc before treating this as an authorization
/// failure — it is a staleness and integrity refusal.
pub const PROJECT_ERR_STALE_PREPARATION: &str = "stale-preparation";

/// PRD #819 Greptile P1(a): an [`AttachRequest::StartPreparedAgent`] presented a
/// token this daemon issued, which has **not** expired and whose approved state
/// is still intact — but the request being made is not the one that preparation
/// approved.
///
/// **This is a different fact from [`PROJECT_ERR_STALE_PREPARATION`], and until
/// this code existed nothing checked it at all.** The audit fix made the record
/// carry what a preparation approved and made the spawn re-validate it against
/// the filesystem; what it did not do is compare the *submitted* spawn fields
/// against that record. So a caller could present a token prepared for project X
/// while submitting the `cwd`, orchestration and role of project Y, and the
/// daemon validated X and started Y. Making the code look validated is what made
/// that easy to miss.
///
/// `stale-preparation` means "your preparation was ours and live, but the world
/// moved"; this means "your request does not match your preparation". Those send
/// an operator in different directions — one is a race with another launch and
/// is fixed by preparing again, the other is a client that is not sending back
/// what it was handed and preparing again will not help — which is the same
/// reasoning that already separates [`PROJECT_ERR_STALE_TOKEN`] from
/// [`PROJECT_ERR_STALE_PREPARATION`].
///
/// See [`crate::project_resolve::verify_prepared_start`] for exactly which
/// fields are bound and, just as load-bearing, which are deliberately not — the
/// `command` is not, because per-launch command override is an existing,
/// documented feature.
pub const PROJECT_ERR_PREPARATION_MISMATCH: &str = "preparation-mismatch";

/// PRD #819 audit follow-up: an [`AttachRequest::StartAgent`] payload carried a
/// `prep_token` key, which that verb does not enforce.
///
/// The remedy is the verb, not the value: send
/// [`AttachRequest::StartPreparedAgent`]. It is a refusal rather than a shrug
/// because the alternative is the exact failure this code's sibling verb exists
/// to remove — serde drops unknown keys on `StartAgent`, so ignoring the token
/// would start the role *unenforced* and report success, and the caller would
/// have no way to tell that from a preparation that was honoured.
///
/// No production caller reaches it: the TUI, the desktop and dispatch all spawn
/// without a token, and the one client method that presents one
/// ([`crate::daemon_client::DaemonClient::start_agent_with_prep_token`]) sends
/// the prepared verb. The wire tests build the payload deliberately, which is
/// the point of the code existing.
pub const PROJECT_ERR_WRONG_START_VERB: &str = "wrong-start-verb";

/// PRD #819 audit fix: [`AttachRequest::PrepareWorkflow`] is refused on this
/// platform because the publish cannot deliver the owner-only guarantee it
/// documents.
///
/// **The premise this replaces was false.** The publish's mode bits,
/// `O_NOFOLLOW | O_DIRECTORY` open and group/other-write refusal are all Unix
/// (`crate::orchestrator_context::open_context_dir`), and the module excused
/// that by asserting the daemon is Unix-only. It is not — [`bind_attach_listener`]
/// returns a `crate::platform::ipc::IpcListener`, which on Windows is an active
/// **named pipe** listener. So a Windows daemon really can be asked to create a
/// directory and write a file at a path a *peer* named, with no protected DACL
/// applied and with path lookups rather than a reparse-safe handle.
///
/// Two honest options existed: implement the DACL half with the helpers in
/// `crate::platform::fsperm` (they exist —
/// `create_owner_only_dir` / `set_file_owner_only` / `ensure_owner_only_dir`),
/// or refuse the verb where the guarantee cannot be provided and narrow the
/// documentation to match. The second was taken: Windows desktop is out of PRD
/// #819's scope, so refusing there is consistent with what shipped, it is far
/// smaller than a correct DACL implementation, and it turns a false claim into a
/// true one. Enabling the verb on Windows later means building the DACL path
/// **and** deleting this code — not deleting this code alone.
///
/// The verb is also struck from [`DAEMON_CAPABILITIES`] on such a platform, so a
/// client learns at the handshake rather than at launch time.
pub const PROJECT_ERR_UNSUPPORTED_PLATFORM: &str = "unsupported-platform";

/// PRD #819 M3: the ONE code carried by every refusal that comes from
/// *resolving* a path against a filesystem.
///
/// It is deliberately a single code for every cause — no such directory, no
/// config there, a config that is a FIFO, a config that does not parse — because
/// the property this verb delivers is that **the wire response does not directly
/// distinguish** those cases for an arbitrary caller-supplied path. A code per
/// cause would hand that distinction back on a plate.
///
/// It is not the only code these verbs can return, and the difference matters:
/// a path that fails [`validate_project_path`]'s string check answers with
/// [`PROJECT_ERR_INVALID_PATH`] instead, before any filesystem is touched. That
/// refusal is about the caller's own request being malformed rather than about
/// what is on disk, so it discloses nothing this one is protecting.
///
/// Read the claim narrowly, and do not widen it in a comment later:
/// canonicalisation, traversal, `open(2)` and TOML parsing do observably
/// different amounts of work, so **no timing property is claimed** and the
/// concurrency bound in [`crate::project_resolve`] protects availability rather
/// than constant time. What is claimed is about the response bytes alone.
///
/// The text after the code splits by trust: a path the daemon already knows
/// carries the detailed diagnostic (`crate::project_resolve::known_path_refusal`),
/// and every other path carries one fixed generic sentence
/// (`crate::project_resolve::generic_refusal`) that names no path, no parser
/// source line and no raw OS error.
pub const PROJECT_ERR_UNRESOLVED: &str = "unresolved";

/// Bounded timeout for a single STREAM_OUT/STREAM_END write to a client. If
/// a client stops draining its socket, the OS send buffer fills and our
/// `write_all` blocks forever — which would also block lag detection (we
/// can't drain the broadcast receiver). With a per-write timeout, a wedged
/// client is dropped within this many seconds instead of pinning the output
/// task. 5s is a generous upper bound for "client can't accept a frame";
/// the client can reattach and replay scrollback.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Issue #717: how long the close-confirmation preview
/// ([`AttachRequest::DispatchWorktreeClosePreview`]) may spend on its
/// `git status --porcelain` before answering conditionally.
///
/// This probe is the SAME one `remove_worktree` runs, but on a very different
/// clock: there it runs detached after the pane is already gone and may take as
/// long as it needs, while here a human is holding `Ctrl+W` and waiting for a
/// dialog. So it gets a deadline, and blowing it degrades the wording rather
/// than dropping the warning (see
/// [`crate::issue_dispatch_run::kept_worktree_preview`]).
///
/// 300 ms against a measured 0–40 ms for `git status --porcelain` on this
/// repo's own worktree is two orders of magnitude of headroom for a cold page
/// cache, while keeping the worst-case keystroke stall under the client's own
/// 500 ms budget — which must be the larger of the two, or the client would
/// always give up first and the deadline here would never be the one that
/// mattered.
const CLOSE_PREVIEW_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Wire I/O
// ---------------------------------------------------------------------------

/// Read a single frame. Returns `Ok(None)` on clean EOF before any header
/// bytes have been read (peer closed the connection cleanly between frames).
/// EOF *after* one or more header bytes is a truncated frame and returns
/// `Err(UnexpectedEof)` — the peer closed mid-header. Likewise EOF inside
/// the payload returns an error via `read_exact`.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    let mut filled = 0usize;
    while filled < header.len() {
        let n = r.read(&mut header[filled..]).await?;
        if n == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("truncated frame header: {filled}/5 bytes before EOF"),
            ));
        }
        filled += n;
    }
    let kind = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds {MAX_FRAME_LEN}"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok(Some((kind, payload)))
}

/// Try to write a single frame within `CLIENT_WRITE_TIMEOUT`. Returns
/// `true` on success and `false` if the write timed out or errored — the
/// caller should treat both as "client gone" and bail out.
async fn write_or_timeout<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, payload: &[u8]) -> bool {
    matches!(
        tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write_frame(w, kind, payload)).await,
        Ok(Ok(()))
    )
}

/// Write a single frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame length {} exceeds {MAX_FRAME_LEN}", payload.len()),
        ));
    }
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum AttachRequest {
    ListAgents,
    /// Spawn an agent process attached to a PTY.
    ///
    /// **Trust boundary.** The attach socket is bound at mode `0o600` and
    /// only accepts connections from the same OS user as the daemon, so
    /// any peer reaching this request can already exec arbitrary code as
    /// that user. We deliberately do **not** sandbox `command`, `cwd`, or
    /// `env`: there is no allowlist, no policy layer, no shell-quoting
    /// validation. Adding any of those here would be security theater —
    /// the same user has equivalent local-exec capability via `sh -c`,
    /// and the daemon's job is to expose PTY plumbing, not to be a
    /// privilege boundary. Multi-tenant or remote scenarios must be
    /// handled at a different layer (separate UID, container, SSH).
    StartAgent {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default = "default_rows")]
        rows: u16,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default)]
        env: Vec<(String, String)>,
        /// M2.11: human-readable label captured into the daemon's per-agent
        /// state. `skip_serializing_if` keeps the on-the-wire shape
        /// backwards-compatible with daemons predating this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        /// M2.12: which tab (mode / orchestration) the spawning UI placed
        /// this agent pane in. Stored on the daemon-side registry and
        /// echoed back via `list_agents` so the TUI can rebuild tab
        /// structure on reconnect. `None` = dashboard pane. Same
        /// `skip_serializing_if` pattern as `display_name` for forward
        /// compat with daemons that don't know about this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_membership: Option<TabMembership>,
        /// M2.13: which AI agent the spawn command runs (inferred at the
        /// TUI spawn site via `AgentType::from_command`). Stored on the
        /// daemon-side registry and echoed back via `list_agents` so a
        /// remote reconnect can build placeholder sessions with the
        /// correct agent_type instead of "No agent". Same
        /// `skip_serializing_if` pattern as the other M2.x fields for
        /// forward compat with daemons that don't know about it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_type: Option<AgentType>,
        /// PRD #201 native prompt delivery: a seed/prompt the daemon stashes
        /// for this pane at spawn time (`AgentPtyRegistry::set_pending_seed`),
        /// pulled NATIVELY by the pane's extension via `dot-agent-deck get-seed`
        /// (→ `pi.sendUserMessage`) instead of the daemon typing it into the
        /// PTY. Set only for a Pi start-role (orchestrator) pane. Same
        /// `skip_serializing_if` pattern as the M2.x fields above, so adding it
        /// is forward + backward compatible and needs no `PROTOCOL_VERSION`
        /// bump — an older daemon simply ignores the field and drives the
        /// unchanged PTY-injection path (the fallback still delivers).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed: Option<String>,
    },
    StopAgent {
        id: String,
    },
    AttachStream {
        id: String,
    },
    Snapshot {
        id: String,
    },
    /// Propagate a TUI-side pane resize to the daemon's PTY. The daemon
    /// ioctls `TIOCSWINSZ` on the master, which the kernel mirrors to the
    /// slave and SIGWINCH's the foreground process. Without this op,
    /// stream-backed panes show width/height mismatches versus the local
    /// vt100 view (see PRD #76, M2.10).
    Resize {
        id: String,
        rows: u16,
        cols: u16,
    },
    /// M2.11: update the daemon-side display_name and cwd for an agent.
    /// Either field may be `None` to clear it. Used by the TUI's rename
    /// flow so renamed panes survive an ssh drop without a separate file
    /// on disk — the daemon's per-agent state is the source of truth.
    SetAgentLabel {
        id: String,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// PRD #100: atomic write-and-submit RPC. Routes the client's
    /// `pane_id` + `text` straight to
    /// [`crate::agent_pty::AgentPtyRegistry::write_to_pane_and_submit`]
    /// on the daemon side, which holds the per-agent writer mutex across
    /// the full `payload → SUBMIT_DELAY → CR` sequence (PRD #93 round-8
    /// atomic contract). Lets a TUI client trigger the same atomic
    /// byte stream the daemon-initiated orchestration-delegate path
    /// already produces — without the two-`STREAM_IN`-frames-with-150ms-gap
    /// pattern, whose mid-sequence mutex release lets a concurrent
    /// daemon-initiated write interleave and fuse a daemon-side CR onto
    /// the user's payload, submitting it prematurely. The user's
    /// trailing CR then lands in an empty input box and is rendered as
    /// a newline — PRD #100's "Enter inserted a newline instead of
    /// submitting" symptom.
    WriteAndSubmit {
        pane_id: String,
        text: String,
    },
    /// PRD #76 M2.17: long-lived subscription to the daemon's
    /// `AgentEvent` broadcast. Server replies with an OK `RESP` then
    /// streams `KIND_EVENT` frames (one per hook event) until either side
    /// closes the connection or the broadcast receiver lags. The TUI in
    /// remote mode opens exactly one of these on startup so its
    /// `AppState` mirrors the daemon's view of live agent activity (agent
    /// type, tool counts, prompts, last-activity timestamps).
    SubscribeEvents,
    /// PRD #76 M2.21: protocol-version handshake. Client sends its
    /// [`PROTOCOL_VERSION`]; server replies with its own in
    /// [`AttachResponse::server_version`]. The daemon never rejects on
    /// `client_version`. On the client side, the **desktop** rejects on
    /// `server_version` — `classify_handshake` in
    /// `desktop/src-tauri/src/daemon_bridge.rs` requires exact equality and is
    /// never bypassed. Issue #491 removed `connect`'s comparison and the local
    /// TUI attach path never had one (issue #405), so the desktop is currently
    /// the only client that refuses on a version *difference*. See the
    /// enforcement note on [`PROTOCOL_VERSION`].
    ///
    /// PRD #103 M1.2: optional `client_build_version` carries the client's
    /// compiled-in `DAD_BUILD_ID`. The daemon logs it but never rejects on
    /// it — mirroring the server-policy on `client_version`. Older clients
    /// omit the field; deserialization tolerates that via
    /// `#[serde(default)]`.
    Hello {
        client_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_build_version: Option<String>,
    },
    /// PRD #127 M1.3: re-read the global `schedules.toml` and diff/replace the
    /// daemon's registered scheduled-task set without a restart. The handler
    /// replies `ok = true` with the names of the now-registered ENABLED tasks
    /// in [`AttachResponse::agents`]. The CLI's mutating subcommands send this
    /// after an atomic write so a running daemon picks the change up live.
    ReloadSchedules,
    /// PRD #127 M1.5: fire a registered scheduled task's callback immediately
    /// (the `schedule run-now` CLI door). Replies `ok = true` if the run
    /// started or was skipped (prior run still active), `ok = false` if no
    /// such task is registered.
    RunNow {
        name: String,
    },
    /// Issue #717: would closing these panes leave a dispatched worktree behind?
    ///
    /// Asked by the TUI while it arms PRD #241's close-confirmation dialog, so
    /// the dialog can warn BEFORE the destructive keystroke that uncommitted
    /// work is about to be kept — and name the path it will be kept at. The
    /// answer rides back on [`AttachResponse::kept_worktree`].
    ///
    /// The daemon is the only process that can answer it. The removal POLICY
    /// lives in its in-memory `WorktreeRegistry` and nowhere on disk, and in
    /// remote mode the worktree is not on the client's filesystem at all — so a
    /// client-side `git status` would be both under-informed and, remotely,
    /// aimed at the wrong machine.
    ///
    /// `pane_ids` is every pane the confirmed close would tear down (one for a
    /// dashboard card, all of them for a Mode/Orchestration tab), because a
    /// multi-role orchestration shares ONE worktree across its role panes and
    /// any of them resolves it. The reply is best-effort: the caller renders no
    /// warning on any error, which is the same way it treats a down daemon.
    DispatchWorktreeClosePreview {
        pane_ids: Vec<String>,
    },
    /// PRD #819 M2: enumerate the projects this daemon knows about. **Read-only.**
    ///
    /// A GUI cannot `cd`, and against a remote daemon it cannot browse to find
    /// out either — so this is the desktop's equivalent of the TUI's selection
    /// mechanism. The answer is derived from what the daemon already holds
    /// (its own startup cwd, live agent cwds, orchestration cwds), revalidated
    /// per candidate; nothing is persisted on either side. The reply rides back
    /// on [`AttachResponse::projects`].
    ///
    /// A struct variant with no fields rather than a unit variant, because the
    /// enumeration will grow bounds (a cap, a filter) and a unit variant cannot
    /// gain a `#[serde(default)]` field without moving the wire shape again.
    ListProjects {},
    /// PRD #819 M2: resolve one project path. **Read-only.**
    ///
    /// One path in, resolved. No directory walk, no children, no parents, and
    /// no implicit widening — resolving `/a/b` does not make `/a` or `/a/b/c`
    /// known. This is the primitive the desktop lacks, and it is deliberately
    /// narrower than a filesystem API: PRD #76's rejected Phase 6 was
    /// `ListDir` / `ReadFile` / `Stat` and this is not that.
    ///
    /// It is **API minimisation, not authorization.** Any peer that reaches
    /// this socket already has the daemon user's local-exec authority via
    /// [`AttachRequest::StartAgent`] — see its trust-boundary note. Withholding
    /// a browse verb limits the blast radius of a compromised or buggy UI and
    /// keeps least privilege available later; it is not a privilege boundary.
    ///
    /// The reply rides back on [`AttachResponse::project`].
    ResolveProject {
        /// An absolute path. Either a path this daemon returned from
        /// [`AttachRequest::ListProjects`], or one a user supplied verbatim —
        /// never one a client derived from its own environment, because a
        /// desktop client's filesystem need not be the daemon's.
        path: String,
    },
    /// PRD #819 M2/M4: prepare a workflow launch — resolve, compose the
    /// coordinator context, and publish it. **The only new verb that writes.**
    ///
    /// Preparing the context is an explicit launch phase rather than an
    /// incidental side effect of resolution: enumerate and resolve stay
    /// read-only, and there is otherwise no operation at which the daemon-side
    /// write could happen ([`AttachRequest::StartAgent`] carries no task and no
    /// config revision, and is issued once per role). A failed preparation
    /// starts no roles.
    ///
    /// The reply rides back on [`AttachResponse::workflow_prepared`].
    PrepareWorkflow {
        /// The daemon-canonical path, as returned by
        /// [`AttachRequest::ListProjects`] or [`AttachRequest::ResolveProject`].
        /// Not a client-derived path.
        path: String,
        orchestration: String,
        /// The coordinator task. Bounded server-side at
        /// [`crate::bounded_read::MAX_TASK_BYTES`] before any filesystem work —
        /// the desktop's own 64 KiB check is a UI affordance and not a bound
        /// this daemon may rely on.
        task: String,
        /// The config revision the client believes it resolved against, as
        /// [`crate::event::ResolvedProject::config_revision`] handed it back.
        /// Stale values are refused with [`PROJECT_ERR_STALE_REVISION`], which
        /// is what closes the TOCTOU window between the picker, the write and
        /// the spawn.
        ///
        /// `#[serde(default)]`, and **absent means "no expectation" rather than
        /// "any revision"** — a client that has not resolved yet must still be
        /// able to launch, which is what keeps the field additive. It is not a
        /// degrade-to-unauthorized of the #608 kind: there is no authorization
        /// here to degrade from, only a staleness check the caller can decline
        /// to make.
        #[serde(default)]
        config_revision: Option<String>,
    },
    /// PRD #819 audit follow-up: start ONE role of a workflow this daemon
    /// prepared. [`AttachRequest::StartAgent`] plus a **required** `prep_token`.
    ///
    /// # Why a verb and not a field
    ///
    /// The token used to ride on `StartAgent` as an additive JSON key, and that
    /// shape **fails open on the wire**. `StartAgent` is a stable op every
    /// daemon back to PRD #76 accepts; an older one decodes the base variant,
    /// ignores the unknown key and starts the role with no preparation
    /// enforcement at all. Nothing on this protocol could catch that from the
    /// client side either: `DaemonClient::connect` is a bare socket connect,
    /// `issue_json_command` writes one request frame, and [`handle_connection`]
    /// decodes exactly one `KIND_REQ` per connection — so a role-start
    /// connection exchanges no [`AttachRequest::Hello`] and re-checks no
    /// [`PROTOCOL_VERSION`]. Verifying the peer on a *previous* connection
    /// narrows the window to the gap between two `connect()` calls; it cannot
    /// shut it.
    ///
    /// A distinct `op` removes the timing argument entirely. A daemon that does
    /// not know this variant fails the frame decode and answers the structured
    /// `malformed request: unknown variant …` refusal from [`handle_connection`]
    /// — `ok: false`, nothing spawned, on the very connection the spawn would
    /// have used. **The launch fails closed with no window at all.**
    ///
    /// # What it does NOT change
    ///
    /// This is not an authorization boundary and the token is not an
    /// authorization token — read [`crate::prep_token`]'s module doc. Any peer
    /// on this socket already holds the daemon user's local-exec authority
    /// through `StartAgent`, which takes arbitrary `command`, `cwd` and `env`; a
    /// peer that wants to spawn something arbitrary calls that and is not
    /// slowed down here. What the verb protects is a *coordinator* against
    /// launching on a preparation some other launch replaced — a staleness and
    /// integrity property, and the same one the token always carried.
    ///
    /// The fields after `prep_token` are `StartAgent`'s, spelled out rather than
    /// nested so the wire shape of a role start is one flat object either way.
    /// Every one carries the same `serde` attribute it does there, so the two
    /// cannot drift in their defaults.
    StartPreparedAgent {
        /// The token [`AttachRequest::PrepareWorkflow`] handed back
        /// ([`crate::event::PreparedWorkflow::token`]).
        ///
        /// **Required, and that is the entire point.** A `#[serde(default)]`
        /// here would let a client omit it and be served anyway, which is the
        /// silent downgrade the verb exists to remove; an absent or wrongly
        /// typed value fails the decode and takes the same structured
        /// malformed-request path an older daemon takes.
        prep_token: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default = "default_rows")]
        rows: u16,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_membership: Option<TabMembership>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_type: Option<AgentType>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed: Option<String>,
    },
}

fn default_rows() -> u16 {
    24
}
fn default_cols() -> u16 {
    80
}

/// PRD #20 R20-003/R20-004: additive identity + idempotency fields that ride
/// alongside a [`AttachRequest::WriteAndSubmit`] request as extra JSON keys.
///
/// They are deliberately NOT declared on the `WriteAndSubmit` enum variant: its
/// `{ pane_id, text }` shape is constructed as a 2-field literal at existing
/// call sites (older tests, the Codex-wrapper path), and widening the variant
/// would break those literals. Instead the handler re-parses the SAME request
/// payload into this struct — every field `#[serde(default)]`, so an older
/// client that omits them still decodes to all-`None`. Unknown keys on the base
/// variant are ignored by serde, so the two parses of one payload never conflict.
///
/// Issue #608: decoding cleanly to all-`None` is no longer the same thing as
/// being AUTHORIZED. `expected_agent_id` is REQUIRED for delivery on the paned
/// arm as well as the pane-less one — absent, the daemon answers
/// `no-live-target` and writes nothing. The wire is untouched; what changed is
/// that an absent identity now means "refuse", not "authorize by pane id alone".
#[derive(Debug, Default, Deserialize)]
struct WriteAndSubmitExtras {
    /// The registry id of the agent the prompt was queued for. On a mismatch
    /// with the live target that currently owns the pane, the daemon returns
    /// `wrong-session` WITHOUT writing (R20-003).
    ///
    /// Issue #608: REQUIRED. An absent id is not a licence to authorize by pane
    /// id alone — it resolves to `Writable::None` → `no-live-target` before any
    /// write is attempted, on the paned and the pane-less arm alike.
    #[serde(default)]
    expected_agent_id: Option<String>,
    /// The session id the prompt was queued for. If a DIFFERENT live session now
    /// owns the pane, the daemon returns `stale` WITHOUT writing (R20-003).
    ///
    /// Issue #608: absent is allowed, but no longer unconditionally. On a paned
    /// target that already carries a current hook session — attached or not —
    /// declining to name that generation IS the mismatch — the daemon knows a
    /// conversation the caller did not name — and the answer is `stale`. An
    /// agent that never emits a session generation carries neither side of the
    /// comparison and still delivers; that carve-out cannot tell such an agent
    /// apart from a conversation that has just ENDED, which is a known hole
    /// documented at the arm that implements it.
    #[serde(default)]
    expected_session_id: Option<String>,
    /// A stable idempotency key. The daemon caches the first result for a
    /// delivery id and replays it on a retry, so a re-sent ambiguous transport
    /// failure never double-submits (R20-004).
    #[serde(default)]
    delivery_id: Option<String>,
}

/// PRD #819 audit follow-up: the detector that keeps the *removed* token-on-
/// `StartAgent` path from coming back as a silent one.
///
/// M4 landed the preparation token as an additive `prep_token` key alongside
/// [`AttachRequest::StartAgent`], re-parsed from the same payload the way
/// [`WriteAndSubmitExtras`] still is. That shape fails open against an older
/// peer — see [`AttachRequest::StartPreparedAgent`], which replaced it — and it
/// never shipped: `PROTOCOL_VERSION` was already 9 and unreleased when it was
/// removed, so no deployed daemon or client ever spoke it.
///
/// Removing the field would have made `start-agent` *ignore* a `prep_token`,
/// because serde drops unknown keys on the base variant — turning a caller
/// aiming at the wrong verb into an unenforced spawn, which is precisely the
/// failure being removed. So the re-parse stays, purely to notice the key and
/// refuse: a token belongs on `start-prepared-agent`, and this verb says so
/// rather than starting the role.
///
/// The field is `serde_json::Value` rather than `String` on purpose. Presence is
/// the whole question — a wrongly typed token is still a caller reaching for the
/// prepared path — and `Option<Value>` accepts any JSON, so this parse cannot
/// fail on a payload that already decoded as `StartAgent` and there is no
/// malformed branch to get wrong. JSON `null` deserializes to `None`, which is
/// the same as absent and is the right reading of it.
///
/// **An absent key still changes nothing.** The TUI, the desktop, dispatch and
/// every existing test call `start-agent` without one, and none of them may
/// break.
#[derive(Debug, Default, Deserialize)]
struct StartAgentExtras {
    /// Present iff the caller put a `prep_token` key on a plain `start-agent`.
    #[serde(default)]
    prep_token: Option<serde_json::Value>,
}

/// PRD #161 M1.1: a snapshot of the agents the daemon is currently managing,
/// carried additively on the [`AttachResponse`] reply to an
/// [`AttachRequest::Hello`]. The shared TUI↔daemon restart prompt (Part A)
/// and the remote `connect` nudge (Part B) both need to state
/// "N running agents: alpha, beta" *before* recycling the daemon, so the
/// handshake reply carries both the `count` and the human-readable `names`.
///
/// Additive + optional on the wire: an older daemon omits the enclosing
/// `running_agents` field entirely (it deserializes to `None` via
/// `#[serde(default)]`), and an older client simply ignores it. This needs
/// no `PROTOCOL_VERSION` bump. `count` and `names` are kept as separate
/// fields (rather than relying on `names.len()`) so a future option B
/// classification can advertise a count without enumerating names if it ever
/// wants to — keeping the shape forward-compatible (PRD #161 D2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningAgentsSummary {
    /// Number of agents the daemon is currently managing.
    pub count: usize,
    /// Display names of those agents, in registry order. Each entry is the
    /// agent's `display_name` when set, falling back to its id, so the prompt
    /// always has a label to show. `#[serde(default)]` lets a payload that
    /// carried only a count decode the names as an empty `Vec`.
    #[serde(default)]
    pub names: Vec<String>,
}

impl RunningAgentsSummary {
    /// Build a summary from the daemon's live [`AgentRecord`]s. The label for
    /// each agent is its `display_name` when present, otherwise its id, so the
    /// restart prompt / connect nudge always has something to print.
    pub fn from_records(records: &[AgentRecord]) -> Self {
        let names = records
            .iter()
            .map(|r| r.display_name.clone().unwrap_or_else(|| r.id.clone()))
            .collect::<Vec<_>>();
        Self {
            count: records.len(),
            names,
        }
    }
}

/// Discriminated by the populated optional fields rather than a tag, since
/// each request type has a fixed shape and clients can decide what to read
/// based on which request they sent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Legacy listing field: just the ids. Always populated by current
    /// daemons so older clients (which only know about `agents`) keep
    /// working. New clients prefer `agent_records` when present so they
    /// also get the captured `DOT_AGENT_DECK_PANE_ID` per agent — see the
    /// M2.x rehydration path in `embedded_pane::hydrate_from_daemon`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    /// Additive companion to `agents`, carrying each agent's spawn-time
    /// `DOT_AGENT_DECK_PANE_ID`. Older daemons omit this field; newer
    /// clients fall back to `agents` when it's `None` so a stale daemon
    /// is forward-compatible (panes hydrate with freshly-allocated ids
    /// instead of preserved ones).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_records: Option<Vec<AgentRecord>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// PRD #76 M2.21: server's [`PROTOCOL_VERSION`], populated in response to
    /// a [`AttachRequest::Hello`] request. Optional so the field is omitted
    /// on unrelated responses and absent on the wire from pre-M2.21 daemons
    /// (in which case the client treats `None` as "incompatible").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<u32>,
    /// PRD #103 M1.1: daemon's compiled-in `env!("DAD_BUILD_ID")` — a
    /// finer-grained identifier than [`PROTOCOL_VERSION`] (it includes the
    /// commit hash and dirty marker) used by the laptop to detect
    /// same-tag-different-commit handler-code skew the protocol version
    /// can't catch. Optional so the field is omitted on unrelated responses
    /// and absent from pre-PRD-103 daemons (the client treats `None` as
    /// "incompatible — recycle the daemon").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_version: Option<String>,
    /// PRD #161 M1.1: snapshot of the agents the daemon is managing at
    /// handshake time (count + display names). Additive + optional — a
    /// pre-PRD-161 daemon omits it (deserializes to `None`), so the field is
    /// forward-compatible and needs no `PROTOCOL_VERSION` bump. The Part-A
    /// restart prompt and the Part-B `connect` nudge read it to say
    /// "N running agents: …" before recycling the daemon. Populated on the
    /// daemon side from the live registry in the `Hello` handler; `None` on
    /// unrelated responses and on the static `daemon hello` CLI probe (which
    /// has no registry to enumerate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_agents: Option<RunningAgentsSummary>,
    /// PRD #161 M1.1: the daemon binary's `env!("DAD_VERSION")` (e.g.
    /// `0.31.1`) — the semver tag *without* the `-g<sha>[-dirty]` build suffix
    /// that `build_version` carries. Additive + optional so a future option B
    /// version-compatibility classification (deferred — see PRD #161 D2)
    /// becomes a non-breaking add: the field is already on the wire, an older
    /// daemon omits it (`None`), and `PROTOCOL_VERSION` is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    /// PRD #20 M3: the honest outcome of an input-delivery request
    /// ([`AttachRequest::WriteAndSubmit`]). `applied`/`queued` mean the input
    /// reached (or was accepted for) a live target; `history-only` /
    /// `no-live-target` / `stale` / `wrong-session` mean it was deliberately
    /// NOT delivered and the client should surface feedback rather than assume
    /// success. Additive + optional (`#[serde(default, skip_serializing_if]`):
    /// a pre-PRD-20 daemon omits it (decodes to `None`, read as "assume
    /// applied" by a newer client), and an older client ignores the extra
    /// field — so it is forward-compatible and needs no `PROTOCOL_VERSION` bump.
    /// `None` on every non-`WriteAndSubmit` response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_result: Option<crate::event::SendResult>,
    /// PRD #20 R20-003/004/006 (finding #6): guarded-send capability advertised
    /// on the `Hello` reply. `Some(true)` means this daemon enforces the
    /// identity/idempotency guards on `write-and-submit` (exact agent + session
    /// match, atomic delivery-id dedup). A NEW client checks THIS field — not the
    /// version number — before an identity-bearing send: when it is absent
    /// (`None`, an OLD daemon that never sets it), the client FAILS SAFE and does
    /// NOT submit, preserving pre-PRD-20 fire-once semantics rather than trusting
    /// an unguarded `ok=true` that could double-submit or mis-deliver on a rebind.
    /// Additive + optional: a pre-PRD-20 daemon omits it, and the plain
    /// [`AttachResponse::hello`] constructor leaves it `None` (only the live
    /// daemon's `Hello` handler sets it via [`AttachResponse::with_guarded_send`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guarded_send: Option<bool>,
    /// Issue #717: the answer to
    /// [`AttachRequest::DispatchWorktreeClosePreview`] — `Some` only when the
    /// close would leave a dispatched worktree on disk holding uncommitted
    /// work. `None` on every other response, and on a close that removes
    /// everything it touches. Additive + optional, so it is the field itself
    /// that is forward-compatible; the `PROTOCOL_VERSION` bump this shipped
    /// with is owed to the new REQUEST variant beside it, not to this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_worktree: Option<crate::issue_dispatch_run::KeptWorktree>,
    /// PRD #819 M2: the answer to [`AttachRequest::ListProjects`]. `None` on
    /// every other response. Additive + optional, so the field itself is
    /// forward-compatible in both directions; the `PROTOCOL_VERSION` bump this
    /// ships with is owed to the three new REQUEST variants beside it, not to
    /// this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<crate::event::ProjectListing>,
    /// PRD #819 M2: the answer to [`AttachRequest::ResolveProject`]. `None` on
    /// every other response, and on a refusal. Additive + optional on the same
    /// basis as [`Self::projects`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<crate::event::ResolvedProject>,
    /// PRD #819 M2: the answer to [`AttachRequest::PrepareWorkflow`]. `None` on
    /// every other response, and on a preparation that failed — a failed
    /// preparation publishes nothing and starts no roles. Additive + optional
    /// on the same basis as [`Self::projects`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_prepared: Option<crate::event::PreparedWorkflow>,
    /// PRD #819 M2/M5: explicit capability advertisement on the
    /// [`AttachRequest::Hello`] reply — the general form of
    /// [`Self::guarded_send`], which stays exactly as it is and is deliberately
    /// **not** folded into this list (an older client reads only the boolean,
    /// and moving it would break that client for no gain).
    ///
    /// Each entry is a stable string naming one op this build's dispatch
    /// accepts and answers; see [`DAEMON_CAPABILITIES`]. Unknown strings are
    /// ignored by a reader, so the set can grow without a bump.
    ///
    /// **Absence means "this daemon does not tell you", which a client must
    /// treat as "withhold" — never as "proceed".** That is the
    /// [`Self::guarded_send`] rule generalised, and it is the only safe reading:
    /// an older daemon omits the field entirely, and it is indistinguishable
    /// from a newer one that chose to.
    ///
    /// It is **compatibility metadata, NOT authentication.** A daemon controls
    /// its own replies and can therefore claim anything; what this buys is a
    /// stable answer to "will this op parse over there", in place of
    /// string-matching serde's `unknown variant …` message — which is not a
    /// stability contract. Nothing may branch on that error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// Issue #770: the orchestration ROLE registrations the daemon is holding
    /// in memory, populated on the [`AttachRequest::ListAgents`] reply.
    ///
    /// `daemon stop` reads it as a second data-loss guard beside the
    /// managed-agent one: those maps have no persistence path, so stopping the
    /// daemon destroys them, and any agent that survives the restart can never
    /// delegate again. Riding `ListAgents` rather than a new request is what
    /// keeps this compatible in both directions — a daemon predating the field
    /// omits it (`None`), and the client then applies exactly today's
    /// agent-only guard; an older client ignores the extra key. Additive and
    /// optional, so no [`PROTOCOL_VERSION`] bump.
    ///
    /// `Some(vec![])` and `None` are deliberately distinct: the first is a new
    /// daemon reporting that it holds no roles, the second is a daemon that
    /// cannot answer the question at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration_roles: Option<Vec<crate::state::OrchestrationRoleRecord>>,
}

impl AttachResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Default::default()
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            ..Default::default()
        }
    }
    pub fn agents(ids: Vec<String>) -> Self {
        Self {
            ok: true,
            agents: Some(ids),
            ..Default::default()
        }
    }
    /// Build a list-agents response that populates *both* the legacy
    /// `agents` field (just ids) and the new `agent_records` field (ids
    /// plus captured pane env). The dual shape is what keeps older
    /// clients reading just `agents` working alongside newer clients
    /// preferring `agent_records`.
    pub fn agent_records(records: Vec<AgentRecord>) -> Self {
        let ids = records.iter().map(|r| r.id.clone()).collect();
        Self {
            ok: true,
            agents: Some(ids),
            agent_records: Some(records),
            ..Default::default()
        }
    }
    pub fn with_id(id: String) -> Self {
        Self {
            ok: true,
            id: Some(id),
            ..Default::default()
        }
    }
    /// PRD #20 M3 / blocker-7: a reply to [`AttachRequest::WriteAndSubmit`] that
    /// carries the honest [`crate::event::SendResult`]. `ok` MIRRORS delivery —
    /// `true` only for `applied`/`queued`, `false` for every non-delivery
    /// (`history-only` / `no-live-target` / `stale` / `wrong-session`). This is
    /// the cross-version safety choice for Rule 12: an OLD client that ignores
    /// the unknown `send_result` field reads `ok=false` and correctly treats
    /// non-delivery as a failure (never a false success), while a NEW client
    /// reads the typed `send_result` BEFORE converting a non-ok response to an
    /// error (see [`crate::daemon_client::DaemonClient::write_and_submit`]). The
    /// request/response wire SHAPE is unchanged (additive optional field), so
    /// `PROTOCOL_VERSION` is not bumped.
    pub fn with_send_result(result: crate::event::SendResult) -> Self {
        let delivered = matches!(
            result,
            crate::event::SendResult::Applied | crate::event::SendResult::Queued
        );
        Self {
            ok: delivered,
            send_result: Some(result),
            ..Default::default()
        }
    }
    /// PRD #76 M2.21: protocol-version handshake reply. `version` is the
    /// daemon's [`PROTOCOL_VERSION`]; the client compares it against its own.
    ///
    /// PRD #103 M1.1: also carries the daemon's compiled-in `DAD_BUILD_ID`
    /// so the laptop can detect handler-code skew (same protocol version,
    /// different commit / dirty tree) the protocol version alone can't
    /// catch.
    pub fn hello(version: u32) -> Self {
        Self {
            ok: true,
            server_version: Some(version),
            // `local_build_id()` returns the compile-time
            // `env!("DAD_BUILD_ID")` in production; integration tests
            // (PRD #103 M4.2) inject a synthetic value via the
            // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` env var so they can
            // simulate same-tag / different-commit skew without
            // rebuilding the binary.
            build_version: Some(crate::build_id::local_build_id()),
            // PRD #161 M1.1: also advertise the daemon's compiled-in
            // `DAD_VERSION` (the semver tag, e.g. `0.31.1`). Always known at
            // compile time like `build_version`, so it rides every hello
            // reply — including the static `daemon hello` CLI probe. Additive
            // and optional; a future option B classifies on this field.
            daemon_version: Some(env!("DAD_VERSION").to_string()),
            ..Default::default()
        }
    }

    /// PRD #161 M1.1: attach a running-agent summary to a handshake reply.
    /// The daemon's `Hello` handler calls this with a snapshot of the live
    /// registry so the client's restart prompt / connect nudge can name the
    /// agents that recycling the daemon would stop. Static probes that have
    /// no registry (the `daemon hello` CLI) leave `running_agents` as `None`.
    pub fn with_running_agents(mut self, summary: RunningAgentsSummary) -> Self {
        self.running_agents = Some(summary);
        self
    }

    /// PRD #20 R20-003/004/006 (finding #6): advertise that this daemon enforces
    /// the guarded-send identity/idempotency contract. The live daemon's `Hello`
    /// handler calls this; a client uses the resulting `guarded_send = Some(true)`
    /// to decide it is SAFE to issue an identity-bearing `write-and-submit`. Its
    /// ABSENCE (any daemon that never calls this, including every pre-PRD-20
    /// build) makes a guarded send FAIL SAFE — see [`Self::guarded_send`].
    pub fn with_guarded_send(mut self) -> Self {
        self.guarded_send = Some(true);
        self
    }

    /// PRD #819 M2/M5: advertise [`DAEMON_CAPABILITIES`] on a handshake reply.
    /// The live daemon's `Hello` handler calls this; the static `daemon hello`
    /// CLI probe and the plain [`Self::hello`] constructor leave the field
    /// `None`, which a client reads as "withhold" — see [`Self::capabilities`].
    pub fn with_capabilities(mut self) -> Self {
        self.capabilities = Some(DAEMON_CAPABILITIES.iter().map(|c| c.to_string()).collect());
        self
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Bind the attach socket and return the listener, ready for `serve_attach`.
/// Cleans up any stale socket file before binding. Split from `run_attach_server`
/// so callers (notably tests) can synchronously confirm the listener is ready
/// to accept connections before spawning the async serve loop — this removes
/// the bind/accept readiness race that the old probe-and-retry pattern was
/// papering over.
pub fn bind_attach_listener(path: &Path) -> io::Result<IpcListener> {
    // PRD #163 M4: only where the endpoint *is* a filesystem path. A `\\.\pipe\`
    // name has no inode to go stale, and `remove_file` on one would fail rather
    // than clean anything up — see `platform::ipc::remove_stale_endpoint`.
    crate::platform::ipc::remove_stale_endpoint(path)?;
    // PRD #42 M2: `IpcListener::bind` does the umask-before-bind dance and the
    // defense-in-depth 0o600 restate that used to live here as a separate
    // `set_permissions` call, so the bound endpoint is owner-only unchanged.
    IpcListener::bind(path)
}

/// Accept-loop half of the attach server. Runs until the listener errors out
/// or the future is dropped. Pairs with `bind_attach_listener`.
///
/// `event_tx` is the daemon-wide `BroadcastMsg` broadcast (PRD #76
/// M2.17 for hook events; extended in M2.19 to also carry delegate
/// signals). It is held here so each accepted connection can call
/// `subscribe()` if the client opens a `SubscribeEvents` stream. The
/// cost of holding a `Sender` with zero subscribers is negligible —
/// `send` only succeeds when at least one `Receiver` exists.
pub async fn serve_attach(
    listener: IpcListener,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    // Discard counter and use an empty state so callers that don't care
    // about idle shutdown or daemon-side orchestration don't need to
    // construct either. The daemon's main path uses
    // [`serve_attach_with_counter`] with its real state.
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::RwLock;
    let dummy_count = Arc::new(AtomicUsize::new(0));
    let dummy_state: SharedState = Arc::new(RwLock::new(crate::state::AppState::default()));
    // No-counter callers (tests, the local daemon_client fallback) don't drive
    // the scheduler; hand them an empty stand-in so `ReloadSchedules`/`RunNow`
    // resolve against an empty registry rather than needing a real one.
    let dummy_scheduler = Arc::new(crate::scheduler::Scheduler::with_stderr_notifier());
    let dummy_reuse = crate::spawn::new_reuse_registry();
    let dummy_worktrees = crate::issue_dispatch_run::new_worktree_registry();
    serve_attach_with_counter(
        listener,
        registry,
        event_tx,
        dummy_count,
        dummy_state,
        None,
        dummy_scheduler,
        dummy_reuse,
        dummy_worktrees,
    )
    .await
}

/// PRD #93 M1.2 variant of [`serve_attach`] that maintains `client_count`
/// across the lifetime of each accepted connection. The daemon's idle
/// monitor reads this count alongside the PTY registry size to decide when
/// the daemon may exit (both must be zero for the configured idle window).
///
/// The counter is incremented immediately after `accept` returns and
/// decremented in the per-connection task's exit branch (panic or not — the
/// `tokio::spawn` future is wrapped so the decrement always runs).
#[allow(clippy::too_many_arguments)]
pub async fn serve_attach_with_counter(
    listener: IpcListener,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    client_count: Arc<std::sync::atomic::AtomicUsize>,
    state: SharedState,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    reuse_registry: crate::spawn::ReuseRegistry,
    worktree_registry: crate::issue_dispatch_run::WorktreeRegistry,
) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;
    // Issue #454: this is one of the two seams that first hold BOTH the
    // registry and the daemon's own `AppState`, so it is where the admission
    // check learns to ask the registry who this daemon owns. Without it,
    // `AppState::apply_event` falls back to the pane set alone and every
    // lifecycle report from an ordinary daemon-spawned pane is dropped as
    // unowned. Idempotent — `run_daemon_with` installs the same registry.
    //
    // Weakly — see the same call in `crate::daemon::run_daemon_with` for the
    // reference cycle a strong reference closes. `registry` is this function's
    // own argument and outlives the loop below, so the oracle stays answerable
    // for as long as the server runs.
    {
        let ownership: Arc<dyn crate::state::AgentOwnership> = registry.clone();
        state
            .write()
            .await
            .set_agent_ownership(Arc::downgrade(&ownership));
    }
    // PRD #93 round-2 reviewer REV-1: the same Notify the registry uses for
    // spawn/close/exit transitions also fires on every attach-counter
    // transition. The daemon's edge-triggered idle monitor waits on it, so
    // a brief detach+reconnect wakes the monitor before any timer can fire.
    // Cloned once per accepted connection — `notify_one` is cheap and tokio
    // Notify stores a permit if no waiter is registered, so a signal sent
    // between the monitor's loop iterations isn't lost.
    let change_notify: Arc<Notify> = registry.change_notify();
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let registry = registry.clone();
                let event_tx = event_tx.clone();
                let counter = client_count.clone();
                let state = state.clone();
                let notify = change_notify.clone();
                let shutdown = shutdown.clone();
                let scheduler = scheduler.clone();
                let reuse_registry = reuse_registry.clone();
                let worktree_registry = worktree_registry.clone();
                tokio::spawn(async move {
                    // RAII guard: increments on creation, decrements on drop,
                    // so a `handle_connection` task that panics or is dropped
                    // still releases its slot in the client count. Without
                    // the guard, an unwinding task would leak a slot and
                    // keep the daemon alive past the idle threshold.
                    //
                    // The guard also signals `change_notify` on drop so the
                    // edge-triggered idle monitor wakes immediately on
                    // disconnect (PRD #93 round-2 reviewer REV-1).
                    struct ClientGuard {
                        counter: Arc<std::sync::atomic::AtomicUsize>,
                        notify: Arc<Notify>,
                    }
                    impl Drop for ClientGuard {
                        fn drop(&mut self) {
                            self.counter.fetch_sub(1, Ordering::SeqCst);
                            self.notify.notify_one();
                        }
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    // Signal the increment too so the monitor cancels any
                    // pending shutdown timer the moment a fresh client
                    // connects, not after the next decrement.
                    notify.notify_one();
                    let _guard = ClientGuard {
                        counter: counter.clone(),
                        notify: notify.clone(),
                    };
                    if let Err(e) = handle_connection(
                        stream,
                        registry,
                        event_tx,
                        state,
                        shutdown,
                        scheduler,
                        reuse_registry,
                        worktree_registry,
                    )
                    .await
                    {
                        warn!("attach protocol connection error: {e}");
                    }
                });
            }
            Err(e) => {
                error!("attach accept failed: {e}");
                return Err(e);
            }
        }
    }
}

/// Bind the attach socket and serve protocol connections forever. Cleans up
/// any stale socket file before binding. Runs until the listener errors out
/// or the future is dropped.
pub async fn run_attach_server(
    path: &Path,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    let listener = bind_attach_listener(path)?;
    info!("Attach protocol listening on {}", path.display());
    serve_attach(listener, registry, event_tx).await
}

/// PRD #93 M1.2 counter-aware sibling of [`run_attach_server`]. The daemon
/// loop uses this so the idle monitor sees attached-client transitions in
/// real time.
pub async fn run_attach_server_with_counter(
    path: &Path,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    client_count: Arc<std::sync::atomic::AtomicUsize>,
    state: SharedState,
) -> io::Result<()> {
    let listener = bind_attach_listener(path)?;
    info!("Attach protocol listening on {}", path.display());
    let dummy_scheduler = Arc::new(crate::scheduler::Scheduler::with_stderr_notifier());
    let dummy_reuse = crate::spawn::new_reuse_registry();
    let dummy_worktrees = crate::issue_dispatch_run::new_worktree_registry();
    serve_attach_with_counter(
        listener,
        registry,
        event_tx,
        client_count,
        state,
        None,
        dummy_scheduler,
        dummy_reuse,
        dummy_worktrees,
    )
    .await
}

/// PRD #20 M3 / R20-003/006 (findings #3, #4, #7): compute the honest
/// [`crate::event::SendResult`] for a `write-and-submit` request. Shared by the
/// idempotent (delivery-id) and legacy paths.
///
/// A dashboard-visible session is not necessarily a live, writable target: a
/// `history-only` / `none` pane is reported WITHOUT writing so the TUI surfaces
/// feedback. A `Live` pane goes through [`AgentPtyRegistry::write_and_submit_guarded`],
/// which binds delivery to the EXACT target agent and RE-VALIDATES liveness +
/// (finding #4) the daemon-authoritative hook-session generation AFTER acquiring
/// the writer, immediately before the write. `Err` is a clean transport failure
/// (nothing written); `Ok(Ambiguous)` a partial write.
///
/// PRD #20 Greptile P1 class-close (stale pre-lock snapshot): every
/// authorization input the guard consults — writability, the hook-session
/// generation, AND live-attachment — is sampled INSIDE the post-lock
/// re-validation closure, so a state change that lands while the send waits for
/// the writer lock (a pane becoming attached, a `/clear`, a liveness flip)
/// cannot be masked by a value snapshotted before the lock. The pre-lock
/// `pane_writable` read below is routing-only (it picks the HistoryOnly / None /
/// Live branch) and fails closed; the Live branch never trusts it — the closure
/// re-checks writability post-lock.
///
/// Issue #608 audit (finding 5): "sampled inside the closure" is not "sampled
/// TOGETHER". Writability and the hook-session generation share ONE `AppState`
/// read guard in there; live-attachment is read from the registry just before
/// that guard is awaited, and nothing the guarded send holds prevents a
/// subscription landing in between — so attachment alone can be one lock
/// acquisition stale, in the PERMISSIVE direction. The closure documents that
/// window and what would close it; the issue #608 arm is written so it does not
/// consult the value at all.
///
/// Issue #608: BOTH arms fail closed on an ABSENT identity, where the paned one
/// used to be permissive. A request that names no agent resolves to
/// `Writable::None` → [`crate::event::SendResult::NoLiveTarget`] before any
/// write is attempted, so reaching the `Live` arm implies `expected_agent_id` is
/// `Some`. The paned re-validation closure additionally refuses a request that
/// names NO session when the pane already carries a current hook session,
/// attached or not: the daemon knows a conversation the caller did not name,
/// which is the state-race / split-view shape, and `Stale` sends the caller back
/// for a fresh generation. A pane that has no current hook session either (an
/// agent that never emits one) still delivers with no session named — a
/// deliberate carve-out that CANNOT distinguish such an agent from a
/// conversation that has just ended, which is a known hole spelled out at the
/// arm that implements it.
async fn compute_write_and_submit_outcome(
    registry: &AgentPtyRegistry,
    state: &SharedState,
    pane_id: &str,
    text: &str,
    extras: &WriteAndSubmitExtras,
) -> Result<crate::event::SendResult, String> {
    use crate::agent_pty::GuardedSend;
    use crate::event::{SendResult, Writable};
    // PRD #20 Greptile (paneless guarded send): a daemon-side agent that carries
    // no pane maps to the `<no-pane>` sentinel. `pane_writable` filters sessions
    // by `pane_id` and can never find such a session (it is stored with
    // `pane_id == None`), so it falls through to the `Live` default — letting a
    // history-only / view-only paneless target pass the liveness gate. Resolve a
    // paneless target's writability by AGENT identity instead, mirroring the
    // attach STREAM_IN input loop. Paned targets keep the pane-keyed resolution.
    //
    // Issue #608: a request that carries NO agent identity cannot be routed by
    // identity on EITHER arm, so it fails closed here — `Writable::None` → no
    // delivery attempted. The pane-less arm always worked this way; the paned
    // arm used to hand a possibly-`None` identity to `write_and_submit_guarded`,
    // which then wrote keyed only by `pane_id` — i.e. into whichever agent holds
    // that pane right now, which is not necessarily the one the caller meant.
    // A pane id is a recycled handle, so that is precisely the accidental
    // mis-delivery this machinery exists to prevent everywhere else.
    //
    // Gated HERE, in the routing resolution, rather than inside the post-lock
    // closure: it makes the invariant STRUCTURAL. Reaching `Writable::Live`
    // below now implies `expected_agent_id` is `Some` on both arms, so the
    // guarded call passes a concrete id instead of an `Option` that merely
    // happens never to be `None`.
    let is_paneless = pane_id == "<no-pane>";
    let writable = match extras.expected_agent_id.as_deref() {
        None => Writable::None,
        Some(agent_id) => {
            let guard = state.read().await;
            if is_paneless {
                guard.agent_writable(agent_id)
            } else {
                guard.pane_writable(pane_id)
            }
        }
    };
    match writable {
        Writable::HistoryOnly => Ok(SendResult::HistoryOnly),
        Writable::None => Ok(SendResult::NoLiveTarget),
        Writable::Live => {
            // The re-validation closure runs UNDER the held target writer (inside
            // `write_and_submit_guarded`), immediately before the write, against
            // the authoritative session state.
            let st = state.clone();
            // Issue #608: `Live` implies `expected_agent_id` is `Some` on BOTH
            // arms — the resolution block above refuses an identity-less request
            // outright — so every guarded call below binds to a concrete id
            // rather than forwarding an `Option`.
            let agent_id = extras
                .expected_agent_id
                .clone()
                .expect("a Live target implies an expected agent id");
            let guarded = if is_paneless {
                // A paneless target is re-validated by agent identity (mirroring
                // STREAM_IN). `<no-pane>` has no pane→hook-session mapping, so the
                // pane-keyed session-generation guard (finding #4) does not apply
                // — STREAM_IN likewise performs no session check on a paneless
                // target.
                let agent_for_check = agent_id.clone();
                registry
                    .write_and_submit_guarded(pane_id, text, Some(&agent_id), move || async move {
                        st.read().await.agent_writable(&agent_for_check) == Writable::Live
                    })
                    .await
            } else {
                let pane_for_check = pane_id.to_string();
                let expected_session = extras.expected_session_id.clone();
                registry
                    .write_and_submit_guarded(pane_id, text, Some(&agent_id), move || async move {
                        // PRD #20 Greptile P1 (daemon_protocol.rs:988) + the
                        // stale-pre-lock-snapshot CLASS close: this closure runs
                        // UNDER the held target writer, immediately before the
                        // write, and re-reads its authorization inputs HERE
                        // rather than trusting values captured before the guarded
                        // send went looking for the writer. That is why
                        // `has_live_attach` moved in: it used to be read BEFORE
                        // `write_and_submit_guarded` acquired the writer and
                        // consulted here (stale), so a pane that became attached
                        // WHILE the send waited for the writer was still seen as
                        // unattached, letting a stale prompt slip into the
                        // freshly-attached conversation.
                        //
                        // Issue #608 audit, finding 5 — WHAT SHARES A SNAPSHOT
                        // AND WHAT DOES NOT. This comment used to call the
                        // closure the SINGLE delivery-time authorization snapshot
                        // in which EVERY input it consults is sampled here,
                        // post-lock. That holds for `pane_writable` and
                        // `pane_hook_session_id`: both are read below under ONE
                        // `AppState` read guard, so they are mutually consistent
                        // and both post-writer-lock. It does NOT hold for
                        // attachment. `has_live_attach` is read from the REGISTRY
                        // first, deliberately before `st.read()` is awaited so no
                        // state lock is held across the registry `inner` mutex —
                        // and `AgentPtyRegistry::subscribe` builds its receiver
                        // under the registry/bus locks WITHOUT ever acquiring the
                        // target writer, so holding that writer fences nothing
                        // out. A pane can therefore become attached between this
                        // sample and the state guard below, and the closure then
                        // authorizes on a stale `false`. The residual window is
                        // one lock acquisition rather than the unbounded
                        // wait-for-writer the move above closed, but it is a real
                        // window and it fails PERMISSIVE. Closing it means making
                        // subscription participate in the writer-held barrier — a
                        // registry change, deferred to the follow-up issue that
                        // carries the sibling call sites, not done here. The
                        // issue #608 arm below is written so it never depends on
                        // this value; only the pre-existing named-session arm
                        // does.
                        let has_live_attach = registry.pane_has_live_attach(&pane_for_check);
                        let guard = st.read().await;
                        if guard.pane_writable(&pane_for_check) != Writable::Live {
                            return false;
                        }
                        // PRD #20 R20-003 (finding #4): is a deck client actively
                        // driving this pane? The strict "reject a None
                        // current-session" rule applies to a LIVE INTERACTIVE
                        // (attached) pane — finding #4's threat is a stale prompt
                        // surfacing in the conversation the user is watching. A
                        // headless (unattached) delivery whose agent identity is
                        // confirmed proceeds. In the real deck the TUI is always
                        // attached to a pane it drives, so this is the strict
                        // guard for every real delivery. It scopes the
                        // NAMED-session arm only, and is the sole consumer of
                        // `has_live_attach` here — the issue #608 arm for an
                        // UNNAMED session deliberately does not read it.
                        //
                        // When the caller named a session, require an EXACT match
                        // against the pane's CURRENT daemon-authoritative
                        // hook-session generation. A same-agent `/clear` / thread
                        // restart rolls the generation over → mismatch → reject
                        // (always). A `None` current-session (the session ended,
                        // or none was recorded) is refused too on an attached,
                        // live-interactive pane — never a silent accept.
                        //
                        // Issue #608: and when the caller named NO session, the
                        // silent accept is closed on the SAME evidence, which is
                        // what the paragraph above always claimed and the code
                        // did not do. A pane that HAS a current hook session is a
                        // conversation the daemon knows about and the caller did
                        // not name — the state-race / split-view shape — so the
                        // write is refused rather than landing in a generation
                        // nobody bound it to. Every in-tree caller that CAN know
                        // a generation supplies one
                        // (`process_pending_seed_prompts` captures the
                        // snapshot's current generation,
                        // `deliver_orchestrator_prompt` calls
                        // `bind_delivery_generation` before its first write, and
                        // both call `bind_generation_before_retry` before a retry
                        // enters a late generation), so an absent expectation
                        // against a known generation is a mismatch, not a caller
                        // that has nothing to say. `Stale` is the right
                        // vocabulary — both TUI delivery paths already classify
                        // it as retryable, so the next snapshot that OBSERVES the
                        // generation binds it and the retry names it.
                        //
                        // Issue #608 audit, finding 6 — HOW FAR THAT RECOVERS.
                        // In the ordinary race it recovers fully, and this
                        // comment used to stop there. A session that lands after
                        // the caller's snapshot costs exactly one safe `Stale`:
                        // the refusal writes nothing and does not bump
                        // `attempts`, so `crate::ui`'s `bind_delivery_generation`
                        // binds the generation on the next render pass that sees
                        // it (`bind_generation_before_retry` does the same for a
                        // delivery that already wrote), and the retry names it.
                        // Binding once rather than every frame is also what keeps
                        // this from looping.
                        //
                        // It is NOT a general guarantee. `Stale` does not carry
                        // the daemon's current generation, so a refused caller's
                        // ONLY route to it is its own event stream — and
                        // `spawn_event_subscriber` (`main.rs`) resubscribes after
                        // a lagged or errored stream WITHOUT replaying what it
                        // missed. A `SessionStart` dropped in that window is
                        // never applied to the client `AppState`, so its
                        // `pane_hook_session_id` for the pane stays `None`
                        // indefinitely: every retry goes out unnamed, every one
                        // is refused here, and at
                        // `crate::prompt_delivery::AUTOMATIC_PROMPT_DEADLINE`
                        // (60 s) the delivery is ABANDONED with the prompt never
                        // delivered. Bounded and logged rather than silent or
                        // mis-delivered — but lost. Closing it means
                        // resynchronizing state after a reconnect, or returning
                        // the daemon's current generation on `Stale`; both are
                        // design changes outside this branch.
                        //
                        // Issue #608 audit, finding 5(b): this arm refuses on the
                        // SESSION evidence alone, with no `has_live_attach`
                        // conjunct. Attachment is the one input this closure
                        // cannot sample under the state guard (see the block
                        // above), and gating a NEW refusal on the one value that
                        // can be stale in the permissive direction would import
                        // that hole straight into it. Refusing regardless of
                        // attachment is a strict superset — it can only reject
                        // more — and `Stale` is retryable, so an unattached
                        // caller whose snapshot HAS the generation names it on
                        // the next attempt (one whose snapshot never observes it
                        // retries unnamed until the deadline — see finding 6
                        // above). Measured before adopting: across the whole
                        // fast tier the ONLY paned send that reaches this arm
                        // against a current generation is an ATTACHED one, which
                        // both rules refuse identically.
                        //
                        // Issue #608 audit, finding 4 — WHAT THIS CARVE-OUT
                        // CANNOT SEE. `(expected None, current None)` still
                        // delivers, because an agent that never emits a
                        // generation legitimately carries neither side of the
                        // comparison. But a `None` CURRENT generation is not
                        // proof that that is what this is. A matching, current
                        // `SessionEnd` REMOVES the pane's `pane_hook_session`
                        // entry (`AppState::apply_event`) while the registry
                        // agent and the attached pane stay alive, so a
                        // conversation that has just ENDED reads here exactly
                        // like an agent that never had one. During a `/clear` or
                        // a thread restart the successor's `SessionStart` has not
                        // landed yet, and a caller that knows the stable agent id
                        // but names no session can land a write in that gap —
                        // into a pane that has demonstrably just closed a logical
                        // conversation. This arm ACCEPTS that write. It is a
                        // deliberate carve-out with a known hole, not an airtight
                        // guard, and issue #608 exists precisely because a
                        // comment in this closure once promised more than the
                        // code delivered.
                        //
                        // Closing it needs evidence this closure does not have:
                        // an AGENT-scoped ended-generation tombstone, or an
                        // `ever_had_generation` witness. The daemon already
                        // records distinguishing evidence in
                        // `AppState::pane_generation_closures` — but keyed BY
                        // PANE, and pane ids are recycled, so a genuinely
                        // sessionless successor must not inherit its
                        // predecessor's policy. That is new daemon state with its
                        // own lifetime and reuse semantics; it is deferred to the
                        // follow-up issue rather than bolted on here, where
                        // getting it wrong would refuse exactly the sessionless
                        // agents this carve-out exists to protect.
                        match expected_session.as_deref() {
                            Some(expected) => match guard.pane_hook_session_id(&pane_for_check) {
                                Some(current) if current != expected => return false,
                                Some(_) => {}
                                None if has_live_attach => return false,
                                None => {}
                            },
                            None => {
                                if guard.pane_hook_session_id(&pane_for_check).is_some() {
                                    return false;
                                }
                            }
                        }
                        true
                    })
                    .await
            };
            match guarded {
                Ok(GuardedSend::Applied) => Ok(SendResult::Applied),
                Ok(GuardedSend::WrongSession) => Ok(SendResult::WrongSession),
                Ok(GuardedSend::Stale) => Ok(SendResult::Stale),
                Ok(GuardedSend::NoLiveTarget) => Ok(SendResult::NoLiveTarget),
                Ok(GuardedSend::Ambiguous) => Ok(SendResult::Ambiguous),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

/// The orchestration bits of a `StartAgent`'s `TabMembership`, captured before
/// the spawn moves `SpawnOptions` (PRD #93 round-5) and consumed afterwards to
/// populate the daemon-side routing maps. A named struct rather than a tuple
/// because PRD #140 added a fifth member and the positional form stopped being
/// readable at the destructuring site.
struct OrchestrationSpawnMeta {
    /// The orchestration's resolved config name.
    name: String,
    /// This pane's role within the orchestration.
    role_name: String,
    /// Whether this pane is the orchestrator (start) role.
    is_start_role: bool,
    /// Round-11 auditor #C: the tab-wide cwd, the `NameCwd` disambiguator.
    orchestration_cwd: Option<String>,
    /// PRD #140: the per-tab instance token, when the client stamped one.
    orchestration_id: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    mut stream: IpcStream,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    state: SharedState,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    reuse_registry: crate::spawn::ReuseRegistry,
    worktree_registry: crate::issue_dispatch_run::WorktreeRegistry,
) -> io::Result<()> {
    let frame = match read_frame(&mut stream).await? {
        Some(f) => f,
        None => return Ok(()),
    };
    // PRD #92 F1: client → server `KIND_SHUTDOWN` is a header-only frame
    // that means "shut the daemon down now." It comes before any
    // `KIND_REQ`, so handle it before the usual request-decoding path.
    //
    // PRD #92 F1 followup hardening:
    //   (a) Reject any non-empty payload — `KIND_SHUTDOWN` is contractually
    //       header-only, and an attacker (or an upgrade-mismatch peer)
    //       smuggling bytes alongside the kind byte must not be able to
    //       trigger daemon teardown by mistake. Drop the frame silently
    //       (close the connection, do not initiate shutdown).
    //   (b) Send a `KIND_SHUTDOWN_ACK` **before** initiating teardown so
    //       the client can distinguish "daemon acknowledged" from
    //       "old daemon closed the connection on an unknown frame."
    //       Teardown can take ≥3 seconds (SIGTERM grace + SIGKILL), so
    //       the ack must be on the wire first.
    if frame.0 == KIND_SHUTDOWN {
        if !frame.1.is_empty() {
            warn!(
                payload_len = frame.1.len(),
                "KIND_SHUTDOWN rejected — frame is contractually header-only"
            );
            return Ok(());
        }
        info!("KIND_SHUTDOWN received — sending ack and beginning graceful daemon shutdown");
        // Ack first: the client's `send_shutdown` waits up to 1s for this
        // frame and treats absence as a hard error. Writing the ack
        // before kicking off the registry drain keeps the wire ordering
        // honest even if the teardown itself takes the full 3-second
        // SIGTERM grace.
        if let Err(e) = write_frame(&mut stream, KIND_SHUTDOWN_ACK, &[]).await {
            warn!(error = %e, "failed to write KIND_SHUTDOWN_ACK before shutdown — proceeding anyway");
        }
        // Drop the registry's children with a 3-second grace window for
        // SIGTERM to take effect; survivors get SIGKILL via the existing
        // teardown.
        let registry_for_shutdown = registry.clone();
        tokio::task::spawn_blocking(move || {
            registry_for_shutdown.shutdown_all_graceful(Duration::from_secs(3));
        })
        .await
        .ok();
        if let Some(s) = shutdown {
            s.notify_one();
        } else {
            // `serve_attach` (test/harness path) doesn't pass a shutdown
            // notify because tests don't run the production hook loop.
            // The registry was still drained, so the test can assert on
            // that side effect.
            warn!(
                "KIND_SHUTDOWN handled but no daemon-shutdown notify wired (likely a test harness)"
            );
        }
        return Ok(());
    }
    if frame.0 != KIND_REQ {
        let resp = AttachResponse::err(format!("expected REQ frame, got kind 0x{:02x}", frame.0));
        write_resp(&mut stream, &resp).await?;
        return Ok(());
    }
    let req: AttachRequest = match serde_json::from_slice(&frame.1) {
        Ok(r) => r,
        Err(e) => {
            let resp = AttachResponse::err(format!("malformed request: {e}"));
            write_resp(&mut stream, &resp).await?;
            return Ok(());
        }
    };

    // PRD #819 audit follow-up: `StartPreparedAgent` is `StartAgent` plus a
    // REQUIRED token, so it is normalised into the base shape and an explicit
    // token here. Everything downstream is then ONE spawn arm — the prepared and
    // unprepared starts cannot drift apart in what they register, what they
    // seed, or what they clean up, which is the failure mode a second copy of
    // that 200-line arm would have.
    //
    // Note what this normalisation does NOT do: it does not put the token back
    // on the wire's `start-agent`. The `op` a peer sent has already decided
    // whether this connection is a prepared start, which is the whole property
    // the verb buys — a daemon that lacks the variant never reaches this line.
    let (req, prepared_token) = match req {
        AttachRequest::StartPreparedAgent {
            prep_token,
            command,
            cwd,
            rows,
            cols,
            env,
            display_name,
            tab_membership,
            agent_type,
            seed,
        } => {
            // Refused where `PrepareWorkflow` is refused, and for its reason
            // rather than a reason of its own: no preparation can exist on this
            // platform, so nothing can be started against one. Saying so beats
            // the `stale-token` this would otherwise produce — that answer is
            // true but sends an operator looking for a client bug — and it keeps
            // `DAEMON_CAPABILITIES`'s promise honest, since the verb is withheld
            // there and a withheld verb this build still answered would make the
            // advertised set mean less than it says.
            if let Err(message) = refuse_prepared_start_where_unsupported() {
                write_resp(&mut stream, &AttachResponse::err(message)).await?;
                return Ok(());
            }
            (
                AttachRequest::StartAgent {
                    command,
                    cwd,
                    rows,
                    cols,
                    env,
                    display_name,
                    tab_membership,
                    agent_type,
                    seed,
                },
                Some(prep_token),
            )
        }
        other => (other, None),
    };

    match req {
        AttachRequest::ListAgents => {
            let mut records = registry.agent_records();
            // PRD #162: enrich each registry record with the daemon's live,
            // event-derived session state so a reconnecting TUI restores the
            // real status / agent type / active tool / tool count / prompt
            // context instead of minting a bare `Idle` / "No agent"
            // placeholder. Match the live `SessionState` on BOTH the agent id
            // (registry `record.id` == `SessionState.agent_id`) AND the pane
            // id (`record.pane_id_env` == `SessionState.pane_id`); a `/clear`
            // restart can leave a stale session on the same keys, so break ties
            // by the most-recent `last_activity` (newest-wins). The dummy-state
            // `serve_attach` path carries an empty `AppState`, so this loop
            // attaches nothing and `live` stays `None` — today's behavior.
            //
            // The pick must be total/deterministic: `HashMap::values()` yields
            // sessions in an unspecified order, so two sessions tying on an
            // identical `last_activity` would otherwise resolve by hash order.
            // Break that tie on the (unique) `session_id` so the same input
            // always selects the same snapshot.
            {
                let guard = state.read().await;
                for record in &mut records {
                    record.live = guard
                        .sessions
                        .values()
                        .filter(|s| {
                            s.agent_id.as_deref() == Some(record.id.as_str())
                                && s.pane_id == record.pane_id_env
                        })
                        .max_by(|a, b| {
                            a.last_activity
                                .cmp(&b.last_activity)
                                .then_with(|| a.session_id.cmp(&b.session_id))
                        })
                        .map(|s| s.live_snapshot());
                }
            }
            // Issue #770: report the orchestration role registrations whose pane
            // still has a live agent, so `daemon stop` can refuse to destroy
            // them. Read under its own short guard rather than inside the join
            // loop above — it is one snapshot of the whole map, not a per-record
            // lookup.
            let orchestration_roles = state.read().await.live_orchestration_roles(&registry);
            let mut resp = AttachResponse::agent_records(records);
            resp.orchestration_roles = Some(orchestration_roles);
            write_resp(&mut stream, &resp).await?;
        }
        AttachRequest::StartAgent {
            command,
            cwd,
            rows,
            cols,
            env,
            display_name,
            tab_membership,
            agent_type,
            seed,
        } => {
            // PRD #92 F1 followup hardening: refuse to start a new agent
            // while the registry's `shutting_down` latch is set. The
            // latch is flipped at the start of `shutdown_all_graceful`
            // so a race between an in-flight `StartAgent` and a
            // `KIND_SHUTDOWN` cannot spawn a new child the teardown is
            // about to miss. Reply with a clean error rather than
            // letting the spawn race the drain.
            if registry.is_shutting_down() {
                write_resp(
                    &mut stream,
                    &AttachResponse::err("start-agent: daemon is shutting down"),
                )
                .await?;
                return Ok(());
            }
            // Trust boundary: same OS user, same exec capability — see the
            // `AttachRequest::StartAgent` docs. We forward `command`/`cwd`/
            // `env` to the spawn path verbatim. The only check here is a
            // sanity guard against an empty/whitespace-only `command`,
            // which is almost certainly a client bug rather than an
            // attack: it would otherwise resolve to a binary named "" or
            // " " and fail with a confusing OS error. This is *not* an
            // allowlist.
            if let Some(c) = command.as_deref()
                && c.trim().is_empty()
            {
                write_resp(
                    &mut stream,
                    &AttachResponse::err("start-agent: command is empty or whitespace-only"),
                )
                .await?;
                return Ok(());
            }

            // PRD #819 audit follow-up: a token reaches this arm ONLY by having
            // arrived on `start-prepared-agent`, which the normalisation above
            // turned into the base shape plus `prepared_token`. A plain
            // `start-agent` that spells a `prep_token` key is a caller aiming at
            // the wrong verb, and it is refused rather than served: serde drops
            // unknown keys on the base variant, so serving it would start the
            // role UNENFORCED and report success — the fail-open the verb exists
            // to remove, reintroduced by silence. See `StartAgentExtras`.
            //
            // `Option<serde_json::Value>` accepts any JSON, and this payload has
            // already decoded as `StartAgent`, so the re-parse cannot fail; the
            // `Err` arm is refused anyway rather than defaulted, because the one
            // thing that must never happen here is a token being downgraded into
            // an absent one.
            if prepared_token.is_none() {
                let carries_token = match serde_json::from_slice::<StartAgentExtras>(&frame.1) {
                    Ok(extras) => extras.prep_token.is_some(),
                    Err(_) => true,
                };
                if carries_token {
                    write_resp(
                        &mut stream,
                        &AttachResponse::err(format!(
                            "{PROJECT_ERR_WRONG_START_VERB}: a preparation token does not belong \
                             on `start-agent`, which enforces none; send `start-prepared-agent` \
                             instead. Nothing was started."
                        )),
                    )
                    .await?;
                    return Ok(());
                }
            }
            // PRD #819 audit fix: "the token exists and is young" was the WHOLE
            // check here, and it binds nothing. The record now carries the state
            // its preparation approved, so a presented token is resolved to that
            // record and the record is re-validated against the filesystem
            // before anything spawns.
            //
            // PRD #819 Greptile P1(a): re-validating the record was only half of
            // it. The record was checked against the FILESYSTEM and never against
            // the REQUEST, so a caller could present a token prepared for project
            // X while submitting the `cwd`, orchestration and role of project Y —
            // and the daemon validated X and started Y. The submitted identity is
            // now matched against the binding too
            // (`project_resolve::verify_prepared_start`), which is where the list
            // of what is bound and what deliberately is not (the `command`, so
            // per-launch overrides keep working) lives.
            //
            // Three refusals, deliberately distinct, because they send an
            // operator in three directions: the token is not ours or has aged out
            // (`stale-token`); it is ours and live but the world moved under it
            // (`stale-preparation`); or it is ours, live and intact and this is
            // not the launch it approved (`preparation-mismatch`).
            if let Some(token) = prepared_token.as_deref() {
                let Some(binding) = crate::prep_token::binding(token) else {
                    write_resp(
                        &mut stream,
                        &AttachResponse::err(format!(
                            "{PROJECT_ERR_STALE_TOKEN}: that preparation is unknown or has \
                             expired; prepare the workflow again"
                        )),
                    )
                    .await?;
                    return Ok(());
                };
                // The submitted identity, lifted out of the request as it stands
                // rather than re-derived from the token — comparing the token to
                // itself is what the previous round did.
                let request = crate::project_resolve::PreparedStartRequest {
                    cwd: cwd.clone(),
                    membership: tab_membership.as_ref().and_then(|tm| match tm {
                        TabMembership::Orchestration {
                            name,
                            role_name,
                            is_start_role,
                            orchestration_cwd,
                            ..
                        } => Some(crate::project_resolve::PreparedStartMembership {
                            orchestration: name.clone(),
                            orchestration_cwd: orchestration_cwd.clone(),
                            role: role_name.clone(),
                            is_start_role: *is_start_role,
                        }),
                        TabMembership::Mode { .. } => None,
                    }),
                };
                // Filesystem work, so it goes through the same bounded blocking
                // pool every other project verb uses — one call, one permit, and
                // never from inside a task that already holds one.
                let outcome = crate::project_resolve::run_bounded(move || {
                    crate::project_resolve::verify_prepared_start(&binding, &request)
                })
                .await;
                let refusal = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(refusal)) => {
                        // The cause is named here and nowhere else: the wire gets
                        // one sentence per category, so the daemon log is the
                        // only place an operator can learn which check fired.
                        warn!(
                            reason = %refusal,
                            "start-prepared-agent refused: the preparation does not cover this start"
                        );
                        Some(refusal.wire_refusal())
                    }
                    Err(e) => {
                        // The pool could not run the check. Refusing is the only
                        // safe answer: "we could not verify" is not "it is
                        // fine", and the caller's remedy — prepare again — is
                        // the same either way. It is reported as a STALENESS
                        // refusal rather than a mismatch, because an unrun check
                        // has found no disagreement — it has found nothing.
                        warn!(reason = %e, "start-prepared-agent refused: the preparation could not be re-validated");
                        Some(crate::project_resolve::stale_preparation_refusal())
                    }
                };
                if let Some(refusal) = refusal {
                    write_resp(&mut stream, &AttachResponse::err(refusal)).await?;
                    return Ok(());
                }
            }

            // PRD #93 round-5: capture the bits we need to populate the
            // daemon's `AppState` role map BEFORE the spawn (we'll need
            // the pane id from env and the orchestration metadata from
            // tab_membership). The spawn moves `opts`, so we clone what
            // we need first.
            let pane_id_env: Option<String> = env
                .iter()
                .find(|(k, _)| k == DOT_AGENT_DECK_PANE_ID)
                .map(|(_, v)| v.clone())
                .filter(|v| is_valid_pane_id_env(v));
            // Round-11 auditor #C: also pull `orchestration_cwd` out of
            // the membership so the daemon can use it (not StartAgent.cwd)
            // as the disambiguator in `pane_orchestration_map`. This keeps
            // round-9 #2's "workers can have different per-pane cwds"
            // contract intact — pane_cwd_map gets StartAgent.cwd
            // per-pane, but pane_orchestration_map keys on the shared
            // orchestration cwd from the TabMembership.
            // PRD #140 M2.0: also pull the per-tab `orchestration_id` so the
            // identity can key on it when present (see below).
            let orchestration_meta: Option<OrchestrationSpawnMeta> =
                tab_membership.as_ref().and_then(|tm| match tm {
                    TabMembership::Orchestration {
                        name,
                        role_name,
                        is_start_role,
                        orchestration_cwd,
                        orchestration_id,
                        ..
                    } if !role_name.is_empty() => Some(OrchestrationSpawnMeta {
                        name: name.clone(),
                        role_name: role_name.clone(),
                        is_start_role: *is_start_role,
                        orchestration_cwd: orchestration_cwd.clone(),
                        orchestration_id: orchestration_id.clone(),
                    }),
                    _ => None,
                });
            let cwd_for_state = cwd.clone();

            let opts = SpawnOptions {
                command: command.as_deref(),
                cwd: cwd.as_deref(),
                display_name: display_name.as_deref(),
                rows,
                cols,
                env,
                tab_membership,
                agent_type,
            };
            match registry.spawn_agent(opts) {
                Ok(id) => {
                    // PRD #201 native prompt delivery: if the spawn carried a
                    // seed (a Pi start-role orchestrator pane), stash it for the
                    // pane's extension to pull natively via `get-seed`, and arm
                    // the PTY-injection safety net in case the pull never comes.
                    // Do this right after spawn so the seed is available before
                    // pi boots + fires `session_start`. Non-seed spawns (every
                    // other pane) skip this entirely and keep the legacy path.
                    if let (Some(pane_id), Some(seed)) = (pane_id_env.as_deref(), seed.as_deref())
                        && !seed.trim().is_empty()
                    {
                        registry.set_pending_seed(pane_id, seed);
                        crate::agent_pty::arm_seed_fallback(
                            registry.clone(),
                            pane_id.to_string(),
                            crate::agent_pty::seed_fallback_grace(),
                        );
                    }
                    // Issue #454: NOTHING is registered in the daemon's
                    // `AppState` here, deliberately.
                    //
                    // `AppState::apply_event` admits a non-`SessionStart`
                    // event only for an agent this process owns (admission
                    // control: an arbitrary same-user process must not be
                    // able to drive daemon session state for an agent the
                    // daemon does not own), and the daemon used to be unable
                    // to answer that for an ordinary pane — it owned one in
                    // `AgentPtyRegistry` and nowhere else, so the real
                    // `dot-agent-deck agent-event` CLI (which emits
                    // `Thinking`/`Idle`/`Working`, never `SessionStart`) had
                    // every one of its reports dropped, `ListAgents` joined
                    // `live = None`, `daemon status` printed
                    // `STATUS=- TOOL=-`, and a TUI reconnect rebuilt the card
                    // as `Idle` (PRD #162).
                    //
                    // The first fix for that inserted `pane_id_env` into
                    // `managed_pane_ids` right here. It was wrong in three
                    // ways, all of them about LIFETIME rather than about this
                    // line: the child can report before this line runs (it is
                    // two `.await`s past the spawn); a child that simply dies
                    // revokes nothing, because the registry marks it `exited`
                    // and `agent_records` then filters it out of the very
                    // lookup `StopAgent` uses to clean up; and every
                    // short-lived pane therefore left an id behind that kept
                    // admitting forged reports. The daemon now installs
                    // `crate::state::AgentOwnership` once at startup and the
                    // registry answers each question as it is asked — from
                    // before the child exists (the spawn reservation) until
                    // its record is reaped. See
                    // `AgentPtyRegistry::owns_generation`.
                    //
                    // WHAT IS ACTUALLY GUARANTEED, stated narrowly because the
                    // wider claim this comment used to make was false. A
                    // NON-`SessionStart` event is admitted only when one of
                    // these holds:
                    //
                    // * it names an `agent_id`, and that GENERATION holds the
                    //   pane it names — live, or retired with the pane not yet
                    //   claimed by anyone else (round 2: pane-scoped was not
                    //   enough, because a pane id is a reusable slot);
                    // * it names NO `agent_id` and some generation holds the
                    //   pane. There is nothing to bind such an event to, and
                    //   PRD #110 / issue #398 keep the shape working
                    //   deliberately, so a same-uid process that omits the id
                    //   can still drive a card on a pane this daemon owns.
                    //   Unchanged by round 2 and stated because it is the
                    //   residual;
                    // * the pane was explicitly registered by this process (an
                    //   orchestration role below, or the auto-registration in
                    //   the next line). Registration is pane-scoped by design —
                    //   the registrant is asserting the pane, not a generation.
                    //
                    // A `SessionStart` is weaker on purpose — `apply_event`
                    // auto-registers a pane id it names, unless the id is the
                    // synthetic `__dead-slot__-…` shape or the registry already
                    // holds a generation for that pane — to cover the TUI
                    // startup race where the hook beats `register_pane`. So a
                    // same-uid process CAN mint a card for a pane NOBODY
                    // spawned by forging one. That is pre-existing, it is not a
                    // cross-user escalation (both sockets are owner-only and
                    // an attach peer can already write to agents directly),
                    // and closing it is tracked separately; it is stated here
                    // rather than papered over.
                    // PRD #93 round-5: populate daemon-side role maps so
                    // `handle_delegate` / `handle_work_done` can resolve
                    // the worker pane and orchestrator pane purely from
                    // daemon state — no TUI round-trip, no broadcast hop.
                    // We do this only for orchestration panes; dashboard
                    // and mode panes don't participate in delegate
                    // dispatch.
                    if let (
                        Some(pane_id),
                        Some(OrchestrationSpawnMeta {
                            name: orch_name,
                            role_name,
                            is_start_role,
                            orchestration_cwd,
                            orchestration_id,
                        }),
                    ) = (pane_id_env.as_deref(), orchestration_meta)
                    {
                        // Round-11 auditor #C: scope the orchestration
                        // identity by `(name, orchestration_cwd)` so
                        // two unnamed orchestrations in different cwds
                        // (`~/a/foo` and `~/b/foo`, both resolving
                        // `name` to "foo") don't collide. The
                        // `orchestration_cwd` is shared across every
                        // role pane in one orchestration tab (round-9
                        // #2: per-pane cwd may diverge, but the
                        // orchestration's identity does not). Older
                        // clients that don't carry the field fall back
                        // to StartAgent.cwd — preserves backwards
                        // compat at the cost of re-opening the
                        // collision; `Some` vs `None` is detectable so
                        // this is documented behavior, not a silent
                        // misroute.
                        let orch_cwd = orchestration_cwd
                            .or_else(|| cwd_for_state.clone())
                            .unwrap_or_default();
                        // PRD #140 M2.0: prefer the per-tab instance token
                        // when the client stamped one. Two tabs of the same
                        // orchestration in the same directory produce
                        // identical `(name, cwd)` pairs, so the tuple alone
                        // cannot tell their panes apart and delegate /
                        // work-done cross-deliver between them (issue #140).
                        // A client predating the token falls back to the
                        // round-11 tuple — same routing behaviour as before,
                        // so old and new clients coexist on one daemon.
                        let identity = match orchestration_id {
                            Some(id) => crate::state::OrchestrationIdentity::Instance {
                                id,
                                name: orch_name,
                            },
                            None => crate::state::OrchestrationIdentity::NameCwd {
                                name: orch_name,
                                cwd: orch_cwd,
                            },
                        };
                        // Shared with the daemon-internal spawn path
                        // (`crate::spawn::spawn`) — see
                        // [`crate::state::AppState::register_orchestration_role`]
                        // for why this must not be inlined again.
                        state.write().await.register_orchestration_role(
                            pane_id,
                            &role_name,
                            is_start_role,
                            identity,
                            cwd_for_state.as_deref(),
                        );
                    }
                    write_resp(&mut stream, &AttachResponse::with_id(id)).await?
                }
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
        AttachRequest::StopAgent { id } => {
            // PRD #93 round-5: capture the agent's `pane_id_env` BEFORE
            // close_agent removes the registry entry, so we can clean up
            // the daemon's per-pane role-map entries after a successful
            // close. Without this, a closed pane's role/cwd would linger
            // in the maps and a subsequent `handle_delegate` aimed at
            // that role would still resolve the dead pane.
            // Capture the agent's record once, BEFORE close_agent removes it.
            // `pane_id_env` cleans up the daemon's per-pane role maps; the
            // record's cwd/orchestration-cwd is how the PRD #120 M2.4 close
            // watcher matches a dispatched issue agent to its worktree.
            //
            // Issue #454: read it through `agent_record_any`, NOT
            // `agent_records`. The latter filters out an agent whose child has
            // already exited — right for hydration, exactly wrong here, because
            // a common way an agent reaches `StopAgent` is that its child died
            // first and the pane is closed afterwards. Through the filtered list
            // such a record came back `None`, so `pane_id_env` was `None`, so
            // EVERY cleanup step below was skipped: the delegation sweep,
            // `cancel_prompt_confirmation`, the role-map removal,
            // `unregister_pane` and the dispatched-worktree cleanup. And it was
            // skipped permanently — `close_agent` drops the entry in this same
            // handler, so no later call could repair it.
            //
            // Round-2 audit (blocker C): reading a DEAD agent's pane back is
            // what makes the cleanup below reachable at all, and it is also
            // what makes it dangerous — because every step after this lookup is
            // PANE-scoped while the agent being stopped is not. The registry
            // deliberately lets a live agent B reuse the pane a dead agent A
            // left; a `StopAgent(A)` arriving after that (a stale client, or
            // just ordinary lifecycle ordering) would then mark B's pane
            // closing, cancel B's prompt confirmation and every delegation
            // touching it, and `unregister_pane` B's role, cwd, orchestrator
            // marker and routing identity. That is strictly worse than the leak
            // it replaced: before the round-1 fix the filtered lookup found
            // nothing for A and cleanup was skipped, so stale state lingered but
            // no LIVE agent's state was deleted.
            //
            // So the pane travels into the cleanup only while it is still A's to
            // give up: nobody else holds it, or A itself still does. Anything
            // else and A owes the pane nothing — its successor owns it, and
            // owns the cleanup of it too. `close_agent(&id)` below is keyed by
            // registry id and is unaffected either way; it is only the
            // pane-scoped work that is gated. The dispatched-worktree cleanup
            // keeps reading the record directly: it is guarded by its own
            // `worktree_still_in_use` sweep over the live records, which is the
            // same "is anyone else using this?" question asked of a worktree
            // instead of a pane.
            //
            // Round-3 review (blocker 1): that gate is now taken from the
            // registry as a HOLD rather than as a boolean read, for two reasons
            // the first version got wrong. It was asked of
            // `pane_current_agent_id`, which sees only published, non-exited
            // agents — so a successor that had RESERVED the pane and not
            // published yet came back `None` and was read as "nobody holds it".
            // And it was check-then-act: decided here, before a `close_agent`
            // that can spend the full three-second termination grace, and acted
            // on afterwards in `unregister_pane`, so a successor could reserve,
            // spawn, publish and register its whole identity inside the gap only
            // for this handler to delete it. The hold answers both — it sees
            // reservations, and nothing may claim the pane until it is dropped
            // at the end of this arm. See `AgentPtyRegistry::hold_pane_for_cleanup`.
            let stopping_record = registry.agent_record_any(&id);
            let pane_cleanup_hold = stopping_record
                .as_ref()
                .and_then(|r| r.pane_id_env.as_deref())
                .and_then(|pane| registry.hold_pane_for_cleanup(pane, &id));
            let pane_id_env = pane_cleanup_hold
                .as_ref()
                .map(|hold| hold.pane_id().to_string());
            let dispatched_worktree = stopping_record
                .as_ref()
                .and_then(crate::issue_dispatch_run::worktree_of_record);
            // PRD #126 M1 review (finding 1) / audit (finding 2): open the
            // race-safe close transition BEFORE terminating the child. This
            // atomically marks the pane closing and drops every outstanding
            // delegation that touches it — as the worker AND as the
            // orchestrator. Three defects close here: the old cancellation ran
            // only AFTER `close_agent`, so a timer firing during the up-to-3s
            // SIGTERM grace window injected the very nudge a deliberate close
            // exists to suppress; it was keyed by worker pane only, so closing
            // an ORCHESTRATOR left every worker's timer armed against a pane id
            // a later, unrelated agent could inherit; and it left a window in
            // which a concurrent `handle_delegate` (holding only the state read
            // guard) could arm after the cancellation, leaving a record nothing
            // would remove. Arming is refused while the mark is set.
            if let Some(pane_id) = pane_id_env.as_deref() {
                let dropped = registry.begin_pane_close(pane_id);
                if !dropped.is_empty() {
                    tracing::debug!(
                        pane_id = %pane_id,
                        dropped = dropped.len(),
                        "StopAgent: dropped outstanding delegations touching the closing pane"
                    );
                }
                // Issue #424 (reviewer finding B9): the same treatment for a
                // spawn-time prompt still being held provisional on this pane.
                // Its guarded re-submissions would refuse anyway once the agent
                // is gone, but a deliberate close should not have to wait out a
                // backoff window to stop being retried into, and the
                // abandonment notice has nowhere left to go.
                crate::spawn::cancel_prompt_confirmation(pane_id);
            }
            // PRD #92 F8 followup (auditor #1): `close_agent` runs the
            // synchronous SIGTERM-with-grace loop in
            // `terminate_child_with_grace_and_wait`, which calls
            // `std::thread::sleep` for up to 3 s while polling the
            // child's `try_wait`. Calling that from inside the async
            // attach-connection task would block a Tokio worker thread
            // for the duration of the grace window — under load this
            // can starve other connections. Mirror the
            // `KIND_SHUTDOWN` handler's pattern: hop the blocking work
            // onto a `spawn_blocking` pool task, await the
            // `JoinHandle`, and surface a join error as a failed
            // close.
            let registry_for_close = registry.clone();
            let id_for_close = id.clone();
            let close_result =
                tokio::task::spawn_blocking(move || registry_for_close.close_agent(&id_for_close))
                    .await;
            let close_outcome: Result<(), String> = match close_result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(join_err) => {
                    tracing::warn!(
                        error = %join_err,
                        agent_id = %id,
                        "spawn_blocking for close_agent panicked or was cancelled"
                    );
                    Err(format!("close_agent task failed: {join_err}"))
                }
            };
            match close_outcome {
                Ok(()) => {
                    if let Some(pane_id) = pane_id_env.as_deref() {
                        // PRD #126: a deliberately closed worker owes nothing.
                        // Without this, closing a stuck worker would still
                        // nag the orchestrator when its timeout expired hours
                        // later, pointing at a pane that no longer exists.
                        // Order matters: take the state write guard and
                        // unregister the pane first, then sweep once more and
                        // only then clear the closing mark, so no interleaving
                        // leaves a record behind.
                        state.write().await.unregister_pane(pane_id);
                        registry.finish_pane_close(pane_id, true);
                    }
                    // PRD #120 M2.4 + S1: if this agent was dispatched into a
                    // per-issue worktree, the tab close is its cleanup trigger.
                    // But a multi-role orchestration shares ONE worktree across
                    // every role pane, so removing it on the first role's close
                    // would nuke it under still-live siblings. `close_agent` has
                    // already dropped the closing agent, so only when NO live
                    // agent still resolves to this worktree was it the LAST one —
                    // then `git worktree remove` the worktree (the clone is
                    // preserved) and drop the registry entry. The child is already
                    // reaped, so the worktree is no longer a live cwd. No-op for
                    // ordinary agents and for earlier sibling-role closes.
                    // PRD #220: the removal POLICY travels with the registry
                    // entry, because this handler serves both producers and sees
                    // only a path — issue-dispatch needs the force removal its
                    // slot-reclaim model depends on, while a PRD #220 dispatch
                    // sibling must keep uncommitted work. See `RemovalPolicy`.
                    //
                    // Cleanup runs DETACHED, and the close is answered without
                    // waiting for it. The agent is already stopped by this point,
                    // so the client's question ("is this pane closed?") is fully
                    // answered — while the cleanup is two `git` invocations
                    // against a worktree an agent has been working in, and
                    // `git status --porcelain` there is seconds, not milliseconds,
                    // on a real checkout. Awaiting it held the response past the
                    // TUI's 5s `CTRL_W_STOP_TIMEOUT`, so the client gave up,
                    // retained the pane "for retry", and the user saw a card that
                    // would not go away even though its agent was gone
                    // (`dispatch/close/001`).
                    if let Some(worktree) = dispatched_worktree {
                        let registry = registry.clone();
                        let worktree_registry = worktree_registry.clone();
                        // Issue #717: the cleanup's verdict is the only
                        // authoritative one — it is measured with the agent
                        // already reaped, so nothing can change the tree under
                        // it — and it lands after the card is gone. Broadcast it
                        // so attached TUIs can say what actually happened
                        // instead of leaving the user with the dialog's
                        // arm-time prediction.
                        let event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            if !crate::issue_dispatch_run::worktree_still_in_use(
                                &registry.agent_records(),
                                &worktree,
                            ) && let Some(entry) = crate::issue_dispatch_run::take_worktree(
                                &worktree_registry,
                                &worktree,
                            ) && let Some(kept) = crate::issue_dispatch_run::remove_worktree(
                                &worktree,
                                &entry.clone_dir,
                                entry.policy,
                            )
                            .await
                            {
                                // Errs only when nothing is subscribed (a
                                // standalone daemon), which is not a failure.
                                let _ = event_tx.send(BroadcastMsg::WorktreeKept(kept));
                            }
                        });
                    }
                    write_resp(&mut stream, &AttachResponse::ok()).await?
                }
                Err(msg) => {
                    // PRD #126: the close failed, so the agent is still live.
                    // Roll the transition back by clearing the closing mark —
                    // future delegates to this pane can arm again. The records
                    // swept at `begin` are deliberately NOT restored: losing a
                    // watch fails safe, resurrecting one could nag about a pane
                    // the user explicitly asked to close.
                    if let Some(pane_id) = pane_id_env.as_deref() {
                        registry.finish_pane_close(pane_id, false);
                    }
                    write_resp(&mut stream, &AttachResponse::err(msg)).await?
                }
            };
            // Issue #454 round 3: released HERE and not one line earlier —
            // everything from the authorisation above through `unregister_pane`
            // and `finish_pane_close` is the pane-scoped cleanup the hold exists
            // to keep valid. Every `?` above releases it too, via `Drop`.
            drop(pane_cleanup_hold);
        }
        AttachRequest::SetAgentLabel {
            id,
            display_name,
            cwd,
        } => match registry.set_agent_label(&id, display_name, cwd) {
            Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::Snapshot { id } => match registry.snapshot(&id) {
            Ok(bytes) => {
                write_resp(&mut stream, &AttachResponse::ok()).await?;
                // Mirror the attach-stream / subscribe-events policy: bound the
                // body and STREAM_END writes with `CLIENT_WRITE_TIMEOUT`. A
                // client that opened a `Snapshot` connection and stopped
                // reading after the OK response could otherwise park this task
                // forever on `write_all` (kernel send buffer fills, the write
                // never completes). On timeout, best-effort STREAM_END with a
                // typed reason and return Ok(()) — a stuck client doesn't
                // justify failing the dispatcher task.
                if !bytes.is_empty()
                    && !write_or_timeout(&mut stream, KIND_STREAM_OUT, &bytes).await
                {
                    let _ = write_or_timeout(&mut stream, KIND_STREAM_END, b"timeout").await;
                    return Ok(());
                }
                if !write_or_timeout(&mut stream, KIND_STREAM_END, &[]).await {
                    return Ok(());
                }
            }
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::AttachStream { id } => {
            handle_attach_stream(stream, registry, id, state.clone()).await?;
        }
        AttachRequest::Resize { id, rows, cols } => match registry.resize(&id, rows, cols) {
            Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::WriteAndSubmit { pane_id, text } => {
            // PRD #20 M3: deliver input honestly. A dashboard-visible session is
            // not necessarily a live, writable target (a wrapped Codex session
            // is history-only), so consult the live session state for the pane
            // and return an honest `SendResult` instead of a fire-and-forget
            // ok(). Only a `Live` target is actually written; a non-live target
            // is reported (`history-only` / `no-live-target`) WITHOUT writing,
            // so the TUI surfaces feedback rather than silently dropping input.
            //
            // PRD #20 R20-003/004/006: the request MAY additionally carry the
            // agent identity + session the prompt was queued for and a stable
            // delivery id. These ride as extra JSON keys alongside the base
            // `WriteAndSubmit { pane_id, text }` shape (the enum stays 2-field so
            // existing literals compile), parsed here from the raw payload. When
            // present the daemon (a) dedups on `delivery_id` so a retry replays
            // the first result instead of re-submitting, (b) binds delivery to
            // the EXACT live registry target and refuses (WrongSession/Stale, no
            // write) on a rebind, and (c) re-validates liveness AFTER acquiring
            // the target's writer, closing the TOCTOU where a pane goes
            // history-only / rebinds while the send waits for the writer.
            // PRD #20 Greptile finding #1 (fail closed on malformed guards): the
            // base `WriteAndSubmit { pane_id, text }` shape already decoded, so
            // the ONLY way this strict re-parse of the SAME payload can fail is a
            // guarded-send identity key (`expected_agent_id` / `expected_session_id`
            // / `delivery_id`) that is PRESENT but has the wrong JSON type (every
            // field is `Option<String>` with `#[serde(default)]`, and unknown keys
            // are ignored). A present-but-malformed guard must REJECT and write
            // nothing, rather than silently dropping every identity check via
            // `unwrap_or_default` and proceeding UNGUARDED (which could reach a
            // rebound pane or double-submit).
            //
            // Issue #608: an ABSENT guard set (a legacy / non-guarded client)
            // still DECODES cleanly to all-`None` — that half is unchanged — but
            // it no longer DEGRADES to pane-only authorization. A paned write
            // that names no agent is now refused with `no-live-target` and
            // writes nothing; see `compute_write_and_submit_outcome`. This
            // comment used to call that degrade "the cross-version fail-safe",
            // which had it backwards: a pane id is a recycled handle, so
            // "deliver to whoever holds this pane now" is exactly the accidental
            // mis-delivery the guarded-send machinery prevents in every other
            // case. The trade is deliberate. What is given up is delivery for an
            // OLD, identity-less client talking to a NEW daemon — and it is
            // given up VISIBLY (`no-live-target`, nothing written) rather than
            // silently into a stranger's conversation. The wire SHAPE is
            // untouched (every field stays `Option<String>` with
            // `#[serde(default)]`, no field added or removed, no new frame
            // kind), so this is a SEMANTIC break behind a stable wire: no
            // `PROTOCOL_VERSION` bump, but a `changelog.d/608.breaking.md`
            // fragment (rule 12 / `docs/develop/versioning.md`).
            let extras: WriteAndSubmitExtras = match serde_json::from_slice(&frame.1) {
                Ok(extras) => extras,
                Err(e) => {
                    write_resp(
                        &mut stream,
                        &AttachResponse::err(format!(
                            "malformed guarded-send identity — refusing unguarded write: {e}"
                        )),
                    )
                    .await?;
                    return Ok(());
                }
            };

            match extras.delivery_id.as_deref() {
                // No idempotency key (legacy / non-guarded caller): compute once,
                // no dedup ledger involvement.
                None => match compute_write_and_submit_outcome(
                    &registry, &state, &pane_id, &text, &extras,
                )
                .await
                {
                    Ok(outcome) => {
                        write_resp(&mut stream, &AttachResponse::with_send_result(outcome)).await?
                    }
                    Err(e) => write_resp(&mut stream, &AttachResponse::err(e)).await?,
                },
                // PRD #20 R20-004 (finding #3): atomic, fingerprint-bound
                // idempotency. Admit the id BEFORE computing so concurrent
                // duplicates serialize on the single-flight lock and replay one
                // result; a retry after a lost response replays the cached
                // delivered/ambiguous result; and reusing the id with a different
                // payload/target is a conflict (never a false success replay).
                Some(did) => {
                    let fingerprint = crate::agent_pty::AgentPtyRegistry::delivery_fingerprint(
                        extras.expected_agent_id.as_deref(),
                        extras.expected_session_id.as_deref(),
                        &pane_id,
                        &text,
                    );
                    match registry.admit_delivery(did, fingerprint).await {
                        crate::agent_pty::DeliveryAdmission::Replay(result) => {
                            write_resp(&mut stream, &AttachResponse::with_send_result(result))
                                .await?
                        }
                        crate::agent_pty::DeliveryAdmission::Conflict => {
                            write_resp(
                                &mut stream,
                                &AttachResponse::err(
                                    "delivery id reused with a conflicting payload/target",
                                ),
                            )
                            .await?
                        }
                        crate::agent_pty::DeliveryAdmission::Proceed(permit) => {
                            match compute_write_and_submit_outcome(
                                &registry, &state, &pane_id, &text, &extras,
                            )
                            .await
                            {
                                Ok(outcome) => {
                                    // Cache a DELIVERED (`applied`/`queued`) or
                                    // AMBIGUOUS outcome; a non-delivery stays
                                    // retryable (see `record_delivery_outcome`).
                                    registry.record_delivery_outcome(&permit, outcome);
                                    write_resp(
                                        &mut stream,
                                        &AttachResponse::with_send_result(outcome),
                                    )
                                    .await?
                                }
                                Err(e) => {
                                    // Clean transport failure (nothing written):
                                    // forget the in-flight record so a retry
                                    // re-attempts, then surface the error.
                                    registry.forget_delivery(&permit);
                                    write_resp(&mut stream, &AttachResponse::err(e)).await?
                                }
                            }
                        }
                    }
                }
            }
        }
        AttachRequest::SubscribeEvents => {
            handle_subscribe_events(stream, event_tx).await?;
        }
        AttachRequest::Hello {
            client_version: _,
            client_build_version,
        } => {
            // PRD #76 M2.21: the daemon never enforces or rejects on
            // `client_version` — we always reply with our own
            // `PROTOCOL_VERSION` and let the caller decide. Centralizing the
            // policy on the client side means a newer client talking to an
            // older daemon (the upgrade-skew direction the daemon can't
            // detect anyway) still gets a sensible mismatch error instead of
            // the daemon rejecting what *would* be its own future shape.
            //
            // PRD #103 M1.2: log the client's build_version when present
            // for post-hoc debugging of mismatch reports. Same server
            // policy — never reject; the laptop decides.
            //
            // `client_build_version` is advisory, not trust-bearing: a
            // hostile or buggy client could embed newlines / ANSI escapes
            // that would corrupt log files or terminal display when an
            // operator tails the log. Pass through `escape_debug` to
            // render any control bytes as printable escapes before
            // formatting. The daemon-side `local_build_id()` is from our
            // own compile-time env and doesn't need the same treatment,
            // but escaping both keeps the log line consistently quoted.
            if let Some(cbv) = client_build_version.as_deref() {
                let daemon_build = crate::build_id::local_build_id();
                let cbv_safe = cbv.escape_debug().to_string();
                let daemon_build_safe = daemon_build.escape_debug().to_string();
                info!(
                    target: "daemon_protocol",
                    "Hello from client build_version=\"{cbv_safe}\" (daemon build_version=\"{daemon_build_safe}\")",
                );
            }
            // PRD #161 M1.1: enumerate the live registry so the reply carries
            // the running-agent summary (count + display names). The Part-A
            // restart prompt and Part-B connect nudge read it to say
            // "N running agents: …" before recycling the daemon. Additive and
            // optional — an older daemon omits it and the client tolerates
            // its absence.
            //
            // PRD #161 FIX 1 test knob: `DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS`
            // makes the reply OMIT `running_agents` (leave it `None`),
            // simulating a pre-#161 daemon so the cross-version None-agents
            // fallback (handshake FIX 1) can be exercised at L2. Gated behind
            // the same `cfg(any(test, debug_assertions))` as
            // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE`, so a shipped release binary
            // compiles the hook out and can never be tricked into hiding its
            // live agents.
            #[cfg(any(test, debug_assertions))]
            let omit_running_agents =
                std::env::var_os("DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS").is_some();
            #[cfg(not(any(test, debug_assertions)))]
            let omit_running_agents = false;
            // PRD #20 finding #6: advertise the guarded-send capability so a new
            // client knows this daemon enforces the identity/idempotency guards
            // and may safely issue an identity-bearing write-and-submit. Its
            // absence (an older daemon) makes the client fail such a send safe.
            // PRD #819 M2/M5: advertise the project verbs explicitly, so a
            // client asks a stable question ("do you know this op?") instead
            // of string-matching serde's `unknown variant` message. Absence
            // means withhold — see `AttachResponse::capabilities`.
            let mut resp = AttachResponse::hello(PROTOCOL_VERSION)
                .with_guarded_send()
                .with_capabilities();
            if !omit_running_agents {
                let summary = RunningAgentsSummary::from_records(&registry.agent_records());
                resp = resp.with_running_agents(summary);
            }
            write_resp(&mut stream, &resp).await?;
        }
        AttachRequest::ReloadSchedules => {
            // PRD #127 M1.3: re-read the global config and diff/replace the
            // registered task set. A bad entry is surfaced via the notifier
            // and skipped; it never fails the reload. Then wake the idle
            // monitor (via the registry's change_notify) so a reload that
            // dropped the last enabled schedule lets the idle gate fire, and
            // one that added a schedule re-arms the carve-out.
            let loaded = crate::config::LoadedSchedules::load();
            scheduler.report_config_errors(&loaded.errors);
            scheduler.reload_apply(
                &loaded.tasks,
                crate::daemon::schedule_callback_factory(
                    registry.clone(),
                    reuse_registry.clone(),
                    worktree_registry.clone(),
                    event_tx.clone(),
                    state.clone(),
                ),
            );
            registry.change_notify().notify_one();
            let names = scheduler.registered_names();
            let mut resp = AttachResponse::ok();
            resp.agents = Some(names);
            write_resp(&mut stream, &resp).await?;
        }
        AttachRequest::RunNow { name } => {
            // PRD #127 M1.5: fire the task now (the `schedule run-now` door).
            // Both started and skipped-still-running mean the task IS
            // registered → ok=true (so `wait_for_schedule_registered` and the
            // CLI treat it as success). PRD #127 C5: surface the started-vs-
            // skipped outcome in `agents` so the caller can report it
            // distinctly; an unknown task → ok=false.
            match scheduler.run_now(&name) {
                Ok(started) => {
                    let token = if started { "started" } else { "skipped" };
                    let mut resp = AttachResponse::ok();
                    resp.agents = Some(vec![token.to_string()]);
                    write_resp(&mut stream, &resp).await?
                }
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
        // Issue #717: read-only preview of what a confirmed close would LEAVE
        // BEHIND, for the close-confirmation dialog. Peeks at the worktree
        // registry and probes `git status --porcelain`; removes nothing, takes
        // no registry entry, and answers `ok = true` with `kept_worktree: None`
        // when the close would leave nothing.
        AttachRequest::DispatchWorktreeClosePreview { pane_ids } => {
            let kept = crate::issue_dispatch_run::kept_worktree_preview(
                &registry.agent_records(),
                &worktree_registry,
                &pane_ids,
                CLOSE_PREVIEW_PROBE_TIMEOUT,
            )
            .await;
            let mut resp = AttachResponse::ok();
            resp.kept_worktree = kept;
            write_resp(&mut stream, &resp).await?
        }
        // PRD #819 M3. `ListProjects` answers from what the daemon already
        // holds — no new persistence, no filesystem browsing — and every
        // candidate is revalidated through the bounded reader before it is
        // offered, because an agent cwd or a scheduler `working_dir` need not be
        // a project at all.
        AttachRequest::ListProjects {} => {
            // Nothing caller-supplied to validate: the enumeration is derived
            // entirely from state the daemon already holds.
            let candidates = project_candidates(&registry, &state, &scheduler).await;
            let resp = match crate::project_resolve::run_bounded(move || {
                crate::project_resolve::resolve_candidates(&candidates)
            })
            .await
            {
                Ok(listing) => {
                    // `ok` even when the listing is empty: "this daemon knows
                    // nothing live" is an answer, not a failure, and it is the
                    // state the client renders its paste-a-path surface for.
                    let mut resp = AttachResponse::ok();
                    resp.projects = Some(listing);
                    resp
                }
                Err(e) => {
                    warn!(reason = %e, "list-projects could not complete");
                    AttachResponse::err(format!(
                        "{PROJECT_ERR_UNRESOLVED}: {}",
                        crate::project_resolve::ProjectResolveError::Internal.detail()
                    ))
                }
            };
            write_resp(&mut stream, &resp).await?
        }
        // PRD #819 M3. Resolve-only: one explicit path in, resolved through the
        // same bounded reader the enumeration uses. No directory walk, no
        // children, no parents, and resolving `/a/b` does not make `/a` or
        // `/a/b/c` known. The refusal's disclosure splits by trust — see
        // [`PROJECT_ERR_UNRESOLVED`].
        AttachRequest::ResolveProject { path } => {
            let resp = match validate_project_path(&path) {
                Err(message) => AttachResponse::err(message),
                Ok(()) => {
                    // The seed set is what decides whether this path gets the
                    // detailed diagnostic. Gathered here, on the async side,
                    // because it is pure in-memory state; the canonicalisation
                    // it is compared against happens inside the blocking half.
                    let seeds = project_candidates(&registry, &state, &scheduler).await;
                    match crate::project_resolve::run_bounded(move || {
                        crate::project_resolve::resolve_for_wire(&path, &seeds)
                    })
                    .await
                    {
                        Ok(Ok(project)) => {
                            let mut resp = AttachResponse::ok();
                            resp.project = Some(project);
                            resp
                        }
                        Ok(Err(refusal)) => AttachResponse::err(refusal),
                        Err(e) => {
                            warn!(reason = %e, "resolve-project could not complete");
                            AttachResponse::err(format!(
                                "{PROJECT_ERR_UNRESOLVED}: {}",
                                crate::project_resolve::ProjectResolveError::Internal.detail()
                            ))
                        }
                    }
                }
            };
            write_resp(&mut stream, &resp).await?
        }
        // PRD #819 M4: the only project verb that writes. Resolve one validated
        // config snapshot, check the revision the client believes it resolved
        // against, find the orchestration, compose the coordinator context and
        // publish it — then report success. Nothing is started here, and a
        // failure at any step returns before the publish, which is what makes
        // "a failed preparation starts no roles" a property of the ordering
        // rather than of a cleanup path.
        //
        // Every field bound, with no `..` rest pattern, so the next field added
        // to the variant is a compile error at this seam rather than a value
        // silently dropped — the mistake `map_tab` in the desktop crate records
        // having made with `orchestration_cwd`.
        AttachRequest::PrepareWorkflow {
            path,
            orchestration,
            task,
            config_revision,
        } => {
            let resp = match refuse_prepare_where_unsupported()
                .and_then(|()| validate_project_path(&path))
                .and_then(|()| validate_task(&task))
            {
                Err(message) => AttachResponse::err(message),
                Ok(()) => {
                    // Same seed set, same reason, as the resolve arm: it is pure
                    // in-memory state and it is what decides whether a refusal
                    // carries the detailed diagnostic.
                    let seeds = project_candidates(&registry, &state, &scheduler).await;
                    // One `run_bounded` call, so the whole resolve → read →
                    // compose → publish sequence runs on ONE blocking thread
                    // under ONE permit. Splitting it would mean acquiring a
                    // second permit from inside work that already holds one,
                    // which is the shape that deadlocks a bounded pool.
                    match crate::project_resolve::run_bounded(move || {
                        crate::project_resolve::prepare_workflow_for_wire(
                            &path,
                            &orchestration,
                            &task,
                            config_revision.as_deref(),
                            &seeds,
                        )
                    })
                    .await
                    {
                        Ok(Ok(prepared)) => {
                            let mut resp = AttachResponse::ok();
                            resp.workflow_prepared = Some(prepared);
                            resp
                        }
                        Ok(Err(refusal)) => AttachResponse::err(refusal),
                        Err(e) => {
                            warn!(reason = %e, "prepare-workflow could not complete");
                            AttachResponse::err(format!(
                                "{PROJECT_ERR_UNRESOLVED}: {}",
                                crate::project_resolve::ProjectResolveError::Internal.detail()
                            ))
                        }
                    }
                }
            };
            write_resp(&mut stream, &resp).await?
        }
        // Unreachable by construction: the normalisation above this `match`
        // rewrote every `StartPreparedAgent` into the `StartAgent` shape plus an
        // explicit token, and nothing between the two can mint one. It is an arm
        // rather than an `unreachable!()` because `handle_connection` serves
        // other clients and a panic would take one connection's bug out on all
        // of them — the same reasoning `PROJECT_ERR_UNIMPLEMENTED` records. If
        // this text is ever observed, the normalisation was edited and the spawn
        // path it feeds is what to read.
        AttachRequest::StartPreparedAgent { .. } => {
            warn!("start-prepared-agent reached the dispatch un-normalised — refusing the spawn");
            write_resp(
                &mut stream,
                &AttachResponse::err(
                    "start-prepared-agent: internal error — the prepared start was not \
                     normalised; nothing was started",
                ),
            )
            .await?
        }
    }
    Ok(())
}

/// PRD #819 M3: the daemon's enumeration seeds, gathered from state it already
/// holds.
///
/// Four sources, and **all four are candidates rather than projects** — an
/// ordinary agent cwd or a scheduled task's working directory need not hold a
/// `.dot-agent-deck.toml` at all, so
/// [`crate::project_resolve::resolve_candidates`] revalidates every one before
/// it is offered:
///
/// 1. the daemon's own startup cwd, captured once in
///    [`crate::daemon::run_daemon_with`];
/// 2. `AgentRecord.cwd` for every live agent;
/// 3. `TabMembership::Orchestration::orchestration_cwd` for every orchestration
///    role;
/// 4. every registered schedule's `working_dir`.
///
/// This function touches **no filesystem**, which is why it runs on the async
/// side: it reads two in-memory registries and the daemon's `AppState`. The
/// `live` join is the same one [`AttachRequest::ListAgents`] performs, and it is
/// here for one reason — `AgentRecord.live` is what carries
/// [`crate::state::SessionSnapshot::last_activity_ms`], the real timestamp the
/// primary nomination prefers over `spawned_at_ms`.
async fn project_candidates(
    registry: &Arc<AgentPtyRegistry>,
    state: &SharedState,
    scheduler: &Arc<crate::scheduler::Scheduler>,
) -> Vec<crate::project_resolve::ProjectCandidate> {
    let mut records = registry.agent_records();
    {
        let guard = state.read().await;
        for record in &mut records {
            record.live = guard
                .sessions
                .values()
                .filter(|s| {
                    s.agent_id.as_deref() == Some(record.id.as_str())
                        && s.pane_id == record.pane_id_env
                })
                .max_by(|a, b| {
                    a.last_activity
                        .cmp(&b.last_activity)
                        .then_with(|| a.session_id.cmp(&b.session_id))
                })
                .map(|s| s.live_snapshot());
        }
    }
    crate::project_resolve::collect_candidates(
        crate::project_resolve::daemon_startup_cwd().as_deref(),
        &records,
        &scheduler.registered_working_dirs(),
    )
}

/// PRD #819 M2/A5: the wire-boundary check every caller-supplied project path
/// passes **before** any filesystem access.
///
/// It reuses [`crate::agent_pty::is_valid_orchestration_cwd`] rather than
/// spelling a second validator, because that predicate is already exactly this
/// shape — non-empty, at most [`crate::agent_pty::CWD_MAX_LEN`] (4096) bytes,
/// free of ASCII control characters, and absolute for this platform — and it is
/// already the rule applied to the `orchestration_cwd` these paths become. A
/// path that survives here and a path the daemon will later accept as an
/// orchestration identity are then the same set by construction.
///
/// Non-UTF-8 needs no check here and is not lossily converted: the frame is
/// JSON and `path` is a `String`, so a non-UTF-8 byte sequence fails the frame
/// decode and never reaches this function. That is a refusal, which is the
/// intended outcome.
///
/// On refusal it returns the message the caller wraps in an
/// [`AttachResponse::err`], and that message names no path. See
/// [`PROJECT_ERR_INVALID_PATH`].
/// PRD #819 audit fix: refuse [`AttachRequest::PrepareWorkflow`] outright where
/// the publish cannot deliver its owner-only, reparse-safe guarantee.
///
/// **First, before the path and the task are even validated.** The refusal is a
/// property of the build rather than of the request, so it must not depend on
/// the request being well-formed — and reporting "invalid path" on a platform
/// where no path would have worked sends the operator after the wrong thing.
///
/// See [`PROJECT_ERR_UNSUPPORTED_PLATFORM`] for why this is a refusal rather
/// than a Windows DACL implementation, and note that the verb is also absent
/// from [`DAEMON_CAPABILITIES`] there, so a client that negotiates never reaches
/// this line.
fn refuse_prepare_where_unsupported() -> Result<(), String> {
    #[cfg(unix)]
    {
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(format!(
            "{PROJECT_ERR_UNSUPPORTED_PLATFORM}: this daemon cannot publish a coordinator context \
             with owner-only permissions on this platform, so preparing a workflow is refused; \
             launch the orchestration from a daemon running on Unix"
        ))
    }
}

/// PRD #819 audit follow-up: the same refusal for
/// [`AttachRequest::StartPreparedAgent`], for the same reason one hop earlier.
///
/// Not a property of the request: [`refuse_prepare_where_unsupported`] refuses
/// every preparation on such a build, and the `PrepareWorkflow` arm is the only
/// production path to [`crate::prep_token::issue`] — so this daemon can never
/// have issued a token, and every prepared start on it is answering for a
/// preparation that could not have happened. `PROJECT_ERR_STALE_TOKEN` would also be true of that and is
/// the wrong sentence — it sends an operator after an expiry or a client bug
/// instead of after the platform. The verb is withheld from
/// [`DAEMON_CAPABILITIES`] there too, so a client that negotiates never reaches
/// this line.
fn refuse_prepared_start_where_unsupported() -> Result<(), String> {
    #[cfg(unix)]
    {
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(format!(
            "{PROJECT_ERR_UNSUPPORTED_PLATFORM}: this daemon cannot prepare a workflow on this \
             platform, so it holds no preparation to start a role against; launch the \
             orchestration from a daemon running on Unix"
        ))
    }
}

fn validate_project_path(path: &str) -> Result<(), String> {
    if crate::agent_pty::is_valid_orchestration_cwd(path) {
        return Ok(());
    }
    Err(format!(
        "{PROJECT_ERR_INVALID_PATH}: project path must be absolute, non-empty, \
         free of control characters, and at most {} bytes",
        crate::agent_pty::CWD_MAX_LEN
    ))
}

/// PRD #819 M2/A5: the wire-boundary bound on
/// [`AttachRequest::PrepareWorkflow`]'s caller-supplied `task`, applied before
/// any filesystem work.
///
/// The bound is [`crate::bounded_read::MAX_TASK_BYTES`] — the constant issue
/// #328 already established and documented for exactly this input class, task
/// prose destined for an agent's prompt. Reusing it keeps one justified number
/// rather than inventing a second. The desktop applies its own tighter 64 KiB
/// check at its UI seam, and that is a client affordance this daemon does not
/// rely on: the audit's requirement is a server-side bound, and a bound that
/// only exists in one client is not one.
///
/// A NUL is refused alongside the length, because the text is destined for a
/// markdown file the daemon writes and the desktop already refuses one at its
/// own seam — so this stays a strict superset of the client's shape check.
fn validate_task(task: &str) -> Result<(), String> {
    if task.len() as u64 > crate::bounded_read::MAX_TASK_BYTES {
        return Err(format!(
            "{PROJECT_ERR_TASK_REJECTED}: task must be at most {} bytes",
            crate::bounded_read::MAX_TASK_BYTES
        ));
    }
    if task.contains('\0') {
        return Err(format!(
            "{PROJECT_ERR_TASK_REJECTED}: task must contain no NUL"
        ));
    }
    Ok(())
}

// CodeRabbit Fix C fixup: bound the response write with `CLIENT_WRITE_TIMEOUT`.
// Every dispatch arm calls `write_resp` first; without a timeout, a same-UID
// client that connected and then stopped reading could pin the dispatcher task
// on this initial OK/Err write (kernel send buffer fills, `write_all` never
// completes). On timeout, surface `io::ErrorKind::TimedOut` so existing `?`
// callers propagate up and let the connection drop.
#[doc(hidden)]
pub async fn write_resp<W: AsyncWrite + Unpin>(w: &mut W, resp: &AttachResponse) -> io::Result<()> {
    let payload = serde_json::to_vec(resp).expect("AttachResponse must serialize");
    match tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write_frame(w, KIND_RESP, &payload)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "write_resp: client did not drain RESP within CLIENT_WRITE_TIMEOUT",
        )),
    }
}

/// Long-lived `SubscribeEvents` handler (PRD #76 M2.17). Confirms the
/// subscription with an OK `RESP`, then forwards every hook
/// [`BroadcastMsg::Event`] from the daemon-wide broadcast as a
/// `KIND_EVENT` frame. Each write is bounded by `CLIENT_WRITE_TIMEOUT`
/// so a wedged client can't pin this task forever. A lagged receiver
/// (the client fell further behind than the broadcast capacity) closes
/// the connection with `KIND_STREAM_END` carrying `"lagged"`; the
/// TUI's reconnect path drains a `list_agents` snapshot to recover.
/// Client disconnect is detected by racing a one-byte read against
/// `rx.recv()` so the broadcast `Receiver` is dropped promptly when
/// the client goes away between messages — otherwise the
/// per-connection task and its receiver would leak for the lifetime
/// of the daemon.
///
/// PRD #93 round-5: orchestration signals (delegate / work-done) used
/// to ride this channel via `BroadcastMsg::Delegate` / `WorkDone`,
/// guarded by a replay buffer (`PendingBroadcasts`), a salvage loop on
/// detach, and a test gate to drive the salvage race. All of that is
/// gone — orchestration prompts now flow directly into target PTYs
/// (see [`AppState::handle_delegate`] /
/// [`AppState::handle_work_done`]) and the surviving PTY scrollback
/// makes a separate replay path unnecessary.
async fn handle_subscribe_events(
    stream: IpcStream,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    let mut rx = event_tx.subscribe();
    let (mut rd, mut wr) = stream.into_split();
    write_resp(&mut wr, &AttachResponse::ok()).await?;

    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(msg) => {
                        let payload = match serde_json::to_vec(&msg) {
                            Ok(b) => b,
                            Err(e) => {
                                // A BroadcastMsg that can't serialize is a daemon
                                // bug — log and skip rather than tear the
                                // subscription down for every other client.
                                warn!("subscribe-events: skipping unserializable broadcast: {e}");
                                continue;
                            }
                        };
                        if !write_or_timeout(&mut wr, KIND_EVENT, &payload).await {
                            let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Daemon's event_tx dropped — daemon is shutting down.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, &[]).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client fell behind beyond EVENT_BROADCAST_CAPACITY.
                        // Tear the subscription down with a typed reason so the
                        // client can drop and reconnect.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"lagged").await;
                        break;
                    }
                }
            }
            // Disconnect detector: the client never writes after the
            // SubscribeEvents request, so any read result here means the
            // socket is gone (EOF / error) or the client is misbehaving.
            // Either way, exit so the receiver drops.
            _ = rd.read_u8() => {
                break;
            }
        }
    }
    Ok(())
}

/// PRD #20 Greptile finding #6: capacity of the per-attach `KIND_STREAM_IN`
/// rejection channel. Bounded so a client that keeps typing into a history-only
/// / view-only target during sustained PTY output — where the output task biases
/// toward `KIND_STREAM_OUT` and drains rejections slowly — cannot grow the
/// daemon's memory without limit. Rejections are coalesceable (the client only
/// needs to learn its input was refused so it can leave its input mode), so a
/// full queue drops the newest reason rather than blocking the input loop.
const REJECT_QUEUE_CAP: usize = 64;

/// Best-effort, NON-BLOCKING enqueue of a typed `KIND_STREAM_IN` rejection reason
/// onto the bounded reject channel (finding #6). On a full queue the reason is
/// dropped and logged — never awaited — so a flooding client can neither
/// back-pressure the input loop nor grow the queue past [`REJECT_QUEUE_CAP`].
fn enqueue_reject(tx: &tokio::sync::mpsc::Sender<Vec<u8>>, reason: &'static [u8]) {
    if tx.try_send(reason.to_vec()).is_err() {
        tracing::debug!(
            target: "pane_write",
            "STREAM_IN rejection dropped — reject queue full (bounded) or closed"
        );
    }
}

async fn handle_attach_stream(
    stream: IpcStream,
    registry: Arc<AgentPtyRegistry>,
    id: String,
    state: SharedState,
) -> io::Result<()> {
    let handle = match registry.subscribe(&id) {
        Ok(h) => h,
        Err(e) => {
            let mut s = stream;
            write_resp(&mut s, &AttachResponse::err(e.to_string())).await?;
            return Ok(());
        }
    };

    let (mut rd, mut wr) = stream.into_split();

    // 1. Confirm the attach succeeded.
    write_resp(&mut wr, &AttachResponse::ok()).await?;
    // 2. Replay the consistent scrollback snapshot before live bytes start
    //    flowing. `subscribe()` guarantees no overlap or gap with the bytes
    //    delivered via `rx` below. The write is bounded by
    //    `CLIENT_WRITE_TIMEOUT` for the same reason live STREAM_OUT writes
    //    are: a client wedged at attach time would otherwise pin this task
    //    forever (kernel send buffer fills, `write_all` never completes,
    //    and the output task never even starts so lag detection can't
    //    fire). On timeout, mirror the output-task policy — best-effort
    //    bounded STREAM_END, then drop the writer and bail.
    if !handle.snapshot.is_empty()
        && !write_or_timeout(&mut wr, KIND_STREAM_OUT, &handle.snapshot).await
    {
        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
        return Ok(());
    }

    let mut rx = handle.rx;
    let writer = handle.writer;
    // PRD #20 R20-008: the target's liveness token, captured ATOMICALLY with the
    // writer at `subscribe` time. The input path re-checks it before every write
    // so a frame arriving during teardown can't reach a dead writer.
    let exited = handle.exited;

    // PRD #128 trace-field-symmetry: the agent's `pane_id_env`, used to enrich
    // the per-frame STREAM_IN trace with `pane_id` and — more importantly — to
    // authorize the write via `pane_writable(pane_id)`.
    //
    // PRD #20 R20-008: this is now the value CAPTURED ON THE HANDLE under the
    // same registry lock as the writer, NOT a separate post-lock
    // `pane_id_env_for_agent` lookup. The old lookup could race a concurrent
    // removal and return the `<agent-gone>` sentinel, and
    // `pane_writable("<agent-gone>")` defaults to `Live` — so a teardown-time
    // frame authorized against `<agent-gone>` could still be written to the
    // cached writer. Using the captured pane id closes that race: it always
    // reflects the exact target the writer belongs to. `<no-pane>` (a daemon-side
    // agent that carried no pane id) can never collide with a real value because
    // `is_valid_pane_id_env` rejects `<` and `>`.
    let pane_id: String = match &handle.pane_id_env {
        Some(s) => s.clone(),
        None => "<no-pane>".to_string(),
    };
    // PRD #20 Greptile finding #3: a daemon-side agent that carried no pane id.
    // `pane_writable("<no-pane>")` can never find its session (that session is
    // stored with `pane_id == None`) and would fall through to the `Live`
    // default — so a history-only / view-only paneless target would still accept
    // `KIND_STREAM_IN`. For such a target the input loop resolves writability by
    // AGENT identity (`agent_writable`) instead, failing closed on a declared
    // non-live session while a paneless target with no declared session keeps the
    // historical `Live` default.
    let is_paneless = pane_id == "<no-pane>";

    // PRD #20 R20-007 (finding #10): the input loop rejects a key/paste frame
    // when the target went non-live/exited/rebound, but `wr` is owned by the
    // output task below — so it hands the typed reason to the output task over
    // this channel, which serializes it onto the wire as a non-terminal
    // `KIND_STREAM_REJECT` frame.
    //
    // PRD #20 Greptile finding #6: the channel is BOUNDED (was unbounded). A
    // client flooding a history-only target with keystrokes while the output task
    // biases toward PTY output would otherwise grow this queue until the daemon
    // exhausts memory. Rejections are coalesceable, so [`enqueue_reject`] drops
    // the newest reason on a full queue instead of awaiting or growing it.
    let (reject_tx, mut reject_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(REJECT_QUEUE_CAP);

    // Output task: forward broadcast bytes → STREAM_OUT frames, and input-loop
    // rejection reasons → STREAM_REJECT frames. Owns `wr` for the duration of
    // streaming.
    //
    // Every write goes through `CLIENT_WRITE_TIMEOUT`. Without it, a client
    // that stops draining its socket pins this task on `write_all` (the
    // kernel send buffer fills and the write never completes) — which also
    // suppresses lag detection, since we can't reach the next `rx.recv()`
    // to observe `RecvError::Lagged`. With the timeout, a wedged client is
    // detected within bounded time and the connection is dropped.
    let output_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                // Bias toward output so heavy PTY traffic isn't starved by
                // the (rare) reject path; both are still serviced.
                biased;
                out = rx.recv() => match out {
                    Ok(bytes) => {
                        if !write_or_timeout(&mut wr, KIND_STREAM_OUT, &bytes).await {
                            // Client wedged or socket error: try one bounded
                            // STREAM_END, then give up. If even STREAM_END
                            // can't get through, dropping `wr` here closes the
                            // socket — the client observes EOF either way.
                            let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Agent terminated (reader thread saw EOF).
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, &[]).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // This subscriber fell behind beyond BROADCAST_CAPACITY.
                        // Better to disconnect than to deliver corrupted ANSI;
                        // the client can reattach and replay scrollback. The
                        // bounded write timeout matters here too: if the client
                        // also wedged its socket, we still need to drop within
                        // a known time rather than block on STREAM_END.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"lagged").await;
                        break;
                    }
                },
                reason = reject_rx.recv() => match reason {
                    // PRD #20 finding #10: a typed, NON-terminal rejection — the
                    // client stays attached (keeps seeing output) but now has an
                    // honest reason to surface and leave its input mode.
                    Some(reason) => {
                        if !write_or_timeout(&mut wr, KIND_STREAM_REJECT, &reason).await {
                            let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
                            break;
                        }
                    }
                    // `reject_tx` dropped (only at teardown, AFTER this task is
                    // aborted — so unreachable in practice). Break rather than
                    // busy-spin on a permanently-ready closed channel.
                    None => break,
                },
            }
        }
    });

    // Input loop: STREAM_IN bytes are forwarded to the shared PTY writer;
    // DETACH (or unknown frame / EOF) ends the loop.
    loop {
        match read_frame(&mut rd).await {
            Ok(Some((KIND_STREAM_IN, bytes))) => {
                use std::io::Write;
                // PRD #20 blocker-6: enforce liveness AUTHORITATIVELY here, not
                // just in the UI. If the focused session became non-live (a
                // wrapped Codex pane that declared `history-only`, or a
                // view-only target), REJECT the key / bracketed-paste frame
                // rather than forwarding it to the PTY — a UI-only gate can't
                // close the race where a pane goes non-live while a stream is
                // already open, nor protect an older/stale client. A pane with
                // no non-live declaration stays `Live` (the historical default),
                // so native PTY panes are unaffected. The connection stays open
                // (the client remains attached, seeing output) — only the write
                // is dropped.
                // PRD #20 R20-008: reject frames to a target that has exited
                // (its reader thread saw EOF) even if the registry entry hasn't
                // been reaped yet — the cached writer points at a dead PTY.
                //
                // PRD #20 R20-007 (finding #10): a rejection is no longer a
                // silent debug-only drop. Send a TYPED, NON-terminal
                // `KIND_STREAM_REJECT` frame (via the output task) so the client
                // can surface honest feedback and leave its input mode for BOTH
                // key and paste, while the stream stays open.
                if exited.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::debug!(
                        target: "pane_write",
                        agent_id = %id,
                        pane_id = %pane_id,
                        payload_len = bytes.len(),
                        "STREAM_IN rejected — target agent has exited"
                    );
                    enqueue_reject(&reject_tx, b"exited");
                    continue;
                }
                // Finding #3: resolve by agent identity for a paneless target, by
                // pane otherwise. Both default to `Live` when nothing is declared,
                // so an ordinary shell is unaffected; a declared non-live target
                // (paneless or paned) fails closed.
                let pre_lock_writable = {
                    let guard = state.read().await;
                    if is_paneless {
                        guard.agent_writable(&id)
                    } else {
                        guard.pane_writable(&pane_id)
                    }
                };
                if pre_lock_writable != crate::event::Writable::Live {
                    tracing::debug!(
                        target: "pane_write",
                        agent_id = %id,
                        pane_id = %pane_id,
                        payload_len = bytes.len(),
                        "STREAM_IN rejected — target is not live (history-only / view-only)"
                    );
                    enqueue_reject(&reject_tx, b"history-only");
                    continue;
                }
                let mut w = writer.lock().await;
                // PRD #20 R20-006 (finding #7): RE-VALIDATE under the held writer,
                // immediately before writing. A close/respawn (registry removal),
                // a liveness transition, or a rebind can land WHILE this frame
                // waited for the writer lock — the pre-lock checks above cannot
                // see that. Re-check exit, the pane's CURRENT live owner (it must
                // still be THIS agent), and writability, holding the writer
                // through the completed write, so no bytes reach a target already
                // declared stale/removed/non-live. All rejections send the same
                // typed frame and keep the stream open.
                if exited.load(std::sync::atomic::Ordering::SeqCst) {
                    drop(w);
                    enqueue_reject(&reject_tx, b"exited");
                    continue;
                }
                // The pane→agent ownership re-check only applies to an agent
                // that actually carries a pane id: a daemon-side agent with no
                // pane (`<no-pane>`) has no pane→agent mapping to re-resolve, and
                // its stream is bound directly to the agent's own writer, so skip
                // the check (it would spuriously find no owner and reject).
                if !is_paneless {
                    match registry.pane_current_agent_id(&pane_id) {
                        Some(current) if current == id => {}
                        Some(_) => {
                            drop(w);
                            enqueue_reject(&reject_tx, b"wrong-session");
                            continue;
                        }
                        None => {
                            drop(w);
                            enqueue_reject(&reject_tx, b"stale");
                            continue;
                        }
                    }
                }
                // Finding #3: re-validate writability under the held writer, by
                // agent identity for a paneless target and by pane otherwise.
                let post_lock_writable = {
                    let guard = state.read().await;
                    if is_paneless {
                        guard.agent_writable(&id)
                    } else {
                        guard.pane_writable(&pane_id)
                    }
                };
                if post_lock_writable != crate::event::Writable::Live {
                    drop(w);
                    enqueue_reject(&reject_tx, b"history-only");
                    continue;
                }
                // PRD #128 (cherry-picked from PR #122): byte-level trace
                // of STREAM_IN frames forwarded to the per-agent PTY
                // writer. Useful for confirming that bytes the TUI
                // queued arrived as distinct frames and that no other
                // path interleaved a write on the same writer mutex
                // between them. Gated by `RUST_LOG=trace`. Emitted
                // INSIDE the writer mutex so trace order matches actual
                // write order. Both `agent_id` and `pane_id` are
                // emitted so the M1.4 diff against the daemon-initiated
                // trace in `AgentPtyRegistry::write_to_pane_internal`
                // can join on either key (`pane_id` is cached once per
                // attach above).
                tracing::trace!(
                    target: "pane_write",
                    source = "stream_in",
                    agent_id = %id,
                    pane_id = %pane_id,
                    payload_len = bytes.len(),
                    payload = %escape_bytes_for_log(&bytes),
                    "STREAM_IN forwarded to PTY writer"
                );
                if w.write_all(&bytes).is_err() {
                    break;
                }
                let _ = w.flush();
                // PRD #127 M2.2: a STREAM_IN frame is a *user* keystroke —
                // stamp the pane's deliver-on-idle debounce clock so a
                // concurrent scheduled reuse fire queues its prompt instead of
                // interrupting active typing. Keyed by `pane_id_env` (the same
                // key the reuse path delivers to).
                //
                // Issue #424 H1 (both reviewers): stamped BEFORE the writer is
                // released, and the `write_all` above stamps it too (the pane
                // writer observes every non-daemon byte — see
                // `crate::agent_pty::PaneWriter`). This used to drop the writer
                // first and stamp afterwards, and a guarded automatic sender
                // queued on that writer acquired it inside the gap, read the
                // stale clock, and submitted the draft that was already
                // physically in the input box. Both halves are deliberate: the
                // writer-level observation is what makes the stamp atomic with
                // respect to handoff, and this call keeps the frame-level
                // contract (including the zero-byte frame the auditor noted)
                // exactly as it was.
                registry.note_user_input(&pane_id);
                drop(w);
            }
            Ok(Some((KIND_DETACH, _))) => {
                // Explicit M2.5 detach: client signalled intent to leave the
                // agent running. Plain socket EOF takes the `Ok(None)` arm
                // below and is intentionally *not* counted as a detach —
                // only voluntary detaches bump the registry counter.
                registry.record_detach();
                break;
            }
            Ok(Some((kind, _))) => {
                warn!("unexpected frame kind 0x{kind:02x} on attach stream — closing");
                break;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Stop the output task; aborting is fine because either we already saw
    // STREAM_END and the loop exited on its own, or we're detaching and the
    // client doesn't expect more bytes.
    output_task.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// PRD #819 audit fix: `PrepareWorkflow` is available exactly where the
    /// publish can deliver its owner-only guarantee, and the capability list
    /// says the same thing the dispatch does.
    ///
    /// **Both halves are asserted against `cfg!(unix)` rather than against a
    /// hardcoded expectation**, which is what makes this one test rather than
    /// two that cannot both run. It pins the property that matters — the gate
    /// and the advertisement agree — so a later change that refuses the verb
    /// while still advertising it, or advertises it while still refusing it,
    /// fails here on whichever platform it is built for. What it deliberately
    /// does **not** do is prove the Windows refusal *text*; only a Windows build
    /// can execute that arm, and `cargo clippy --all-targets` for a Windows
    /// target type-checks it.
    #[test]
    fn prepare_workflow_is_offered_exactly_where_it_is_supported() {
        let gate = refuse_prepare_where_unsupported();
        let advertised = DAEMON_CAPABILITIES.contains(&CAP_PREPARE_WORKFLOW);
        assert_eq!(
            gate.is_ok(),
            advertised,
            "the dispatch gate and the advertised capability set must agree; gate = {gate:?}, \
             advertised = {advertised}"
        );
        assert_eq!(
            advertised,
            cfg!(unix),
            "the verb is Unix-only because the publish's mode bits and its \
             `O_NOFOLLOW | O_DIRECTORY` open are"
        );
        // The read-only verbs are unaffected — the refusal is about the write.
        assert!(DAEMON_CAPABILITIES.contains(&CAP_LIST_PROJECTS));
        assert!(DAEMON_CAPABILITIES.contains(&CAP_RESOLVE_PROJECT));
        if let Err(message) = gate {
            assert!(
                message.starts_with(PROJECT_ERR_UNSUPPORTED_PLATFORM),
                "the refusal must carry the stable code, got {message:?}"
            );
        }
    }

    /// Issue #454, the root cause pinned at its own seam: a `StartAgent` for an
    /// ORDINARY dashboard pane — no `tab_membership`, so none of the
    /// orchestration role-map machinery runs — must leave the daemon's
    /// `AppState` able to accept that pane's lifecycle reports.
    ///
    /// Until the fix, `managed_pane_ids` was the only answer the daemon had and
    /// it was populated only by `register_orchestration_role` and by
    /// `apply_event`'s auto-register-on-`SessionStart` branch, so this pane
    /// lived in `AgentPtyRegistry` and nowhere else. `AppState::apply_event`
    /// then rejected every non-`SessionStart` report the pane made — which is
    /// every report the real `dot-agent-deck agent-event` CLI sends — and
    /// `ListAgents` had no live session to join onto the record.
    ///
    /// Two negatives are just as load-bearing. Owning the pane must NOT make a
    /// dashboard pane look like an orchestration role, or `handle_delegate`
    /// would start resolving panes that never joined an orchestration. And the
    /// daemon must NOT record the pane in `managed_pane_ids`: an entry there
    /// survives the child's death (nothing reports it) and would keep admitting
    /// reports for a pane with no process behind it.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_ordinary_daemon_spawned_panes_reports_are_admitted() {
        use crate::daemon_client::{DaemonClient, StartAgentOptions};

        let dir = tempfile::tempdir().expect("tempdir for the attach socket");
        let sock = dir.path().join("attach.sock");
        let registry = Arc::new(AgentPtyRegistry::new());
        let (event_tx, _rx) = broadcast::channel(16);
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));

        let server = {
            let sock = sock.clone();
            let registry = registry.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let _ = run_attach_server_with_counter(
                    &sock,
                    registry,
                    event_tx,
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    state,
                )
                .await;
            })
        };
        // The bind happens inside the task, so wait for it to accept.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::net::UnixStream::connect(&sock).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "attach socket never came up at {}",
                sock.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pane_id = "dashboard-pane-454";
        let agent_id = DaemonClient::new(sock.clone())
            .start_agent(StartAgentOptions {
                command: Some("cat".to_string()),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                // Deliberately no `tab_membership`: this is a plain dashboard
                // pane, the case the old code registered nowhere.
                ..StartAgentOptions::default()
            })
            .await
            .expect("spawn an ordinary pane through the attach socket");

        // The registry owns the pane — that is the fact the admission check now
        // consults. Nothing was copied into `AppState` to make it true.
        assert!(
            registry
                .agent_records()
                .iter()
                .any(|r| r.id == agent_id && r.pane_id_env.as_deref() == Some(pane_id)),
            "precondition: the registry owns the spawned pane"
        );
        {
            let mut guard = state.write().await;
            assert!(
                !guard.managed_pane_ids.contains(pane_id),
                "an ordinary pane needs no entry in the daemon's registered set: \
                 an entry there could not be revoked when the child died and \
                 would keep admitting reports for a pane with no process behind \
                 it; managed={:?}",
                guard.managed_pane_ids
            );
            assert!(
                !guard.pane_role_map.contains_key(pane_id),
                "owning a dashboard pane must not give it an orchestration ROLE"
            );
            assert!(
                !guard.orchestrator_pane_ids.contains(pane_id),
                "owning a dashboard pane must not make it an orchestrator"
            );
            // The property that actually matters, exercised end to end through
            // the daemon's own `AppState`: the lifecycle report a real
            // `dot-agent-deck agent-event --type running` sends is ADMITTED, and
            // carries the ids `ListAgents` joins on.
            guard.apply_event(thinking_event_454(pane_id, &agent_id));
            let session = guard
                .sessions
                .values()
                .find(|s| s.pane_id.as_deref() == Some(pane_id))
                .expect("a `Thinking` report for a daemon-spawned pane must be admitted");
            assert_eq!(session.agent_id.as_deref(), Some(agent_id.as_str()));
            assert_eq!(session.status, crate::state::SessionStatus::Thinking);
        }

        registry.shutdown_all();
        server.abort();
    }

    /// The payload the real `dot-agent-deck agent-event --type running` CLI puts
    /// on the hook socket (`Commands::AgentEvent` in `main.rs`): a bare
    /// `AgentEvent`, `EventType::Thinking`, a pane-derived session id, and the
    /// `DOT_AGENT_DECK_PANE_ID` / `DOT_AGENT_DECK_AGENT_ID` pair the daemon
    /// injected into the spawned pane. Never a `SessionStart` — which is the
    /// whole reason admission decided issue #454.
    #[cfg(unix)]
    fn thinking_event_454(pane_id: &str, agent_id: &str) -> crate::event::AgentEvent {
        crate::event::AgentEvent {
            session_id: format!("{pane_id}-session"),
            agent_type: crate::event::AgentType::Pi,
            event_type: crate::event::EventType::Thinking,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: Default::default(),
            pane_id: Some(pane_id.to_string()),
            agent_id: Some(agent_id.to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        }
    }

    /// Issue #454 review, item 3: a child that exits on its own revokes
    /// admission, and a later `StopAgent` can still clean up after it.
    ///
    /// Both halves failed before. Admission failed because the pane id had been
    /// copied into the daemon's registered set at spawn and only `StopAgent`
    /// removed it — so `dead-pane` stayed admissible for as long as the daemon
    /// lived, while the registry separately allowed an unrelated later spawn to
    /// REUSE that id. Cleanup failed because `StopAgent` read the stopping
    /// agent's `pane_id_env` out of `agent_records()`, which filters exited
    /// entries: for a child that had already died it read `None` and skipped
    /// every cleanup step — permanently, since the same handler then dropped the
    /// registry entry.
    ///
    /// `/usr/bin/true` is the shortest possible version of the normal death
    /// path, not a contrived one: any agent whose process ends before its pane
    /// is closed goes through exactly this.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_naturally_exited_pane_is_disowned_and_still_cleaned_up_on_stop() {
        use crate::daemon_client::{DaemonClient, StartAgentOptions};

        let dir = tempfile::tempdir().expect("tempdir for the attach socket");
        let sock = dir.path().join("attach.sock");
        let registry = Arc::new(AgentPtyRegistry::new());
        let (event_tx, _rx) = broadcast::channel(16);
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));

        let server = {
            let sock = sock.clone();
            let registry = registry.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let _ = run_attach_server_with_counter(
                    &sock,
                    registry,
                    event_tx,
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    state,
                )
                .await;
            })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::net::UnixStream::connect(&sock).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "attach socket never came up at {}",
                sock.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // A role pane, so the role-map cleanup `StopAgent` owes is observable
        // too — that is the half `agent_records()`' exited filter silently
        // skipped.
        let pane_id = "dead-pane-454";
        let client = DaemonClient::new(sock.clone());
        let agent_id = client
            .start_agent(StartAgentOptions {
                command: Some("/usr/bin/true".to_string()),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                tab_membership: Some(crate::agent_pty::TabMembership::Orchestration {
                    name: "dead-orch-454".to_string(),
                    role_index: 0,
                    role_name: "worker".to_string(),
                    is_start_role: false,
                    orchestration_cwd: Some(dir.path().to_string_lossy().into_owned()),
                    display_title: None,
                    orchestration_id: Some("orch-454".to_string()),
                }),
                ..StartAgentOptions::default()
            })
            .await
            .expect("spawn a short-lived role pane through the attach socket");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.live_count() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the child never exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Half one — natural exit WITHOUT a stop. The role registration still
        // stands (nothing has taken it back yet), and the retired generation
        // still owns its OWN pane, because its final report can still be in
        // flight (round-2 reviewer blocker B). What it does NOT do is speak for
        // anyone else's id, and once the role entry AND the registry record are
        // gone below, the dead pane admits nothing at all.
        assert!(
            registry.generation_ownership(Some(pane_id), Some(&agent_id))
                == crate::state::Ownership::Owned,
            "a retired generation still owns its own pane until something \
             claims or reaps it"
        );
        assert!(
            registry.generation_ownership(Some(pane_id), Some("some-other-id-454"))
                == crate::state::Ownership::Unclaimed,
            "but it does not make the pane a bearer token for any other id"
        );

        // Half two — the later `StopAgent` still finds the metadata it needs.
        client
            .stop_agent(&agent_id)
            .await
            .expect("stop the already-dead agent");

        {
            let mut guard = state.write().await;
            assert!(
                !guard.managed_pane_ids.contains(pane_id),
                "StopAgent must unregister a naturally-exited agent's pane; \
                 managed={:?}",
                guard.managed_pane_ids
            );
            assert!(
                !guard.pane_role_map.contains_key(pane_id),
                "StopAgent must take back the dead pane's ROLE, or a later \
                 delegate still resolves it"
            );
            guard.apply_event(thinking_event_454(pane_id, &agent_id));
            assert!(
                guard.sessions.is_empty(),
                "a report naming the dead pane must be refused; sessions={:?}",
                guard.sessions.keys().collect::<Vec<_>>()
            );
        }

        registry.shutdown_all();
        server.abort();
    }

    /// Round-2 audit, blocker C — and a regression the round-1 fix INTRODUCED,
    /// which is why it is worth a test of its own rather than a clause in the
    /// one above.
    ///
    /// `agent_record_any` deliberately resolves a dead agent's pane so that
    /// `StopAgent` can clean up after a child that died on its own. But every
    /// cleanup step downstream of that lookup is PANE-scoped while the agent
    /// being stopped is not, and the registry explicitly permits a live agent to
    /// reuse a dead one's pane id. So: A dies, B takes pane P, a stale
    /// `StopAgent(A)` arrives — and the handler marked P closing, cancelled
    /// every delegation touching it, and `unregister_pane`d B's role, cwd,
    /// orchestrator marker and routing identity. Reachable through ordinary
    /// lifecycle ordering, not only a malicious client.
    ///
    /// On `main` this could not happen for the opposite reason: the filtered
    /// lookup found nothing for A, so cleanup was skipped entirely. Stale state
    /// leaked, but no LIVE agent's state was ever deleted. The fix has to keep
    /// the first behaviour without buying the second, so the pane travels into
    /// the cleanup only while it is still A's to give up.
    ///
    /// The ORDER is the finding: A exits, B claims the pane, and only then is A
    /// stopped.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_retired_agent_leaves_its_panes_new_occupant_alone() {
        use crate::daemon_client::{DaemonClient, StartAgentOptions};

        let dir = tempfile::tempdir().expect("tempdir for the attach socket");
        let sock = dir.path().join("attach.sock");
        let registry = Arc::new(AgentPtyRegistry::new());
        let (event_tx, _rx) = broadcast::channel(16);
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));

        let server = {
            let sock = sock.clone();
            let registry = registry.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let _ = run_attach_server_with_counter(
                    &sock,
                    registry,
                    event_tx,
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    state,
                )
                .await;
            })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::net::UnixStream::connect(&sock).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "attach socket never came up at {}",
                sock.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pane_id = "handover-pane-454";
        let client = DaemonClient::new(sock.clone());
        let role_spawn = |command: &str, role: &str| {
            let dir_path = dir.path().to_string_lossy().into_owned();
            StartAgentOptions {
                command: Some(command.to_string()),
                cwd: Some(dir_path.clone()),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                tab_membership: Some(crate::agent_pty::TabMembership::Orchestration {
                    name: "handover-orch-454".to_string(),
                    role_index: 0,
                    role_name: role.to_string(),
                    is_start_role: false,
                    orchestration_cwd: Some(dir_path),
                    display_title: None,
                    orchestration_id: Some("orch-handover-454".to_string()),
                }),
                ..StartAgentOptions::default()
            }
        };

        // Generation A takes the pane and immediately dies.
        let old_id = client
            .start_agent(role_spawn("/usr/bin/true", "worker-a"))
            .await
            .expect("spawn the first role pane");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.live_count() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first child never exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Generation B takes the pane over — which the registry allows and the
        // daemon re-registers under B's own role.
        let new_id = client
            .start_agent(role_spawn("/bin/sh", "worker-b"))
            .await
            .expect("the pane must be reusable once its child is gone");
        assert_eq!(
            registry.pane_current_agent_id(pane_id).as_deref(),
            Some(new_id.as_str()),
            "precondition: the live agent on the pane is the second generation"
        );

        // B has work outstanding against it, which is what the pane-scoped
        // cleanup would sweep.
        let armed = registry
            .arm_outstanding_delegation(
                pane_id,
                "worker-b",
                "orchestrator-pane-454",
                "orchestrator-agent-454",
                None,
            )
            .expect("precondition: arming a delegation on the live pane must succeed");

        // Only now does the stale stop for the RETIRED generation arrive.
        client
            .stop_agent(&old_id)
            .await
            .expect("stop the already-dead first generation");

        assert_eq!(
            registry.pane_current_agent_id(pane_id).as_deref(),
            Some(new_id.as_str()),
            "stopping a retired generation must not touch the live agent that \
             took its pane"
        );
        assert!(
            registry
                .take_outstanding_delegation_if(pane_id, armed.seq)
                .is_some(),
            "the live agent's outstanding delegation must survive — the \
             pane-scoped `begin_pane_close` sweep would have cancelled it, \
             silently dropping in-flight orchestration work"
        );
        assert!(
            !registry.is_pane_closing(pane_id),
            "the live agent's pane must not be left marked closing"
        );

        {
            let guard = state.read().await;
            assert!(
                guard.managed_pane_ids.contains(pane_id),
                "the live agent's pane must stay registered; managed={:?}",
                guard.managed_pane_ids
            );
            assert_eq!(
                guard.pane_role_map.get(pane_id).map(String::as_str),
                Some("worker-b"),
                "the live agent's ROLE must survive — losing it means every \
                 later delegate to `worker-b` resolves nothing"
            );
            assert!(guard.pane_cwd_map.contains_key(pane_id), "…and its cwd");
            assert!(
                guard.pane_orchestration_map.contains_key(pane_id),
                "…and its routing identity"
            );
        }

        registry.shutdown_all();
        server.abort();
    }

    /// Round-3 review, blocker 1 — the same finding as the test above, one
    /// ordering earlier, which is the ordering the fix for it MISSED.
    ///
    /// The gate that decided whether the stopping agent still owed its pane any
    /// cleanup asked `pane_current_agent_id`, and that lookup sees only
    /// PUBLISHED, non-exited agents. A successor that has RESERVED the pane and
    /// not published yet — the window `spawn_agent` occupies between taking its
    /// reservation and inserting its record, which every spawn passes through —
    /// therefore came back `None`, and `None` was read as "nobody holds this
    /// pane, so it is still mine to give up". The handler then marked the pane
    /// closing, cancelled every delegation touching it, and `unregister_pane`d
    /// the pane's role, cwd, orchestrator marker and routing identity, all of
    /// which the successor was in the middle of claiming.
    ///
    /// The sibling above places the successor fully LIVE before the stop, so it
    /// catches neither this ordering nor the check-then-act one (the successor
    /// reserving after the gate but before `unregister_pane`, across a
    /// `close_agent` that can spend the full three-second termination grace).
    /// Both are answered the same way, by the durable hold whose own two
    /// properties are pinned in `crate::agent_pty`'s tests; this one pins that
    /// the HANDLER actually takes it.
    ///
    /// What survives here is the predecessor's own registration, because the
    /// successor has not published yet — and that is the correct outcome, not a
    /// leak: the pane is the successor's, so the successor's `StartAgent`
    /// overwrites every one of these entries the moment it publishes, and its
    /// own close is what takes them back.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_retired_agent_leaves_a_pane_its_successor_has_reserved_alone() {
        use crate::daemon_client::{DaemonClient, StartAgentOptions};

        let dir = tempfile::tempdir().expect("tempdir for the attach socket");
        let sock = dir.path().join("attach.sock");
        let registry = Arc::new(AgentPtyRegistry::new());
        let (event_tx, _rx) = broadcast::channel(16);
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));

        let server = {
            let sock = sock.clone();
            let registry = registry.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let _ = run_attach_server_with_counter(
                    &sock,
                    registry,
                    event_tx,
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    state,
                )
                .await;
            })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::net::UnixStream::connect(&sock).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "attach socket never came up at {}",
                sock.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pane_id = "reserved-handover-pane-454";
        let client = DaemonClient::new(sock.clone());
        let dir_path = dir.path().to_string_lossy().into_owned();
        let old_id = client
            .start_agent(StartAgentOptions {
                command: Some("/usr/bin/true".to_string()),
                cwd: Some(dir_path.clone()),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                tab_membership: Some(crate::agent_pty::TabMembership::Orchestration {
                    name: "reserved-orch-454".to_string(),
                    role_index: 0,
                    role_name: "worker-a".to_string(),
                    is_start_role: false,
                    orchestration_cwd: Some(dir_path),
                    display_title: None,
                    orchestration_id: Some("orch-reserved-454".to_string()),
                }),
                ..StartAgentOptions::default()
            })
            .await
            .expect("spawn the first role pane");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.live_count() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first child never exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The successor is mid-spawn: it holds the pane's reservation and has
        // not published a record yet.
        registry.reserve_pane_for_test("successor-454", pane_id);
        assert!(
            registry.pane_current_agent_id(pane_id).is_none(),
            "precondition: the old gate's question answers None in exactly this \
             state, which is why it authorised the cleanup"
        );

        // Work outstanding against the pane — what the pane-scoped
        // `begin_pane_close` sweep would silently drop.
        let armed = registry
            .arm_outstanding_delegation(
                pane_id,
                "worker-a",
                "orchestrator-pane-454",
                "orchestrator-agent-454",
                None,
            )
            .expect("precondition: arming a delegation on the pane must succeed");

        client
            .stop_agent(&old_id)
            .await
            .expect("stop the already-dead first generation");

        assert!(
            registry
                .take_outstanding_delegation_if(pane_id, armed.seq)
                .is_some(),
            "the pane's outstanding delegation must survive — `begin_pane_close` \
             would have cancelled it on behalf of an agent that no longer owns \
             the pane"
        );
        assert!(
            !registry.is_pane_closing(pane_id),
            "the successor's pane must not be left marked closing"
        );
        {
            let guard = state.read().await;
            assert!(
                guard.managed_pane_ids.contains(pane_id),
                "a pane a successor has already reserved must not be \
                 unregistered by its predecessor's close; managed={:?}",
                guard.managed_pane_ids
            );
            assert_eq!(
                guard.pane_role_map.get(pane_id).map(String::as_str),
                Some("worker-a"),
                "…nor may its ROLE be taken back: the successor's `StartAgent` \
                 is about to overwrite this entry, and deleting it afterwards \
                 leaves every delegate to that role resolving nothing"
            );
            assert!(guard.pane_cwd_map.contains_key(pane_id), "…nor its cwd");
            assert!(
                guard.pane_orchestration_map.contains_key(pane_id),
                "…nor its routing identity"
            );
        }

        registry.shutdown_all();
        server.abort();
    }

    /// PRD #20 Greptile finding #6 (coder-authored): the per-attach STREAM_IN
    /// rejection channel must be BOUNDED so a client flooding a history-only /
    /// view-only target with keystrokes — while the output task biases toward PTY
    /// output and drains rejections slowly — cannot grow the daemon's memory
    /// without limit. Flood the channel far past its capacity WITHOUT draining
    /// (modeling exactly that backlog) and prove: every enqueue returns without
    /// blocking, and the queue never holds more than [`REJECT_QUEUE_CAP`] items —
    /// the flood is dropped/coalesced, not buffered. Before the fix the channel
    /// was `unbounded_channel`, so this backlog would hold all
    /// `REJECT_QUEUE_CAP * 100` items instead of the cap.
    #[tokio::test]
    async fn reject_queue_is_bounded_under_flood() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(REJECT_QUEUE_CAP);
        // A flooding client keeps typing into a non-live target; the receiver
        // (output task) is busy with PTY output and never drains here.
        let flood = REJECT_QUEUE_CAP * 100;
        for _ in 0..flood {
            // `enqueue_reject` must be strictly non-blocking; if it ever awaited
            // or panicked on a full queue, this loop would hang or fail.
            enqueue_reject(&tx, b"history-only");
        }
        let mut buffered = 0usize;
        while rx.try_recv().is_ok() {
            buffered += 1;
        }
        assert!(
            buffered <= REJECT_QUEUE_CAP,
            "reject queue grew past its bound: buffered {buffered} > cap {REJECT_QUEUE_CAP} \
             (a flood of {flood} rejections must be coalesced/dropped, not buffered)"
        );
        assert_eq!(
            buffered, REJECT_QUEUE_CAP,
            "an undrained flood should fill the bounded queue exactly to its cap"
        );
    }

    /// PRD #20 Greptile P1 (daemon_protocol.rs:988) — attach-after-check
    /// barrier, closing the stale-pre-lock-snapshot class for `has_live_attach`.
    /// The attach flag used to be sampled BEFORE `write_and_submit_guarded`
    /// acquired the target writer, then consulted in the post-lock re-validation
    /// closure. If the pane became attached WHILE the send waited for that
    /// writer, the closure saw the stale (pre-lock) "unattached" value and let a
    /// stale prompt — whose named session no longer exists (`pane_hook_session_id`
    /// is `None`) — slip into the freshly-attached conversation instead of
    /// rejecting it.
    ///
    /// This mirrors `guarded_send_rejects_agent_removal_after_writer_lock`: it
    /// holds the EXACT target writer so a guarded send parks AFTER its pre-lock
    /// checks but BEFORE the write, makes the pane become attached during that
    /// window, then releases the writer. Because the fix samples attachment
    /// INSIDE the post-lock closure (one delivery-time snapshot), the send must
    /// observe the NEW attached state and reject with `Stale` — no bytes into the
    /// new conversation. It pins the pre-lock reading as `false` and the post-lock
    /// reading as `true`, so the closure is provably the single source of truth:
    /// had the stale pre-lock value been trusted, the outcome would be `Applied`.
    ///
    /// PRD #42 build-windows: this test spawns a real PTY running `/bin/sh`,
    /// which does not exist on Windows, so — like its sibling
    /// `guarded_send_rejects_agent_removal_after_writer_lock` (whole
    /// `agent_pty::spawn_tests` module is `#[cfg(all(test, unix))]`) — it is
    /// gated to Unix. The module here is mixed cross-platform, so the gate is
    /// on the function itself (already inside `#[cfg(test)]`). No Unix coverage
    /// is lost: the fast tier still exercises it green on Unix.
    #[cfg(unix)]
    #[tokio::test]
    async fn guarded_send_rechecks_live_attach_after_writer_lock() {
        let reg = Arc::new(AgentPtyRegistry::new());
        let pane_id = "pane-attach-after-check-barrier";
        let id = reg
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn agent");

        // State: the pane is registered but carries NO session, so `pane_writable`
        // defaults to `Live` (the send enters the guarded path) while
        // `pane_hook_session_id` is `None` (the named generation is gone). With an
        // `expected_session_id` supplied, delivery then hinges ENTIRELY on whether
        // the pane is attached at DELIVERY time — isolating the attach re-check.
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(pane_id.to_string());

        // Grab the EXACT writer the guarded send will contend for WITHOUT staying
        // subscribed: `subscribe` bumps `receiver_count`, so keep only `.writer`
        // and let the rest of the handle (its receiver) drop — returning the pane
        // to UNATTACHED for the pre-lock reading.
        let writer = reg.subscribe(&id).expect("subscribe for writer").writer;
        assert!(
            !reg.pane_has_live_attach(pane_id),
            "precondition: pane must be UNATTACHED before the send parks (pre-lock reading)"
        );
        let guard = writer.lock().await;

        let reg_task = reg.clone();
        let state_task = state.clone();
        let pane = pane_id.to_string();
        let extras = WriteAndSubmitExtras {
            expected_agent_id: Some(id.clone()),
            expected_session_id: Some("queued-generation".to_string()),
            ..Default::default()
        };
        let mut task = tokio::spawn(async move {
            compute_write_and_submit_outcome(
                &reg_task,
                &state_task,
                &pane,
                "printf 'ATTACHED-AFTER-CHECK\\n'",
                &extras,
            )
            .await
        });

        // Precondition: the send is parked on the held writer — past its pre-lock
        // checks, not yet written.
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut task)
                .await
                .is_err(),
            "precondition: guarded send must block on the held writer"
        );

        // The pane becomes ATTACHED while the send waits for the writer. Keep the
        // handle alive so `receiver_count > 0` for the post-lock reading.
        let _attach = reg.subscribe(&id).expect("attach during writer wait");
        assert!(
            reg.pane_has_live_attach(pane_id),
            "the pane must now read as ATTACHED (post-lock reading differs from pre-lock)"
        );

        // Release the writer; the guarded send now runs its post-lock closure.
        drop(guard);

        let result = task.await.expect("join guarded-send task");
        assert_eq!(
            result,
            Ok(crate::event::SendResult::Stale),
            "a pane attached after the pre-lock check must be re-evaluated post-lock: with \
             its named session gone the stale prompt is refused as Stale (had the pre-lock \
             'unattached' value been trusted, it would have been Applied)"
        );

        drop(_attach);
        reg.shutdown_all();
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, KIND_STREAM_OUT, b"hello")
            .await
            .unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, payload) = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(kind, KIND_STREAM_OUT);
        assert_eq!(payload, b"hello");
    }

    #[tokio::test]
    async fn frame_eof_returns_none() {
        let buf: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn frame_partial_header_returns_err() {
        // 1, 2, 3, 4 bytes followed by EOF must each be reported as a
        // truncated frame (Err), not a clean disconnect (Ok(None)). Only
        // 0-bytes-then-EOF is a clean disconnect.
        for n in 1usize..=4 {
            let buf: Vec<u8> = vec![0u8; n];
            let mut cursor = std::io::Cursor::new(buf);
            let err = read_frame(&mut cursor)
                .await
                .expect_err(&format!("expected Err for {n}-byte partial header"));
            assert_eq!(
                err.kind(),
                io::ErrorKind::UnexpectedEof,
                "wrong error kind for {n}-byte partial header"
            );
        }
    }

    #[tokio::test]
    async fn frame_partial_body_returns_err() {
        // Header claims 16 bytes of payload; only 5 supplied before EOF.
        // The body read must fail as truncated.
        let mut buf: Vec<u8> = Vec::new();
        buf.push(KIND_STREAM_OUT);
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"hello"); // 5 bytes — short
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor)
            .await
            .expect_err("expected Err for truncated body");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn frame_zero_length_payload() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, KIND_STREAM_END, &[]).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, payload) = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(kind, KIND_STREAM_END);
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn frame_rejects_oversize() {
        // Hand-crafted header claiming 32 MiB payload — must be rejected
        // before any allocation happens.
        let mut buf: Vec<u8> = vec![KIND_STREAM_OUT];
        buf.extend_from_slice(&((MAX_FRAME_LEN as u32 + 1).to_be_bytes()));
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_serde_round_trip() {
        let req = AttachRequest::StartAgent {
            command: Some("/bin/sh".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("FOO".into(), "BAR".into())],
            display_name: Some("auditor".into()),
            tab_membership: None,
            agent_type: None,
            seed: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::StartAgent {
                command,
                env,
                display_name,
                tab_membership,
                ..
            } => {
                assert_eq!(command.as_deref(), Some("/bin/sh"));
                assert_eq!(env, vec![("FOO".to_string(), "BAR".to_string())]);
                assert_eq!(display_name.as_deref(), Some("auditor"));
                assert!(tab_membership.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn start_agent_omits_display_name_when_none() {
        // Forward compat: older daemons must accept a StartAgent payload
        // that doesn't carry `display_name`, and the field must not be
        // present in JSON when it's None.
        let req = AttachRequest::StartAgent {
            command: Some("/bin/sh".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![],
            display_name: None,
            tab_membership: None,
            agent_type: None,
            seed: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("display_name"),
            "display_name=None should be omitted from the wire payload"
        );
        assert!(
            !v.as_object().unwrap().contains_key("tab_membership"),
            "tab_membership=None should be omitted from the wire payload"
        );
        assert!(
            !v.as_object().unwrap().contains_key("agent_type"),
            "agent_type=None should be omitted from the wire payload"
        );
        // PRD #201: the additive `seed` field is `skip_serializing_if`, so a
        // no-seed StartAgent keeps the exact legacy wire shape an older daemon
        // parses — the reason `get-seed`/`seed` need no `PROTOCOL_VERSION` bump.
        assert!(
            !v.as_object().unwrap().contains_key("seed"),
            "seed=None should be omitted from the wire payload"
        );
    }

    #[test]
    fn start_agent_with_mode_tab_membership_round_trip() {
        // PRD #76 M2.12: tab_membership round-trips through the wire format
        // and survives `serde_json::from_str` on a foreign client.
        let req = AttachRequest::StartAgent {
            command: Some("claude".into()),
            cwd: Some("/work".into()),
            rows: 24,
            cols: 80,
            env: vec![],
            display_name: Some("k8s-ops".into()),
            tab_membership: Some(TabMembership::Mode {
                name: "k8s-ops".into(),
            }),
            agent_type: None,
            seed: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Wire shape sanity: tagged enum with snake_case kind.
        assert_eq!(v["tab_membership"]["kind"], "mode");
        assert_eq!(v["tab_membership"]["name"], "k8s-ops");
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::StartAgent { tab_membership, .. } => {
                assert_eq!(
                    tab_membership,
                    Some(TabMembership::Mode {
                        name: "k8s-ops".into()
                    })
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn start_agent_with_orchestration_tab_membership_round_trip() {
        let req = AttachRequest::StartAgent {
            command: Some("claude".into()),
            cwd: Some("/work".into()),
            rows: 24,
            cols: 80,
            env: vec![],
            display_name: Some("coder".into()),
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
            seed: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["tab_membership"]["kind"], "orchestration");
        assert_eq!(v["tab_membership"]["name"], "tdd-cycle");
        assert_eq!(v["tab_membership"]["role_index"], 2);
        assert_eq!(v["tab_membership"]["role_name"], "coder");
        assert_eq!(v["tab_membership"]["is_start_role"], false);
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::StartAgent { tab_membership, .. } => {
                assert_eq!(
                    tab_membership,
                    Some(TabMembership::Orchestration {
                        name: "tdd-cycle".into(),
                        role_index: 2,
                        role_name: "coder".into(),
                        is_start_role: false,
                        orchestration_cwd: None,
                        display_title: None,
                        orchestration_id: None,
                    })
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_record_with_tab_membership_round_trip() {
        // PRD #76 M2.12: the daemon's echo via `list_agents` must serialize
        // tab_membership so the TUI can rebuild tabs on reconnect. Older
        // clients ignore the unknown field; older daemons omit it (None).
        let rec = AgentRecord {
            id: "7".into(),
            pane_id_env: Some("pid-7".into()),
            display_name: Some("coder".into()),
            cwd: Some("/work".into()),
            tab_membership: Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 1,
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
        let json = serde_json::to_string(&rec).unwrap();
        let back: AgentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tab_membership, rec.tab_membership);
    }

    #[test]
    fn agent_record_omits_tab_membership_when_none() {
        let rec = AgentRecord {
            id: "1".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
            live: None,
            spawned_at_ms: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("tab_membership"),
            "tab_membership=None should be omitted from the wire payload"
        );
        let back: AgentRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert!(back.tab_membership.is_none());
    }

    #[test]
    fn agent_record_without_tab_membership_field_deserializes() {
        // Forward compat: an older daemon that doesn't know about
        // tab_membership omits the field. A newer TUI must deserialize the
        // payload with `tab_membership: None` and treat the agent as a
        // dashboard pane on hydration.
        let json = r#"{"id":"1","display_name":"foo","cwd":"/tmp"}"#;
        let rec: AgentRecord = serde_json::from_str(json).unwrap();
        assert!(rec.tab_membership.is_none());
    }

    #[test]
    fn start_agent_deserializes_old_client_shape_without_tab_membership() {
        // M2.12 fixup auditor #4: explicit compat test using a
        // hand-crafted JSON literal in the *old* client shape — no
        // `tab_membership` field at all. A newer daemon must accept
        // the payload and decode `tab_membership: None`. Asserting via
        // round-trip of the current struct doesn't catch this: it'd
        // serialize the (`skip_serializing_if = None`) field as
        // absent, but only because our struct produces that shape.
        // This test pins the actual wire surface an older client
        // would send.
        let json = r#"{
            "op": "start-agent",
            "command": "/bin/sh",
            "cwd": "/tmp",
            "rows": 24,
            "cols": 80,
            "env": [],
            "display_name": "auditor"
        }"#;
        let req: AttachRequest = serde_json::from_str(json).unwrap();
        match req {
            AttachRequest::StartAgent {
                command,
                cwd,
                display_name,
                tab_membership,
                rows,
                cols,
                ..
            } => {
                assert_eq!(command.as_deref(), Some("/bin/sh"));
                assert_eq!(cwd.as_deref(), Some("/tmp"));
                assert_eq!(display_name.as_deref(), Some("auditor"));
                assert_eq!(rows, 24);
                assert_eq!(cols, 80);
                assert!(
                    tab_membership.is_none(),
                    "old-client payload without tab_membership must decode as None"
                );
            }
            _ => panic!("expected StartAgent variant, got {req:?}"),
        }
    }

    #[test]
    fn agent_record_deserializes_old_daemon_shape_without_tab_membership() {
        // M2.12 fixup auditor #4 (sibling case): hand-crafted JSON
        // literal in the *old* daemon shape — `AgentRecord` without a
        // `tab_membership` field. A newer TUI must accept the payload
        // and decode `tab_membership: None`, treating the agent as a
        // dashboard pane on hydration.
        let json = r#"{
            "id": "42",
            "pane_id_env": "pid-42",
            "display_name": "auditor",
            "cwd": "/work"
        }"#;
        let rec: AgentRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.id, "42");
        assert_eq!(rec.pane_id_env.as_deref(), Some("pid-42"));
        assert_eq!(rec.display_name.as_deref(), Some("auditor"));
        assert_eq!(rec.cwd.as_deref(), Some("/work"));
        assert!(
            rec.tab_membership.is_none(),
            "old-daemon record without tab_membership must decode as None"
        );
    }

    /// Scenario: Build a `SessionSnapshot` for every `SessionStatus` variant
    /// and round-trip it through JSON, asserting the status (and the agent
    /// type / active tool / tool count / prompts) survive; attach one to an
    /// `AgentRecord` and confirm `live` round-trips as `Some`; finally decode
    /// an older-daemon `AgentRecord` JSON that predates the `live` field and
    /// assert it deserializes with `live == None` (additive optional — no
    /// `PROTOCOL_VERSION` bump).
    #[spec("session/live/001")]
    #[test]
    fn live_001_session_snapshot_serde_and_agent_record_back_compat() {
        use crate::event::AgentType;
        use crate::state::{ActiveTool, SessionSnapshot, SessionStatus};

        // (a) Every SessionStatus variant survives a SessionSnapshot round-trip.
        for status in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::Thinking,
            SessionStatus::WaitingForInput,
            SessionStatus::Compacting,
            SessionStatus::Error,
        ] {
            let snap = SessionSnapshot {
                status: status.clone(),
                agent_type: Some(AgentType::ClaudeCode),
                active_tool: Some(ActiveTool {
                    name: "Read".into(),
                    detail: Some("src/main.rs".into()),
                }),
                tool_count: 3,
                first_prompts: vec!["build the feature".into()],
                last_user_prompt: Some("build the feature".into()),
                live_target: None,
                last_activity_ms: None,
            };
            let json = serde_json::to_string(&snap).expect("SessionSnapshot serializes");
            let back: SessionSnapshot =
                serde_json::from_str(&json).expect("SessionSnapshot deserializes");
            assert_eq!(back.status, status, "status must round-trip for {status:?}");
            assert_eq!(back.agent_type, Some(AgentType::ClaudeCode));
            assert_eq!(
                back.active_tool.as_ref().map(|t| t.name.as_str()),
                Some("Read"),
                "active tool name must round-trip"
            );
            assert_eq!(back.tool_count, 3);
            assert_eq!(back.first_prompts, vec!["build the feature".to_string()]);
            assert_eq!(back.last_user_prompt.as_deref(), Some("build the feature"));
        }

        // (b) An AgentRecord carrying a live snapshot round-trips with live == Some.
        let rec = AgentRecord {
            id: "9".into(),
            pane_id_env: Some("pane-9".into()),
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
            live: Some(SessionSnapshot {
                status: SessionStatus::Working,
                agent_type: Some(AgentType::ClaudeCode),
                active_tool: None,
                tool_count: 0,
                first_prompts: Vec::new(),
                last_user_prompt: None,
                live_target: None,
                last_activity_ms: None,
            }),
            spawned_at_ms: None,
        };
        let json = serde_json::to_string(&rec).expect("AgentRecord serializes");
        let back: AgentRecord = serde_json::from_str(&json).expect("AgentRecord deserializes");
        let live = back
            .live
            .expect("live snapshot must survive the AgentRecord round-trip");
        assert_eq!(live.status, SessionStatus::Working);
        assert_eq!(live.agent_type, Some(AgentType::ClaudeCode));

        // (c) Back-compat: an older daemon's AgentRecord JSON has no `live`
        // field at all. It must decode via `#[serde(default)]` with
        // live == None — additive optional, no PROTOCOL_VERSION bump.
        let legacy = r#"{
            "id": "42",
            "pane_id_env": "pid-42",
            "display_name": "auditor",
            "cwd": "/work"
        }"#;
        let old: AgentRecord = serde_json::from_str(legacy)
            .expect("older daemon shape must decode via #[serde(default)] on live");
        assert!(
            old.live.is_none(),
            "older AgentRecord without a live field must decode as None"
        );
    }

    /// Scenario: Build a live `SessionState` whose `last_activity` is an hour
    /// in the past, snapshot it, and assert the wire carries THAT instant as
    /// epoch milliseconds rather than anything resembling `now` — the honesty
    /// property that separates `last_activity` from the rejected session
    /// duration. Then serialize a snapshot with no activity time and confirm
    /// the key is absent from the JSON entirely, and decode an older peer's
    /// payload that predates the field and confirm it arrives as `None`
    /// (additive optional — no `PROTOCOL_VERSION` bump).
    #[spec("session/live/013")]
    #[test]
    fn live_013_last_activity_is_event_derived_and_additive() {
        use crate::event::AgentType;
        use crate::state::{SessionSnapshot, SessionState, SessionStatus};
        use std::collections::VecDeque;

        // (a) POPULATED, and populated from the session's OWN recorded instant.
        //
        // This is the check that decided the milestone. `SessionState.started_at`
        // was rejected as a column because the hydration path invents it as
        // `now`, so a duration resets under a restarted daemon and silently
        // lies about long-running work. `last_activity` does not have that
        // defect: `apply_event` sets it from the observed `AgentEvent.timestamp`
        // and only ever advances it, so an agent quiet for an hour snapshots as
        // quiet for an hour. Pinning an hour-old instant is what would fail if
        // anyone ever "helpfully" stamped this at snapshot time.
        let quiet_since = chrono::Utc::now() - chrono::Duration::hours(1);
        let session = SessionState {
            session_id: "sess-745".into(),
            agent_type: AgentType::ClaudeCode,
            cwd: None,
            status: SessionStatus::Idle,
            active_tool: None,
            started_at: quiet_since,
            last_activity: quiet_since,
            recent_events: VecDeque::new(),
            tool_count: 0,
            last_user_prompt: None,
            first_prompts: Vec::new(),
            pane_id: Some("pane-745".into()),
            agent_id: Some("agent-745".into()),
            display_name: None,
            shell_synthetic_working: false,
            orchestration_orphaned: false,
        };
        let snap = session.live_snapshot();
        assert_eq!(
            snap.last_activity_ms,
            Some(quiet_since.timestamp_millis()),
            "the snapshot must carry the session's own last_activity, not a \
             timestamp minted when the snapshot was taken"
        );

        // Round-trips as an exact integer: epoch milliseconds is the wire
        // representation precisely so there is no format to lose anything to.
        let json = serde_json::to_string(&snap).expect("SessionSnapshot serializes");
        let back: SessionSnapshot = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.last_activity_ms, snap.last_activity_ms);

        // (b) OMITTED when absent. `skip_serializing_if` keeps the key off the
        // wire, so a peer sees absence rather than a null it would have to
        // special-case — and the overview renders nothing at all for it.
        let mut absent = snap.clone();
        absent.last_activity_ms = None;
        let json = serde_json::to_string(&absent).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("is JSON");
        assert!(
            value.get("last_activity_ms").is_none(),
            "an absent activity time must have no key at all; got {json}"
        );

        // (c) FORWARD-COMPATIBLE with an older peer, which is the entire basis
        // of the do-not-bump decision — proven rather than asserted. An older
        // daemon's snapshot payload has no `last_activity_ms` key, and it must
        // decode via `#[serde(default)]` with every other field intact.
        let legacy = r#"{
            "status": "Working",
            "agent_type": "claude_code",
            "tool_count": 7
        }"#;
        let old: SessionSnapshot = serde_json::from_str(legacy)
            .expect("an older peer's snapshot must decode via #[serde(default)]");
        assert!(
            old.last_activity_ms.is_none(),
            "an older peer reports no activity time, which must read as absent"
        );
        assert_eq!(old.status, SessionStatus::Working);
        assert_eq!(old.tool_count, 7);

        // And the other direction: a NEWER peer's payload carrying the key must
        // not disturb the fields an older reader does understand.
        let newer = r#"{
            "status": "Idle",
            "tool_count": 0,
            "last_activity_ms": 1756684800123
        }"#;
        let forward: SessionSnapshot =
            serde_json::from_str(newer).expect("a newer peer's snapshot must decode");
        assert_eq!(forward.last_activity_ms, Some(1_756_684_800_123));
    }

    /// Scenario: Spawn a real agent through the registry, read it back out of
    /// `agent_records()`, and assert the wire carries the instant the daemon
    /// forked that child — bracketed by two `Utc::now()` readings taken either
    /// side of the spawn, so a value minted at snapshot time or copied from a
    /// session would fall outside. Then register an agent the registry did NOT
    /// spawn and confirm its record reports no spawn time at all and omits the
    /// key from the JSON entirely, and decode an older peer's `AgentRecord`
    /// payload that predates the field and confirm it arrives as `None` with
    /// every other field intact (additive optional — no `PROTOCOL_VERSION`
    /// bump).
    #[spec("session/live/014")]
    #[test]
    fn live_014_spawn_time_is_observed_and_additive() {
        use crate::agent_pty::{AgentPtyRegistry, AgentRecord, SpawnOptions};
        use portable_pty::{CommandBuilder, PtySize, PtySystem};

        // (a) RECORDED, and recorded as an OBSERVATION of our own fork.
        //
        // This is the property that made a duration shippable where the PRD had
        // rejected one. `SessionState.started_at` is event-derived — a session
        // exists only once a hook event has arrived, and the hydration path
        // invents it as `Utc::now()` when `pane_started_at` has no entry — so an
        // agent that has never emitted an event has no start instant at all.
        // A spawn is something the daemon DID, so it needs no signal and no
        // inference. Bracketing the spawn is what would fail if anyone ever
        // "helpfully" stamped this at snapshot time instead.
        let before = chrono::Utc::now().timestamp_millis();
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions::default())
            .expect("spawn should succeed");
        let after = chrono::Utc::now().timestamp_millis();

        let records = registry.agent_records();
        let rec = records.iter().find(|r| r.id == id).expect("agent missing");
        let spawned_at_ms = rec
            .spawned_at_ms
            .expect("the daemon forked this child, so it must report when");
        assert!(
            (before..=after).contains(&spawned_at_ms),
            "the spawn instant must lie inside the spawn call itself \
             ({before} ..= {after}), got {spawned_at_ms}"
        );

        // And it does not MOVE. The bracket above already rules out a value
        // minted at snapshot time, but only to the resolution of a millisecond
        // on a fast machine; reading the same record again a few milliseconds
        // later settles it outright — a `Utc::now()` anywhere on the read path
        // reports a different number here, whatever the clock's granularity.
        std::thread::sleep(Duration::from_millis(5));
        let again = registry.agent_records();
        let again = again.iter().find(|r| r.id == id).expect("agent missing");
        assert_eq!(
            again.spawned_at_ms,
            Some(spawned_at_ms),
            "the spawn instant is recorded once and read back, never recomputed"
        );

        // Round-trips as an exact integer: epoch milliseconds is the wire
        // representation precisely so there is no format to lose anything to.
        let json = serde_json::to_string(rec).expect("AgentRecord serializes");
        let back: AgentRecord = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.spawned_at_ms, Some(spawned_at_ms));
        registry.shutdown_all();

        // (b) ABSENT when this registry did not do the spawning, and omitted
        // from the wire entirely rather than sent as a null. There is no
        // `Utc::now()` fallback anywhere on this path — an invented value is
        // exactly the failure the PRD's original duration rejection was about.
        let adopted = Arc::new(AgentPtyRegistry::new());
        let pair = portable_pty::NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let child = pair
            .slave
            .spawn_command(CommandBuilder::new("sleep"))
            .expect("spawn a child the registry did not fork");
        let adopted_id = adopted.insert_test_agent(child);
        let adopted_records = adopted.agent_records();
        let adopted_rec = adopted_records
            .iter()
            .find(|r| r.id == adopted_id)
            .expect("adopted agent missing");
        assert!(
            adopted_rec.spawned_at_ms.is_none(),
            "a child this registry did not fork has no spawn time to report"
        );
        let json = serde_json::to_string(adopted_rec).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("is JSON");
        assert!(
            value.get("spawned_at_ms").is_none(),
            "an absent spawn time must have no key at all; got {json}"
        );
        adopted.shutdown_all();

        // (c) FORWARD-COMPATIBLE with an older peer, which is the entire basis
        // of the do-not-bump decision — proven rather than asserted. An older
        // daemon's `AgentRecord` has no `spawned_at_ms` key, and it must decode
        // via `#[serde(default)]` with every other field intact.
        let legacy = r#"{
            "id": "3",
            "pane_id_env": "pane-3",
            "display_name": "coder"
        }"#;
        let old: AgentRecord = serde_json::from_str(legacy)
            .expect("an older peer's record must decode via #[serde(default)]");
        assert!(
            old.spawned_at_ms.is_none(),
            "an older peer reports no spawn time, which must read as absent"
        );
        assert_eq!(old.id, "3");
        assert_eq!(old.pane_id_env.as_deref(), Some("pane-3"));
        assert_eq!(old.display_name.as_deref(), Some("coder"));

        // And the other direction: a NEWER peer's payload carrying the key must
        // not disturb the fields an older reader does understand.
        let newer = r#"{
            "id": "4",
            "pane_id_env": "pane-4",
            "spawned_at_ms": 1756684800123
        }"#;
        let forward: AgentRecord =
            serde_json::from_str(newer).expect("a newer peer's record must decode");
        assert_eq!(forward.spawned_at_ms, Some(1_756_684_800_123));
        assert_eq!(forward.pane_id_env.as_deref(), Some("pane-4"));
    }

    /// Scenario: A newer daemon advertises an `AgentRecord` whose `live.status`
    /// is a `SessionStatus` string this (older) build does not know
    /// (`"Hibernating"`). Because `live` is a present field, an unknown status
    /// must NOT fail the whole `AgentRecord` deserialization:
    /// `serde_json::from_str::<AgentRecord>` must return `Ok` and the record
    /// must survive with its `id` / `pane_id_env` intact and usable. Pairs with
    /// session/live/001 (older-shape -> `live == None` back-compat); this pins
    /// newer-shape forward-compat. Mechanism-agnostic: it does NOT pin whether
    /// the fix maps the unknown status to a catch-all variant (so `live` stays
    /// `Some`) or drops `live` to `None` — only that the parse succeeds and the
    /// record is retained.
    #[spec("session/live/009")]
    #[test]
    fn live_009_unknown_session_status_does_not_fail_agent_record_parse() {
        // A newer daemon adds a future SessionStatus variant; this older TUI has
        // no matching enum arm. `live` is a PRESENT field, so a strict enum
        // decode would fail the ENTIRE AgentRecord parse rather than degrading.
        let payload = r#"{
            "id": "fwd-compat-9",
            "pane_id_env": "pane-fwd-9",
            "live": {
                "status": "Hibernating",
                "tool_count": 0
            }
        }"#;

        let parsed = serde_json::from_str::<AgentRecord>(payload);
        assert!(
            parsed.is_ok(),
            "an unknown SessionStatus string must degrade gracefully, not fail \
             the whole AgentRecord parse (forward-compat); got {:?}",
            parsed.as_ref().err()
        );

        // The agent record itself must survive and stay usable — regardless of
        // whether the fix keeps `live = Some(<catch-all>)` or drops it to `None`.
        let rec = parsed.expect("unknown SessionStatus must parse Ok");
        assert_eq!(
            rec.id, "fwd-compat-9",
            "the AgentRecord must be retained through the unknown-status parse"
        );
        assert_eq!(rec.pane_id_env.as_deref(), Some("pane-fwd-9"));
    }

    #[test]
    fn set_agent_label_serde_round_trip() {
        let req = AttachRequest::SetAgentLabel {
            id: "7".into(),
            display_name: Some("coder".into()),
            cwd: Some("/tmp/work".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "set-agent-label");
        assert_eq!(v["id"], "7");
        assert_eq!(v["display_name"], "coder");
        assert_eq!(v["cwd"], "/tmp/work");
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::SetAgentLabel {
                id,
                display_name,
                cwd,
            } => {
                assert_eq!(id, "7");
                assert_eq!(display_name.as_deref(), Some("coder"));
                assert_eq!(cwd.as_deref(), Some("/tmp/work"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn resize_request_serde_round_trip() {
        // Wire shape must be `op = "resize"` (kebab-case) so existing
        // dispatcher matches the same way as start-agent / stop-agent.
        let req = AttachRequest::Resize {
            id: "agent-7".into(),
            rows: 50,
            cols: 200,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "resize");
        assert_eq!(v["id"], "agent-7");
        assert_eq!(v["rows"], 50);
        assert_eq!(v["cols"], 200);

        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::Resize { id, rows, cols } => {
                assert_eq!(id, "agent-7");
                assert_eq!(rows, 50);
                assert_eq!(cols, 200);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn subscribe_events_request_serde_round_trip() {
        // PRD #76 M2.17: SubscribeEvents has no payload fields, so the
        // wire shape is just `{"op": "subscribe-events"}`. Older daemons
        // would respond with `expected REQ frame, got kind 0x...` —
        // adding the variant doesn't break the existing dispatch.
        let req = AttachRequest::SubscribeEvents;
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "subscribe-events");
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AttachRequest::SubscribeEvents));
    }

    #[tokio::test]
    async fn kind_event_frame_round_trip() {
        // The KIND_EVENT payload is a JSON-encoded BroadcastMsg.
        // PRD #93 round-5: only the Event variant rides this channel
        // now — Delegate / WorkDone are dispatched directly into PTYs
        // by the daemon. Pin the on-wire shape so a future rename of
        // the enum tag or the variant name trips the build instead of
        // silently breaking remote-mode TUIs.
        use crate::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
        use chrono::Utc;
        use std::collections::HashMap;

        let event = AgentEvent {
            session_id: "sess-1".into(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            tool_name: Some("Read".into()),
            tool_detail: Some("src/main.rs".into()),
            cwd: Some("/work".into()),
            timestamp: Utc::now(),
            user_prompt: Some("fix the login bug".into()),
            metadata: HashMap::new(),
            pane_id: Some("7".into()),
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
        };
        let payload = serde_json::to_vec(&BroadcastMsg::Event(event)).unwrap();

        // Pin the on-wire JSON shape. A self-symmetric round-trip
        // would pass even if someone renamed `#[serde(tag = "kind")]`
        // to `tag = "type"` or renamed the `Event` variant rename
        // from `"event"`. Spell the contract out so a future
        // structural rename trips the test.
        //
        // The TUI's `apply_event` reads `tool_detail`, `cwd`,
        // `timestamp`, and `user_prompt` in addition to the discriminator
        // fields, so pin those too — a self-symmetric round-trip would
        // otherwise hide a rename or omission that breaks the remote UI.
        let wire: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(wire["kind"], "event");
        assert_eq!(wire["session_id"], "sess-1");
        assert_eq!(wire["agent_type"], "claude_code");
        assert_eq!(wire["event_type"], "tool_start");
        assert_eq!(wire["tool_name"], "Read");
        assert_eq!(wire["tool_detail"], "src/main.rs");
        assert_eq!(wire["cwd"], "/work");
        assert!(wire["timestamp"].is_string());
        assert_eq!(wire["user_prompt"], "fix the login bug");
        assert_eq!(wire["pane_id"], "7");

        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, KIND_EVENT, &payload).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, body) = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(kind, KIND_EVENT);
        let back: BroadcastMsg = serde_json::from_slice(&body).unwrap();
        let BroadcastMsg::Event(e) = back else {
            panic!("expected a BroadcastMsg::Event");
        };
        assert_eq!(e.session_id, "sess-1");
        assert_eq!(e.event_type, EventType::ToolStart);
        assert_eq!(e.tool_name.as_deref(), Some("Read"));
        assert_eq!(e.pane_id.as_deref(), Some("7"));
    }

    #[test]
    fn response_helpers() {
        let r = AttachResponse::ok();
        assert!(r.ok);
        assert!(r.error.is_none());

        let r = AttachResponse::err("nope");
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("nope"));

        let r = AttachResponse::agents(vec!["1".into(), "2".into()]);
        assert!(r.ok);
        assert_eq!(
            r.agents.as_deref(),
            Some(&["1".to_string(), "2".to_string()][..])
        );

        let r = AttachResponse::with_id("42".into());
        assert!(r.ok);
        assert_eq!(r.id.as_deref(), Some("42"));
    }

    #[test]
    fn hello_request_serde_round_trip() {
        // PRD #76 M2.21: pin the on-wire JSON shape so a future structural
        // change to the AttachRequest enum trips the test rather than
        // silently breaking the handshake. Mirrors the
        // `kind_event_frame_round_trip` precedent.
        let req = AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            client_build_version: Some(env!("DAD_BUILD_ID").to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "hello");
        assert_eq!(v["client_version"], PROTOCOL_VERSION);
        assert_eq!(v["client_build_version"], env!("DAD_BUILD_ID"));

        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::Hello {
                client_version,
                client_build_version,
            } => {
                assert_eq!(client_version, PROTOCOL_VERSION);
                assert_eq!(client_build_version.as_deref(), Some(env!("DAD_BUILD_ID")));
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn hello_request_omits_client_build_version_when_none() {
        // PRD #103 M1.2: when a (legacy) client doesn't populate
        // `client_build_version`, the wire payload must not carry the
        // field. Older daemons would reject any unknown key as a strictness
        // failure (they don't, but the contract holds anyway).
        let req = AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            client_build_version: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("client_build_version"),
            "client_build_version=None must be omitted from the wire payload"
        );
    }

    #[test]
    fn hello_request_deserializes_legacy_shape_without_client_build_version() {
        // PRD #103 M1.2: a pre-PRD-103 client emits only `client_version`.
        // The daemon side must accept the payload and decode
        // `client_build_version` as None — `#[serde(default)]` makes this
        // work, but the test pins the wire contract.
        let json = r#"{"op":"hello","client_version":2}"#;
        let req: AttachRequest = serde_json::from_str(json).unwrap();
        match req {
            AttachRequest::Hello {
                client_version,
                client_build_version,
            } => {
                assert_eq!(client_version, 2);
                assert!(client_build_version.is_none());
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn hello_response_serde_round_trip() {
        let resp = AttachResponse::hello(PROTOCOL_VERSION);
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["server_version"], PROTOCOL_VERSION);
        // PRD #103 M1.1: hello() must populate `build_version` from the
        // daemon's compiled-in DAD_BUILD_ID so the laptop can detect
        // handler-code skew. The exact value is build-time-derived; we just
        // require it's present and non-empty here.
        let wire_build_version = v["build_version"]
            .as_str()
            .expect("hello() must emit build_version on the wire");
        assert!(
            !wire_build_version.is_empty(),
            "build_version must be non-empty"
        );
        assert_eq!(wire_build_version, env!("DAD_BUILD_ID"));

        let back: AttachResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert_eq!(back.server_version, Some(PROTOCOL_VERSION));
        assert_eq!(back.build_version.as_deref(), Some(env!("DAD_BUILD_ID")));
    }

    #[test]
    fn response_omits_build_version_when_none() {
        // PRD #103 M1.1: forward compat. An unrelated response (e.g.
        // list-agents) must NOT carry `build_version` on the wire — older
        // peers ignore the field, newer peers treat its absence on a hello
        // reply as "incompatible / recycle the daemon".
        let resp = AttachResponse::ok();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("build_version"),
            "build_version=None should be omitted from the wire payload"
        );
    }

    #[test]
    fn response_deserializes_legacy_shape_without_build_version() {
        // PRD #103 M1.1: a pre-PRD-103 daemon emits `server_version` but
        // not `build_version`. The newer client must accept the payload
        // and decode the field as None — which is what the mismatch logic
        // uses to flag "daemon too old / recycle it".
        let json = r#"{"ok":true,"server_version":2}"#;
        let resp: AttachResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.server_version, Some(2));
        assert!(resp.build_version.is_none());
    }

    #[test]
    fn response_omits_server_version_when_none() {
        // Forward compat: an unrelated response (e.g. list-agents) must NOT
        // carry `server_version` on the wire. Pre-M2.21 clients/daemons
        // ignore the field; newer clients use its absence as the signal to
        // treat the peer as protocol-too-old.
        let resp = AttachResponse::ok();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("server_version"),
            "server_version=None should be omitted from the wire payload"
        );
    }

    #[test]
    fn response_deserializes_legacy_shape_without_server_version() {
        // A pre-M2.21 daemon never emits `server_version`. A newer client
        // must accept the payload and decode the field as None — which is
        // what the protocol-mismatch logic looks for to detect "remote too
        // old to know about the handshake".
        let json = r#"{"ok":true}"#;
        let resp: AttachResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.server_version.is_none());
    }
}
